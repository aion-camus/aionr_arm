/*******************************************************************************
 * Copyright (c) 2015-2018 Parity Technologies (UK) Ltd.
 * Copyright (c) 2018-2019 Aion foundation.
 *
 *     This file is part of the aion network project.
 *
 *     The aion network project is free software: you can redistribute it
 *     and/or modify it under the terms of the GNU General Public License
 *     as published by the Free Software Foundation, either version 3 of
 *     the License, or any later version.
 *
 *     The aion network project is distributed in the hope that it will
 *     be useful, but WITHOUT ANY WARRANTY; without even the implied
 *     warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
 *     See the GNU General Public License for more details.
 *
 *     You should have received a copy of the GNU General Public License
 *     along with the aion network project source files.
 *     If not, see <https://www.gnu.org/licenses/>.
 *
 ******************************************************************************/

//! Test client.

use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrder};
use std::sync::Arc;
use std::collections::{HashMap, BTreeMap};
use std::mem;
use std::time::Duration;
use itertools::Itertools;
use rustc_hex::FromHex;
use blake2b::blake2b;
use aion_types::{H256, H128, U256, Address};
use parking_lot::RwLock;
use journaldb;
use kvdb::DBValue;
use kvdb::{RepositoryConfig, DatabaseConfig, DbRepository};
use bytes::Bytes;
use rlp::*;
use key::{generate_keypair, public_to_address_ed25519};
use tempdir::TempDir;
use transaction::{
    self, Transaction, LocalizedTransaction, PendingTransaction, SignedTransaction, Action,
    DEFAULT_TRANSACTION_TYPE,
};
use blockchain::{TreeRoute, BlockReceipts};
use client::{
    BlockChainClient, MiningBlockChainClient, BlockChainInfo, BlockStatus, BlockId, TransactionId,
    LastHashes, CallAnalytics, BlockImportError, ProvingBlockChainClient,
};
use db::{COL_STATE, DB_NAMES};
use header::{Header as BlockHeader, BlockNumber};
use filter::Filter;
use log_entry::LocalizedLogEntry;
use receipt::{Receipt, LocalizedReceipt};
use error::ImportResult;
use factory::VmFactory;
use miner::{Miner, MinerService};
use spec::Spec;
use types::basic_account::BasicAccount;
use types::pruning_info::PruningInfo;

use verification::queue::QueueInfo;
use block::{OpenBlock, SealedBlock, ClosedBlock};
use executive::Executed;
use error::CallError;
use state_db::StateDB;
use encoded;
use kvdb::{KeyValueDB, MemoryDBRepository};

use super::super::transaction::UnverifiedTransaction;

/// Test client.
pub struct TestBlockChainClient {
    /// Blocks.
    pub blocks: RwLock<HashMap<H256, Bytes>>,
    /// Mapping of numbers to hashes.
    pub numbers: RwLock<HashMap<usize, H256>>,
    /// Genesis block hash.
    pub genesis_hash: H256,
    /// Last block hash.
    pub last_hash: RwLock<H256>,
    /// Extra data do set for each block
    pub extra_data: Bytes,
    /// Difficulty.
    pub difficulty: RwLock<U256>,
    /// Balances.
    pub balances: RwLock<HashMap<Address, U256>>,
    /// Nonces.
    pub nonces: RwLock<HashMap<Address, U256>>,
    /// Storage.
    pub storage: RwLock<HashMap<(Address, H128), H128>>,
    /// Code.
    pub code: RwLock<HashMap<Address, Bytes>>,
    /// Execution result.
    pub execution_result: RwLock<Option<Result<Executed, CallError>>>,
    /// Transaction receipts.
    pub receipts: RwLock<HashMap<TransactionId, LocalizedReceipt>>,
    /// Logs
    pub logs: RwLock<Vec<LocalizedLogEntry>>,
    /// Block queue size.
    pub queue_size: AtomicUsize,
    /// Miner
    pub miner: Arc<Miner>,
    /// Spec
    pub spec: Spec,
    /// VM Factory
    pub vm_factory: VmFactory,
    /// Timestamp assigned to latest sealed block
    pub latest_block_timestamp: RwLock<u64>,
    /// Ancient block info.
    pub ancient_block: RwLock<Option<(H256, u64)>>,
    /// First block info.
    pub first_block: RwLock<Option<(H256, u64)>>,
    /// Pruning history size to report.
    pub history: RwLock<Option<u64>>,
    // db
    pub db: Arc<KeyValueDB>,
}

/// Used for generating test client blocks.
#[derive(Clone)]
pub enum EachBlockWith {
    /// Plain block.
    Nothing,
    /// Block with an uncle.
    Uncle,
    /// Block with a transaction.
    Transaction,
    /// Block with an uncle and transaction.
    UncleAndTransaction,
}

impl TestBlockChainClient {
    /// Create test client with custom spec.
    pub fn new_with_spec(spec: Spec) -> Self {
        TestBlockChainClient::new_with_spec_and_extra(spec, Bytes::new())
    }

    /// Create test client with custom spec and extra data.
    pub fn new_with_spec_and_extra(spec: Spec, extra_data: Bytes) -> Self {
        let genesis_block = spec.genesis_block();
        let genesis_hash = spec.genesis_header().hash();

        let mut client = TestBlockChainClient {
            blocks: RwLock::new(HashMap::new()),
            numbers: RwLock::new(HashMap::new()),
            genesis_hash: H256::new(),
            extra_data: extra_data,
            last_hash: RwLock::new(H256::new()),
            difficulty: RwLock::new(spec.genesis_header().difficulty().clone()),
            balances: RwLock::new(HashMap::new()),
            nonces: RwLock::new(HashMap::new()),
            storage: RwLock::new(HashMap::new()),
            code: RwLock::new(HashMap::new()),
            execution_result: RwLock::new(None),
            receipts: RwLock::new(HashMap::new()),
            logs: RwLock::new(Vec::new()),
            queue_size: AtomicUsize::new(0),
            miner: Arc::new(Miner::with_spec(&spec)),
            spec: spec,
            vm_factory: VmFactory::new(),
            latest_block_timestamp: RwLock::new(10_000_000),
            ancient_block: RwLock::new(None),
            first_block: RwLock::new(None),
            history: RwLock::new(None),
            db: Arc::new(MemoryDBRepository::new()),
        };

        // insert genesis hash.
        client.blocks.get_mut().insert(genesis_hash, genesis_block);
        client.numbers.get_mut().insert(0, genesis_hash);
        *client.last_hash.get_mut() = genesis_hash;
        client.genesis_hash = genesis_hash;
        client
    }

    /// Set the transaction receipt result
    pub fn set_transaction_receipt(&self, id: TransactionId, receipt: LocalizedReceipt) {
        self.receipts.write().insert(id, receipt);
    }

    /// Set the execution result.
    pub fn set_execution_result(&self, result: Result<Executed, CallError>) {
        *self.execution_result.write() = Some(result);
    }

    /// Set the balance of account `address` to `balance`.
    pub fn set_balance(&self, address: Address, balance: U256) {
        self.balances.write().insert(address, balance);
    }

    /// Set nonce of account `address` to `nonce`.
    pub fn set_nonce(&self, address: Address, nonce: U256) {
        self.nonces.write().insert(address, nonce);
    }

    /// Set `code` at `address`.
    pub fn set_code(&self, address: Address, code: Bytes) {
        self.code.write().insert(address, code);
    }

    /// Set storage `position` to `value` for account `address`.
    pub fn set_storage(&self, address: Address, position: H128, value: H128) {
        self.storage.write().insert((address, position), value);
    }

    /// Set block queue size for testing
    pub fn set_queue_size(&self, size: usize) { self.queue_size.store(size, AtomicOrder::Relaxed); }

    /// Set timestamp assigned to latest sealed block
    pub fn set_latest_block_timestamp(&self, ts: u64) { *self.latest_block_timestamp.write() = ts; }

    /// Set logs to return for each logs call.
    pub fn set_logs(&self, logs: Vec<LocalizedLogEntry>) { *self.logs.write() = logs; }

    /// Add blocks to test client.
    pub fn add_blocks(&self, count: usize, with: EachBlockWith) {
        let len = self.numbers.read().len();
        for n in len..(len + count) {
            let mut header = BlockHeader::new();
            header.set_difficulty(From::from(n));
            header.set_parent_hash(self.last_hash.read().clone());
            header.set_number(n as BlockNumber);
            header.set_gas_limit(U256::from(1_000_000));
            header.set_extra_data(self.extra_data.clone());
            let txs = match with {
                EachBlockWith::Transaction | EachBlockWith::UncleAndTransaction => {
                    let mut txs = RlpStream::new_list(1);
                    let keypair = generate_keypair();
                    // Update nonces value
                    self.nonces
                        .write()
                        .insert(public_to_address_ed25519(&keypair.public()), U256::one());
                    let tx = Transaction {
                        action: Action::Create,
                        value: U256::from(100),
                        value_bytes: Vec::new(),
                        data: "3331600055".from_hex().unwrap(),
                        gas: U256::from(100_000),
                        gas_bytes: Vec::new(),
                        gas_price: U256::from(200_000_000_000u64),
                        gas_price_bytes: Vec::new(),
                        nonce: U256::zero(),
                        nonce_bytes: Vec::new(),
                        transaction_type: DEFAULT_TRANSACTION_TYPE,
                    };
                    let signed_tx = tx.sign(&keypair.secret().0, None);
                    txs.append(&signed_tx);
                    txs.out()
                }
                _ => ::rlp::EMPTY_LIST_RLP.to_vec(),
            };

            let mut rlp = RlpStream::new_list(2);
            rlp.append(&header);
            rlp.append_raw(&txs, 1);
            self.import_block(rlp.as_raw().to_vec()).unwrap();
        }
    }

    /// Make a bad block by setting invalid extra data.
    pub fn corrupt_block(&self, n: BlockNumber) {
        let hash = self.block_hash(BlockId::Number(n)).unwrap();
        let mut header: BlockHeader = self.block_header(BlockId::Number(n)).unwrap().decode();
        header.set_extra_data(b"This extra data is way too long to be considered valid".to_vec());
        let mut rlp = RlpStream::new_list(3);
        rlp.append(&header);
        rlp.append_raw(&::rlp::NULL_RLP, 1);
        rlp.append_raw(&::rlp::NULL_RLP, 1);
        self.blocks.write().insert(hash, rlp.out());
    }

    /// Make a bad block by setting invalid parent hash.
    pub fn corrupt_block_parent(&self, n: BlockNumber) {
        let hash = self.block_hash(BlockId::Number(n)).unwrap();
        let mut header: BlockHeader = self.block_header(BlockId::Number(n)).unwrap().decode();
        header.set_parent_hash(H256::from(42));
        let mut rlp = RlpStream::new_list(3);
        rlp.append(&header);
        rlp.append_raw(&::rlp::NULL_RLP, 1);
        rlp.append_raw(&::rlp::NULL_RLP, 1);
        self.blocks.write().insert(hash, rlp.out());
    }

    /// TODO:
    pub fn block_hash_delta_minus(&mut self, delta: usize) -> H256 {
        let blocks_read = self.numbers.read();
        let index = blocks_read.len() - delta;
        blocks_read[&index].clone()
    }

    fn block_hash(&self, id: BlockId) -> Option<H256> {
        match id {
            BlockId::Hash(hash) => Some(hash),
            BlockId::Number(n) => self.numbers.read().get(&(n as usize)).cloned(),
            BlockId::Earliest => self.numbers.read().get(&0).cloned(),
            BlockId::Latest | BlockId::Pending => {
                self.numbers
                    .read()
                    .get(&(self.numbers.read().len() - 1))
                    .cloned()
            }
        }
    }

    /// Inserts a transaction with given gas price to miners transactions queue.
    pub fn insert_transaction_with_gas_price_to_queue(&self, gas_price: U256) -> H256 {
        let keypair = generate_keypair();
        let tx = Transaction {
            action: Action::Create,
            value: U256::from(100),
            value_bytes: Vec::new(),
            data: "3331600055".from_hex().unwrap(),
            gas: U256::from(100_000),
            gas_bytes: Vec::new(),
            gas_price: gas_price,
            gas_price_bytes: Vec::new(),
            nonce: U256::zero(),
            nonce_bytes: Vec::new(),
            transaction_type: DEFAULT_TRANSACTION_TYPE.into(),
        };
        let signed_tx = tx.sign(&keypair.secret().0, None);
        self.set_balance(signed_tx.sender(), 10_000_000_000_000_000_000u64.into());
        let hash = signed_tx.hash();
        let res = self
            .miner
            .import_external_transactions(self, vec![signed_tx.into()]);
        let res = res.into_iter().next().unwrap().expect("Successful import");
        assert_eq!(res, transaction::ImportResult::Current);
        hash
    }

    /// Inserts a transaction to miners transactions queue.
    pub fn insert_transaction_to_queue(&self) -> H256 {
        self.insert_transaction_with_gas_price_to_queue(U256::from(20_000_000_000u64))
    }

    /// Set reported history size.
    pub fn set_history(&self, h: Option<u64>) { *self.history.write() = h; }
}

pub fn get_temp_state_db() -> (StateDB, TempDir) {
    let tempdir = TempDir::new("").unwrap();
    let db_config = DatabaseConfig::default();
    let mut db_configs = Vec::new();
    for db_name in DB_NAMES.to_vec() {
        db_configs.push(RepositoryConfig {
            db_name: db_name.into(),
            db_config: db_config.clone(),
            db_path: tempdir.path().join(db_name).to_str().unwrap().to_string(),
        });
    }
    let dbs = DbRepository::init(db_configs).unwrap();
    let dbs = Arc::new(dbs);
    let journal_db = journaldb::new(dbs, journaldb::Algorithm::OverlayRecent, COL_STATE);
    let state_db = StateDB::new(journal_db, 1024 * 1024);
    (state_db, tempdir)
}

impl MiningBlockChainClient for TestBlockChainClient {
    fn as_block_chain_client(&self) -> &BlockChainClient { self }

    fn prepare_open_block(
        &self,
        author: Address,
        gas_range_target: (U256, U256),
        extra_data: Bytes,
    ) -> OpenBlock
    {
        let engine = &*self.spec.engine;
        let genesis_header = self.spec.genesis_header();
        let (state_db, _tempdir) = get_temp_state_db();
        let db = self
            .spec
            .ensure_db_good(state_db, &Default::default())
            .unwrap();

        let last_hashes = vec![genesis_header.hash()];
        let mut open_block = OpenBlock::new(
            engine,
            Default::default(),
            db,
            &genesis_header,
            None,
            Arc::new(last_hashes),
            author,
            gas_range_target,
            extra_data,
            false,
            self.db.clone(),
        )
        .expect("Opening block for tests will not fail.");
        // TODO [todr] Override timestamp for predictability (set_timestamp_now kind of sucks)
        open_block.set_timestamp(*self.latest_block_timestamp.read());
        open_block
    }

    fn reopen_block(&self, block: ClosedBlock) -> OpenBlock { block.reopen(&*self.spec.engine) }

    fn vm_factory(&self) -> &VmFactory { &self.vm_factory }

    fn import_sealed_block(&self, _block: SealedBlock) -> ImportResult { Ok(H256::default()) }

    fn broadcast_transaction(&self, _transactions: Bytes) {}

    fn broadcast_proposal_block(&self, _block: SealedBlock) {}

    fn prepare_block_interval(&self) -> Duration { Duration::default() }
}

impl BlockChainClient for TestBlockChainClient {
    fn call(
        &self,
        _t: &SignedTransaction,
        _analytics: CallAnalytics,
        _block: BlockId,
    ) -> Result<Executed, CallError>
    {
        self.execution_result.read().clone().unwrap()
    }

    fn call_many(
        &self,
        txs: &[(SignedTransaction, CallAnalytics)],
        block: BlockId,
    ) -> Result<Vec<Executed>, CallError>
    {
        let mut res = Vec::with_capacity(txs.len());
        for &(ref tx, analytics) in txs {
            res.push(self.call(tx, analytics, block)?);
        }
        Ok(res)
    }

    fn estimate_gas(&self, _t: &SignedTransaction, _block: BlockId) -> Result<U256, CallError> {
        Ok(21000.into())
    }

    fn replay(&self, _id: TransactionId, _analytics: CallAnalytics) -> Result<Executed, CallError> {
        self.execution_result.read().clone().unwrap()
    }

    fn replay_block_transactions(
        &self,
        _block: BlockId,
        _analytics: CallAnalytics,
    ) -> Result<Box<Iterator<Item = Executed>>, CallError>
    {
        Ok(Box::new(
            self.execution_result.read().clone().unwrap().into_iter(),
        ))
    }

    fn block_total_difficulty(&self, _id: BlockId) -> Option<U256> { Some(U256::zero()) }

    fn block_hash(&self, id: BlockId) -> Option<H256> { Self::block_hash(self, id) }

    fn nonce(&self, address: &Address, id: BlockId) -> Option<U256> {
        match id {
            BlockId::Latest | BlockId::Pending => {
                Some(
                    self.nonces
                        .read()
                        .get(address)
                        .cloned()
                        .unwrap_or(U256::zero()),
                )
            }
            _ => None,
        }
    }

    fn storage_root(&self, _address: &Address, _id: BlockId) -> Option<H256> { None }

    fn latest_nonce(&self, address: &Address) -> U256 {
        self.nonce(address, BlockId::Latest).unwrap()
    }

    fn code(&self, address: &Address, id: BlockId) -> Option<Option<Bytes>> {
        match id {
            BlockId::Latest | BlockId::Pending => Some(self.code.read().get(address).cloned()),
            _ => None,
        }
    }

    fn code_hash(&self, address: &Address, id: BlockId) -> Option<H256> {
        match id {
            BlockId::Latest | BlockId::Pending => {
                self.code.read().get(address).map(|c| blake2b(&c))
            }
            _ => None,
        }
    }

    fn balance(&self, address: &Address, id: BlockId) -> Option<U256> {
        match id {
            BlockId::Latest | BlockId::Pending => {
                Some(
                    self.balances
                        .read()
                        .get(address)
                        .cloned()
                        .unwrap_or_else(U256::zero),
                )
            }
            _ => None,
        }
    }

    fn latest_balance(&self, address: &Address) -> U256 {
        self.balance(address, BlockId::Latest).unwrap()
    }

    fn storage_at(&self, address: &Address, position: &H128, id: BlockId) -> Option<H128> {
        match id {
            BlockId::Latest | BlockId::Pending => {
                Some(
                    self.storage
                        .read()
                        .get(&(address.clone(), position.clone()))
                        .cloned()
                        .unwrap_or_else(H128::new),
                )
            }
            _ => None,
        }
    }

    fn list_accounts(
        &self,
        _id: BlockId,
        _after: Option<&Address>,
        _count: u64,
    ) -> Option<Vec<Address>>
    {
        None
    }

    fn list_storage(
        &self,
        _id: BlockId,
        _account: &Address,
        _after: Option<&H128>,
        _count: u64,
    ) -> Option<Vec<H128>>
    {
        None
    }
    fn transaction(&self, _id: TransactionId) -> Option<LocalizedTransaction> {
        None // Simple default.
    }

    fn transaction_block(&self, _id: TransactionId) -> Option<H256> {
        None // Simple default.
    }

    fn transaction_receipt(&self, id: TransactionId) -> Option<LocalizedReceipt> {
        self.receipts.read().get(&id).cloned()
    }

    fn logs(&self, filter: Filter) -> Vec<LocalizedLogEntry> {
        let mut logs = self.logs.read().clone();
        let len = logs.len();
        match filter.limit {
            Some(limit) if limit <= len => logs.split_off(len - limit),
            _ => logs,
        }
    }

    fn last_hashes(&self) -> LastHashes {
        unimplemented!();
    }

    fn best_block_header(&self) -> encoded::Header {
        self.block_header(BlockId::Hash(self.chain_info().best_block_hash))
            .expect("Best block always has header.")
    }

    fn block_header(&self, id: BlockId) -> Option<encoded::Header> {
        self.block_hash(id)
            .and_then(|hash| {
                self.blocks
                    .read()
                    .get(&hash)
                    .map(|r| Rlp::new(r).at(0).as_raw().to_vec())
            })
            .map(encoded::Header::new)
    }

    fn block_number(&self, _id: BlockId) -> Option<BlockNumber> { unimplemented!() }

    fn block_body(&self, id: BlockId) -> Option<encoded::Body> {
        self.block_hash(id).and_then(|hash| {
            self.blocks.read().get(&hash).map(|r| {
                let mut stream = RlpStream::new_list(2);
                stream.append_raw(Rlp::new(r).at(1).as_raw(), 1);
                stream.append_raw(Rlp::new(r).at(2).as_raw(), 1);
                encoded::Body::new(stream.out())
            })
        })
    }

    fn block(&self, id: BlockId) -> Option<encoded::Block> {
        self.block_hash(id)
            .and_then(|hash| self.blocks.read().get(&hash).cloned())
            .map(encoded::Block::new)
    }

    fn block_extra_info(&self, id: BlockId) -> Option<BTreeMap<String, String>> {
        self.block(id)
            .map(|block| block.view().header())
            .map(|header| self.spec.engine.extra_info(&header))
    }

    fn block_status(&self, id: BlockId) -> BlockStatus {
        match id {
            BlockId::Number(number) if (number as usize) < self.blocks.read().len() => {
                BlockStatus::InChain
            }
            BlockId::Hash(ref hash) if self.blocks.read().get(hash).is_some() => {
                BlockStatus::InChain
            }
            BlockId::Latest | BlockId::Earliest => BlockStatus::InChain,
            BlockId::Pending => BlockStatus::Pending,
            _ => BlockStatus::Unknown,
        }
    }

    // works only if blocks are one after another 1 -> 2 -> 3
    fn tree_route(&self, from: &H256, to: &H256) -> Option<TreeRoute> {
        Some(TreeRoute {
            ancestor: H256::new(),
            index: 0,
            blocks: {
                let numbers_read = self.numbers.read();
                let mut adding = false;

                let mut blocks = Vec::new();
                for (_, hash) in numbers_read
                    .iter()
                    .sorted_by(|tuple1, tuple2| tuple1.0.cmp(tuple2.0))
                {
                    if hash == to {
                        if adding {
                            blocks.push(hash.clone());
                        }
                        adding = false;
                        break;
                    }
                    if hash == from {
                        adding = true;
                    }
                    if adding {
                        blocks.push(hash.clone());
                    }
                }
                if adding {
                    Vec::new()
                } else {
                    blocks
                }
            },
        })
    }

    // TODO: returns just hashes instead of node state rlp(?)
    fn state_data(&self, hash: &H256) -> Option<Bytes> {
        // starts with 'f' ?
        if *hash > H256::from("f000000000000000000000000000000000000000000000000000000000000000") {
            let mut rlp = RlpStream::new();
            rlp.append(&hash.clone());
            return Some(rlp.out());
        }
        None
    }

    fn block_receipts(&self, hash: &H256) -> Option<Bytes> {
        // starts with 'f' ?
        if *hash > H256::from("f000000000000000000000000000000000000000000000000000000000000000") {
            let receipt = BlockReceipts::new(vec![Receipt::new(
                H256::zero(),
                U256::zero(),
                U256::zero(),
                vec![],
                Bytes::default(),
                String::default(),
            )]);
            let mut rlp = RlpStream::new();
            rlp.append(&receipt);
            return Some(rlp.out());
        }
        None
    }

    fn import_block(&self, b: Bytes) -> Result<H256, BlockImportError> {
        let header = Rlp::new(&b).val_at::<BlockHeader>(0);
        let h = header.hash();
        let number: usize = header.number() as usize;
        if number > self.blocks.read().len() {
            panic!(
                "Unexpected block number. Expected {}, got {}",
                self.blocks.read().len(),
                number
            );
        }
        if number > 0 {
            match self.blocks.read().get(header.parent_hash()) {
                Some(parent) => {
                    let parent = Rlp::new(parent).val_at::<BlockHeader>(0);
                    if parent.number() != (header.number() - 1) {
                        panic!("Unexpected block parent");
                    }
                }
                None => {
                    panic!(
                        "Unknown block parent {:?} for block {}",
                        header.parent_hash(),
                        number
                    );
                }
            }
        }
        let len = self.numbers.read().len();
        if number == len {
            {
                let mut difficulty = self.difficulty.write();
                *difficulty = *difficulty + header.difficulty().clone();
            }
            mem::replace(&mut *self.last_hash.write(), h.clone());
            self.blocks.write().insert(h.clone(), b);
            self.numbers.write().insert(number, h.clone());
            let mut parent_hash = header.parent_hash().clone();
            if number > 0 {
                let mut n = number - 1;
                while n > 0 && self.numbers.read()[&n] != parent_hash {
                    *self.numbers.write().get_mut(&n).unwrap() = parent_hash.clone();
                    n -= 1;
                    parent_hash = Rlp::new(&self.blocks.read()[&parent_hash])
                        .val_at::<BlockHeader>(0)
                        .parent_hash()
                        .clone();
                }
            }
        } else {
            self.blocks.write().insert(h.clone(), b.to_vec());
        }
        Ok(h)
    }

    fn import_block_with_receipts(&self, b: Bytes, _r: Bytes) -> Result<H256, BlockImportError> {
        self.import_block(b)
    }

    fn queue_info(&self) -> QueueInfo {
        QueueInfo {
            verified_queue_size: self.queue_size.load(AtomicOrder::Relaxed),
            unverified_queue_size: 0,
            verifying_queue_size: 0,
            max_queue_size: 0,
            max_mem_use: 0,
            mem_used: 0,
        }
    }

    fn clear_queue(&self) {}

    fn clear_bad(&self) {}

    fn additional_params(&self) -> BTreeMap<String, String> { Default::default() }

    fn chain_info(&self) -> BlockChainInfo {
        let number = self.blocks.read().len() as BlockNumber - 1;
        BlockChainInfo {
            total_difficulty: *self.difficulty.read(),
            pending_total_difficulty: *self.difficulty.read(),
            genesis_hash: self.genesis_hash.clone(),
            best_block_hash: self.last_hash.read().clone(),
            best_block_number: number,
            best_block_timestamp: number,
            first_block_hash: self.first_block.read().as_ref().map(|x| x.0),
            first_block_number: self.first_block.read().as_ref().map(|x| x.1),
            ancient_block_hash: self.ancient_block.read().as_ref().map(|x| x.0),
            ancient_block_number: self.ancient_block.read().as_ref().map(|x| x.1),
        }
    }

    fn import_queued_transactions(&self, _transactions: Vec<UnverifiedTransaction>) {}

    fn queue_consensus_message(&self, message: Bytes) {
        self.spec.engine.handle_message(&message).unwrap();
    }

    fn ready_transactions(&self) -> Vec<PendingTransaction> {
        let info = self.chain_info();
        self.miner
            .ready_transactions(info.best_block_number, info.best_block_timestamp)
    }

    fn spec_name(&self) -> String { "foundation".into() }

    fn disable(&self) {
        unimplemented!();
    }

    fn pruning_info(&self) -> PruningInfo {
        let best_num = self.chain_info().best_block_number;
        PruningInfo {
            earliest_chain: 1,
            earliest_state: self
                .history
                .read()
                .as_ref()
                .map(|x| best_num - x)
                .unwrap_or(0),
        }
    }

    fn call_contract(
        &self,
        _id: BlockId,
        _address: Address,
        _data: Bytes,
    ) -> Result<Bytes, String>
    {
        Ok(vec![])
    }

    fn registrar_address(&self) -> Option<Address> { None }

    fn registry_address(&self, _name: String, _block: BlockId) -> Option<Address> { None }
}

impl ProvingBlockChainClient for TestBlockChainClient {
    fn prove_storage(&self, _: H256, _: H256, _: BlockId) -> Option<(Vec<Bytes>, H256)> { None }

    fn prove_account(&self, _: H256, _: BlockId) -> Option<(Vec<Bytes>, BasicAccount)> { None }

    fn prove_transaction(&self, _: SignedTransaction, _: BlockId) -> Option<(Bytes, Vec<DBValue>)> {
        None
    }

    fn epoch_signal(&self, _: H256) -> Option<Vec<u8>> { None }
}

impl super::traits::EngineClient for TestBlockChainClient {
    fn update_sealing(&self) { self.miner.update_sealing(self) }

    fn submit_seal(&self, block_hash: H256, seal: Vec<Bytes>) {
        if self.miner.submit_seal(self, block_hash, seal).is_err() {
            warn!(target: "poa", "Wrong internal seal submission!")
        }
    }

    fn broadcast_consensus_message(&self, _message: Bytes) {}

    fn epoch_transition_for(&self, _block_hash: H256) -> Option<::engines::EpochTransition> { None }

    fn chain_info(&self) -> BlockChainInfo { BlockChainClient::chain_info(self) }

    fn as_full_client(&self) -> Option<&BlockChainClient> { Some(self) }

    fn block_number(&self, id: BlockId) -> Option<BlockNumber> {
        BlockChainClient::block_number(self, id)
    }

    fn block_header(&self, id: BlockId) -> Option<::encoded::Header> {
        BlockChainClient::block_header(self, id)
    }
}
