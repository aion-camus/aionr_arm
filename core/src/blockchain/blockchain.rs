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

//! Blockchain database.

use std::collections::{HashMap, hash_map};
use std::sync::Arc;
use std::mem;
use itertools::Itertools;
use bloomchain as bc;
use heapsize::HeapSizeOf;
use aion_types::{H256, U256};
use ethbloom::Bloom;
use parking_lot::{Mutex, RwLock};
use bytes::Bytes;
use rlp::*;
use rlp_compress::{compress, decompress, blocks_swapper};
use header::*;
use transaction::*;
use views::*;
use log_entry::{LogEntry, LocalizedLogEntry};
use receipt::Receipt;
use blooms::{BloomGroup, GroupPosition};
use blockchain::best_block::{BestBlock, BestAncientBlock};
use blockchain::block_info::{BlockInfo, BlockLocation, BranchBecomingCanonChainData};
use blockchain::extras::{
    BlockReceipts, BlockDetails, TransactionAddress, EPOCH_KEY_PREFIX, EpochTransitions,
};
use types::blockchain_info::BlockChainInfo;
use types::tree_route::TreeRoute;
use blockchain::update::ExtrasUpdate;
use blockchain::{CacheSize, ImportRoute, Config};
use db::{self, Writable, Readable, CacheUpdatePolicy};
use cache_manager::CacheManager;
use encoded;
use engines::epoch::{Transition as EpochTransition, PendingTransition as PendingEpochTransition};
use rayon::prelude::*;
use ansi_term::Colour;
use kvdb::{DBTransaction, KeyValueDB};

extern crate blake2b;

const LOG_BLOOMS_LEVELS: usize = 3;
const LOG_BLOOMS_ELEMENTS_PER_INDEX: usize = 16;

/// Interface for querying blocks by hash and by number.
pub trait BlockProvider {
    /// Returns true if the given block is known
    /// (though not necessarily a part of the canon chain).
    fn is_known(&self, hash: &H256) -> bool;

    /// Get the first block of the best part of the chain.
    /// Return `None` if there is no gap and the first block is the genesis.
    /// Any queries of blocks which precede this one are not guaranteed to
    /// succeed.
    fn first_block(&self) -> Option<H256>;

    /// Get the number of the first block.
    fn first_block_number(&self) -> Option<BlockNumber> {
        self.first_block().map(|b| {
            self.block_number(&b).expect(
                "First block is always set to an existing block or `None`. Existing block always \
                 has a number; qed",
            )
        })
    }

    /// Get the best block of an first block sequence if there is a gap.
    fn best_ancient_block(&self) -> Option<H256>;

    /// Get the number of the first block.
    fn best_ancient_number(&self) -> Option<BlockNumber> {
        self.best_ancient_block().map(|h| {
            self.block_number(&h).expect(
                "Ancient block is always set to an existing block or `None`. Existing block \
                 always has a number; qed",
            )
        })
    }
    /// Get raw block data
    fn block(&self, hash: &H256) -> Option<encoded::Block>;

    /// Get the familial details concerning a block.
    fn block_details(&self, hash: &H256) -> Option<BlockDetails>;

    /// Get the hash of given block's number.
    fn block_hash(&self, index: BlockNumber) -> Option<H256>;

    /// Get the address of transaction with given hash.
    fn transaction_address(&self, hash: &H256) -> Option<TransactionAddress>;

    /// Get receipts of block with given hash.
    fn block_receipts(&self, hash: &H256) -> Option<BlockReceipts>;

    /// Get the partial-header of a block.
    fn block_header(&self, hash: &H256) -> Option<Header> {
        self.block_header_data(hash).map(|header| header.decode())
    }

    /// Get the header RLP of a block.
    fn block_header_data(&self, hash: &H256) -> Option<encoded::Header>;

    /// Get the block body (uncles and transactions).
    fn block_body(&self, hash: &H256) -> Option<encoded::Body>;

    /// Get the number of given block's hash.
    fn block_number(&self, hash: &H256) -> Option<BlockNumber> {
        self.block_details(hash).map(|details| details.number)
    }

    /// Get transaction with given transaction hash.
    fn transaction(&self, address: &TransactionAddress) -> Option<LocalizedTransaction> {
        self.block_body(&address.block_hash).and_then(|body| {
            self.block_number(&address.block_hash).and_then(|n| {
                body.view()
                    .localized_transaction_at(&address.block_hash, n, address.index)
            })
        })
    }

    /// Get transaction receipt.
    fn transaction_receipt(&self, address: &TransactionAddress) -> Option<Receipt> {
        self.block_receipts(&address.block_hash)
            .and_then(|br| br.receipts.into_iter().nth(address.index))
    }

    /// Get a list of transactions for a given block.
    /// Returns None if block does not exist.
    fn transactions(&self, hash: &H256) -> Option<Vec<LocalizedTransaction>> {
        self.block_body(hash).and_then(|body| {
            self.block_number(hash)
                .map(|n| body.view().localized_transactions(hash, n))
        })
    }

    /// Returns reference to genesis hash.
    fn genesis_hash(&self) -> H256 {
        self.block_hash(0)
            .expect("Genesis hash should always exist")
    }

    /// Returns the header of the genesis block.
    fn genesis_header(&self) -> Header {
        self.block_header(&self.genesis_hash())
            .expect("Genesis header always stored; qed")
    }

    /// Returns numbers of blocks containing given bloom.
    fn blocks_with_bloom(
        &self,
        bloom: &Bloom,
        from_block: BlockNumber,
        to_block: BlockNumber,
    ) -> Vec<BlockNumber>;

    /// Returns logs matching given filter.
    fn logs<F>(
        &self,
        blocks: Vec<BlockNumber>,
        matches: F,
        limit: Option<usize>,
    ) -> Vec<LocalizedLogEntry>
    where
        F: Fn(&LogEntry) -> bool + Send + Sync,
        Self: Sized;
}

#[derive(Debug, Hash, Eq, PartialEq, Clone)]
enum CacheId {
    BlockHeader(H256),
    BlockBody(H256),
    BlockDetails(H256),
    BlockHashes(BlockNumber),
    TransactionAddresses(H256),
    BlocksBlooms(GroupPosition),
    BlockReceipts(H256),
}

impl bc::group::BloomGroupDatabase for BlockChain {
    fn blooms_at(&self, position: &bc::group::GroupPosition) -> Option<bc::group::BloomGroup> {
        let position = GroupPosition::from(position.clone());
        let result = self
            .db
            .read_with_cache(db::COL_EXTRA, &self.blocks_blooms, &position)
            .map(Into::into);
        self.cache_man
            .lock()
            .note_used(CacheId::BlocksBlooms(position));
        result
    }
}

/// Structure providing fast access to blockchain data.
///
/// **Does not do input data verification.**
pub struct BlockChain {
    // All locks must be captured in the order declared here.
    blooms_config: bc::Config,

    best_block: RwLock<BestBlock>,
    // Stores best block of the first uninterrupted sequence of blocks. `None` if there are no gaps.
    // Only updated with `insert_unordered_block`.
    best_ancient_block: RwLock<Option<BestAncientBlock>>,
    // Stores the last block of the last sequence of blocks. `None` if there are no gaps.
    // This is calculated on start and does not get updated.
    first_block: Option<H256>,

    // block cache
    block_headers: RwLock<HashMap<H256, Bytes>>,
    block_bodies: RwLock<HashMap<H256, Bytes>>,

    // extra caches
    block_details: RwLock<HashMap<H256, BlockDetails>>,
    block_hashes: RwLock<HashMap<BlockNumber, H256>>,
    transaction_addresses: RwLock<HashMap<H256, TransactionAddress>>,
    blocks_blooms: RwLock<HashMap<GroupPosition, BloomGroup>>,
    block_receipts: RwLock<HashMap<H256, BlockReceipts>>,

    db: Arc<KeyValueDB>,

    cache_man: Mutex<CacheManager<CacheId>>,

    pending_best_block: RwLock<Option<BestBlock>>,
    pending_block_hashes: RwLock<HashMap<BlockNumber, H256>>,
    pending_block_details: RwLock<HashMap<H256, BlockDetails>>,
    pending_transaction_addresses: RwLock<HashMap<H256, Option<TransactionAddress>>>,
}

impl BlockProvider for BlockChain {
    /// Returns true if the given block is known
    /// (though not necessarily a part of the canon chain).
    fn is_known(&self, hash: &H256) -> bool {
        self.db
            .exists_with_cache(db::COL_EXTRA, &self.block_details, hash)
    }

    fn first_block(&self) -> Option<H256> { self.first_block.clone() }

    fn best_ancient_block(&self) -> Option<H256> {
        self.best_ancient_block.read().as_ref().map(|b| b.hash)
    }

    fn best_ancient_number(&self) -> Option<BlockNumber> {
        self.best_ancient_block.read().as_ref().map(|b| b.number)
    }

    /// Get raw block data
    fn block(&self, hash: &H256) -> Option<encoded::Block> {
        match (self.block_header_data(hash), self.block_body(hash)) {
            (Some(header), Some(body)) => {
                let mut block = RlpStream::new_list(2);
                let body_rlp = body.rlp();
                block.append_raw(header.rlp().as_raw(), 1);
                block.append_raw(body_rlp.at(0).as_raw(), 1);
                Some(encoded::Block::new(block.out()))
            }
            _ => None,
        }
    }

    /// Get block header data
    fn block_header_data(&self, hash: &H256) -> Option<encoded::Header> {
        // Check cache first
        {
            let read = self.block_headers.read();
            if let Some(v) = read.get(hash) {
                return Some(encoded::Header::new(v.clone()));
            }
        }

        // Check if it's the best block
        {
            let best_block = self.best_block.read();
            if &best_block.hash == hash {
                return Some(encoded::Header::new(
                    Rlp::new(&best_block.block).at(0).as_raw().to_vec(),
                ));
            }
        }

        // Read from DB and populate cache
        let opt = self
            .db
            .get(db::COL_HEADERS, hash)
            .expect("Low level database error. Some issue with disk?");

        let result = match opt {
            Some(b) => {
                let bytes = decompress(&b, blocks_swapper()).into_vec();
                let mut write = self.block_headers.write();
                write.insert(*hash, bytes.clone());
                Some(encoded::Header::new(bytes))
            }
            None => None,
        };

        self.cache_man.lock().note_used(CacheId::BlockHeader(*hash));
        result
    }

    /// Get block body data
    fn block_body(&self, hash: &H256) -> Option<encoded::Body> {
        // Check cache first
        {
            let read = self.block_bodies.read();
            if let Some(v) = read.get(hash) {
                return Some(encoded::Body::new(v.clone()));
            }
        }

        // Check if it's the best block
        {
            let best_block = self.best_block.read();
            if &best_block.hash == hash {
                return Some(encoded::Body::new(Self::block_to_body(&best_block.block)));
            }
        }

        // Read from DB and populate cache
        let opt = self
            .db
            .get(db::COL_BODIES, hash)
            .expect("Low level database error. Some issue with disk?");

        let result = match opt {
            Some(b) => {
                let bytes = decompress(&b, blocks_swapper()).into_vec();
                let mut write = self.block_bodies.write();
                write.insert(*hash, bytes.clone());
                Some(encoded::Body::new(bytes))
            }
            None => None,
        };

        self.cache_man.lock().note_used(CacheId::BlockBody(*hash));

        result
    }

    /// Get the familial details concerning a block.
    fn block_details(&self, hash: &H256) -> Option<BlockDetails> {
        let result = self
            .db
            .read_with_cache(db::COL_EXTRA, &self.block_details, hash);
        self.cache_man
            .lock()
            .note_used(CacheId::BlockDetails(*hash));
        result
    }

    /// Get the hash of given block's number.
    fn block_hash(&self, index: BlockNumber) -> Option<H256> {
        let result = self
            .db
            .read_with_cache(db::COL_EXTRA, &self.block_hashes, &index);
        self.cache_man.lock().note_used(CacheId::BlockHashes(index));
        result
    }

    /// Get the address of transaction with given hash.
    fn transaction_address(&self, hash: &H256) -> Option<TransactionAddress> {
        let result = self
            .db
            .read_with_cache(db::COL_EXTRA, &self.transaction_addresses, hash);
        self.cache_man
            .lock()
            .note_used(CacheId::TransactionAddresses(*hash));
        result
    }

    /// Get receipts of block with given hash.
    fn block_receipts(&self, hash: &H256) -> Option<BlockReceipts> {
        let result = self
            .db
            .read_with_cache(db::COL_EXTRA, &self.block_receipts, hash);
        self.cache_man
            .lock()
            .note_used(CacheId::BlockReceipts(*hash));
        result
    }

    /// Returns numbers of blocks containing given bloom.
    fn blocks_with_bloom(
        &self,
        bloom: &Bloom,
        from_block: BlockNumber,
        to_block: BlockNumber,
    ) -> Vec<BlockNumber>
    {
        let range = from_block as bc::Number..to_block as bc::Number;
        let chain = bc::group::BloomGroupChain::new(self.blooms_config, self);
        chain
            .with_bloom(&range, bloom)
            .into_iter()
            .map(|b| b as BlockNumber)
            .collect()
    }

    fn logs<F>(
        &self,
        mut blocks: Vec<BlockNumber>,
        matches: F,
        limit: Option<usize>,
    ) -> Vec<LocalizedLogEntry>
    where
        F: Fn(&LogEntry) -> bool + Send + Sync,
        Self: Sized,
    {
        // sort in reverse order
        blocks.sort_by(|a, b| b.cmp(a));

        let mut logs = blocks
            .chunks(128)
            .flat_map(move |blocks_chunk| {
                blocks_chunk
                    .into_par_iter()
                    .filter_map(|number| self.block_hash(*number).map(|hash| (*number, hash)))
                    .filter_map(|(number, hash)| {
                        self.block_receipts(&hash)
                            .map(|r| (number, hash, r.receipts))
                    })
                    .filter_map(|(number, hash, receipts)| {
                        self.block_body(&hash)
                            .map(|ref b| (number, hash, receipts, b.transaction_hashes()))
                    })
                    .flat_map(|(number, hash, mut receipts, mut hashes)| {
                        if receipts.len() != hashes.len() {
                            warn!(
                                target: "blockchain",
                                "Block {} ({}) has different number of receipts ({}) to \
                                 transactions ({}). Database corrupt?",
                                number,
                                hash,
                                receipts.len(),
                                hashes.len()
                            );
                            assert!(false);
                        }
                        let mut log_index = receipts
                            .iter()
                            .fold(0, |sum, receipt| sum + receipt.logs().len());

                        let receipts_len = receipts.len();
                        hashes.reverse();
                        receipts.reverse();
                        receipts
                            .into_iter()
                            .map(|receipt| receipt.logs().clone())
                            .zip(hashes)
                            .enumerate()
                            .flat_map(move |(index, (mut logs, tx_hash))| {
                                let current_log_index = log_index;
                                let no_of_logs = logs.len();
                                log_index -= no_of_logs;

                                logs.reverse();
                                logs.into_iter().enumerate().map(move |(i, log)| {
                                    LocalizedLogEntry {
                                        entry: log.clone(),
                                        block_hash: hash,
                                        block_number: number,
                                        transaction_hash: tx_hash,
                                        // iterating in reverse order
                                        transaction_index: receipts_len - index - 1,
                                        transaction_log_index: no_of_logs - i - 1,
                                        log_index: current_log_index - i - 1,
                                    }
                                })
                            })
                            .filter(|log_entry| matches(&log_entry.entry))
                            .take(limit.unwrap_or(::std::usize::MAX))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            })
            .take(limit.unwrap_or(::std::usize::MAX))
            .collect::<Vec<LocalizedLogEntry>>();
        logs.reverse();
        logs
    }
}

/// An iterator which walks the blockchain towards the genesis.
#[derive(Clone)]
pub struct AncestryIter<'a> {
    current: H256,
    chain: &'a BlockChain,
}

impl<'a> Iterator for AncestryIter<'a> {
    type Item = H256;
    fn next(&mut self) -> Option<H256> {
        if self.current.is_zero() {
            None
        } else {
            self.chain
                .block_details(&self.current)
                .map(|details| mem::replace(&mut self.current, details.parent))
        }
    }
}

/// An iterator which walks all epoch transitions.
/// Returns epoch transitions.
pub struct EpochTransitionIter<'a> {
    chain: &'a BlockChain,
    prefix_iter: Box<Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a>,
}

impl<'a> Iterator for EpochTransitionIter<'a> {
    type Item = (u64, EpochTransition);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.prefix_iter.next() {
                Some((key, val)) => {
                    // iterator may continue beyond values beginning with this
                    // prefix.
                    if !key.starts_with(&EPOCH_KEY_PREFIX[..]) {
                        return None;
                    }

                    let transitions: EpochTransitions = ::rlp::decode(&val[..]);

                    // if there are multiple candidates, at most one will be on the
                    // canon chain.
                    for transition in transitions.candidates.into_iter() {
                        let is_in_canon_chain = self
                            .chain
                            .block_hash(transition.block_number)
                            .map_or(false, |hash| hash == transition.block_hash);

                        // if the transition is within the block gap, there will only be
                        // one candidate.
                        let is_ancient = self
                            .chain
                            .first_block_number()
                            .map_or(false, |first| first > transition.block_number);

                        if is_ancient || is_in_canon_chain {
                            return Some((transitions.number, transition));
                        }
                    }

                    // some epochs never occurred on the main chain.
                }
                None => return None,
            }
        }
    }
}

impl BlockChain {
    /// Create new instance of blockchain from given Genesis.
    pub fn new(config: Config, genesis: &[u8], db: Arc<KeyValueDB>) -> BlockChain {
        // 400 is the avarage size of the key
        let cache_man = CacheManager::new(config.pref_cache_size, config.max_cache_size, 400);

        let mut bc = BlockChain {
            blooms_config: bc::Config {
                levels: LOG_BLOOMS_LEVELS,
                elements_per_index: LOG_BLOOMS_ELEMENTS_PER_INDEX,
            },
            first_block: None,
            best_block: RwLock::new(BestBlock::default()),
            best_ancient_block: RwLock::new(None),
            block_headers: RwLock::new(HashMap::new()),
            block_bodies: RwLock::new(HashMap::new()),
            block_details: RwLock::new(HashMap::new()),
            block_hashes: RwLock::new(HashMap::new()),
            transaction_addresses: RwLock::new(HashMap::new()),
            blocks_blooms: RwLock::new(HashMap::new()),
            block_receipts: RwLock::new(HashMap::new()),
            db: db.clone(),
            cache_man: Mutex::new(cache_man),
            pending_best_block: RwLock::new(None),
            pending_block_hashes: RwLock::new(HashMap::new()),
            pending_block_details: RwLock::new(HashMap::new()),
            pending_transaction_addresses: RwLock::new(HashMap::new()),
        };

        // load best block
        let best_block_hash = match bc
            .db
            .get(db::COL_EXTRA, b"best")
            .expect("EXTRA db not be found")
        {
            Some(best) => H256::from_slice(&best),
            None => {
                // best block does not exist
                // we need to insert genesis into the cache
                let block = BlockView::new(genesis);
                let header = block.header_view();
                let hash = block.hash();

                let details = BlockDetails {
                    number: header.number(),
                    total_difficulty: header.difficulty(),
                    parent: header.parent_hash(),
                    children: vec![],
                };

                let mut batch = DBTransaction::new();
                batch.put(db::COL_HEADERS, &hash, block.header_rlp().as_raw());
                batch.put(db::COL_BODIES, &hash, &Self::block_to_body(genesis));

                batch.write(db::COL_EXTRA, &hash, &details);
                batch.write(db::COL_EXTRA, &header.number(), &hash);

                batch.put(db::COL_EXTRA, b"best", &hash);
                bc.db
                    .write(batch)
                    .expect("Low level database error. Some issue with disk?");
                hash
            }
        };

        {
            // Fetch best block details
            let best_block_number = bc
                .block_number(&best_block_hash)
                .expect("best block not found, db may crashed");
            let best_block_total_difficulty = bc
                .block_details(&best_block_hash)
                .expect("best block not found, db may crashed")
                .total_difficulty;
            let best_block_rlp = bc
                .block(&best_block_hash)
                .expect("best block not found, db may crashed")
                .into_inner();
            let best_block_timestamp = BlockView::new(&best_block_rlp).header().timestamp();

            let raw_first = bc
                .db
                .get(db::COL_EXTRA, b"first")
                .expect("EXTRA db not be found")
                .map(|v| v.into_vec());
            let mut best_ancient = bc
                .db
                .get(db::COL_EXTRA, b"ancient")
                .expect("EXTRA db not be found")
                .map(|h| H256::from_slice(&h));
            let best_ancient_number;
            if best_ancient.is_none() && best_block_number > 1 && bc.block_hash(1).is_none() {
                best_ancient = Some(bc.genesis_hash());
                best_ancient_number = Some(0);
            } else {
                best_ancient_number = best_ancient.as_ref().and_then(|h| bc.block_number(h));
            }

            // binary search for the first block.
            match raw_first {
                None => {
                    let (mut f, mut hash) = (best_block_number, best_block_hash);
                    let mut l = best_ancient_number.unwrap_or(0);

                    loop {
                        if l >= f {
                            break;
                        }

                        let step = (f - l) >> 1;
                        let m = l + step;

                        match bc.block_hash(m) {
                            Some(h) => {
                                f = m;
                                hash = h
                            }
                            None => l = m + 1,
                        }
                    }

                    if hash != bc.genesis_hash() {
                        trace!(target:"blockchain","First block calculated: {:?}", hash);
                        let mut batch = DBTransaction::new();
                        batch.put(db::COL_EXTRA, b"first", &hash);
                        db.write(batch).expect("Low level database error.");
                        bc.first_block = Some(hash);
                    }
                }
                Some(raw_first) => {
                    bc.first_block = Some(H256::from_slice(&raw_first));
                }
            }

            // and write them
            let mut best_block = bc.best_block.write();
            *best_block = BestBlock {
                number: best_block_number,
                total_difficulty: best_block_total_difficulty,
                hash: best_block_hash,
                timestamp: best_block_timestamp,
                block: best_block_rlp,
            };

            if let (Some(hash), Some(number)) = (best_ancient, best_ancient_number) {
                let mut best_ancient_block = bc.best_ancient_block.write();
                *best_ancient_block = Some(BestAncientBlock {
                    hash: hash,
                    number: number,
                });
            }
        }

        bc
    }

    /// Returns true if the given parent block has given child
    /// (though not necessarily a part of the canon chain).
    fn is_known_child(&self, parent: &H256, hash: &H256) -> bool {
        self.db
            .read_with_cache(db::COL_EXTRA, &self.block_details, parent)
            .map_or(false, |d| d.children.contains(hash))
    }

    /// Returns a tree route between `from` and `to`, which is a tuple of:
    ///
    /// - a vector of hashes of all blocks, ordered from `from` to `to`.
    ///
    /// - common ancestor of these blocks.
    ///
    /// - an index where best common ancestor would be
    ///
    /// 1.) from newer to older
    ///
    /// - bc: `A1 -> A2 -> A3 -> A4 -> A5`
    /// - from: A5, to: A4
    /// - route:
    ///
    ///   ```json
    ///   { blocks: [A5], ancestor: A4, index: 1 }
    ///   ```
    ///
    /// 2.) from older to newer
    ///
    /// - bc: `A1 -> A2 -> A3 -> A4 -> A5`
    /// - from: A3, to: A4
    /// - route:
    ///
    ///   ```json
    ///   { blocks: [A4], ancestor: A3, index: 0 }
    ///   ```
    ///
    /// 3.) fork:
    ///
    /// - bc:
    ///
    ///   ```text
    ///   A1 -> A2 -> A3 -> A4
    ///              -> B3 -> B4
    ///   ```
    /// - from: B4, to: A4
    /// - route:
    ///
    ///   ```json
    ///   { blocks: [B4, B3, A3, A4], ancestor: A2, index: 2 }
    ///   ```
    ///
    /// If the tree route verges into pruned or unknown blocks,
    /// `None` is returned.
    pub fn tree_route(&self, from: H256, to: H256) -> Option<TreeRoute> {
        let mut from_branch = vec![];
        let mut to_branch = vec![];

        let mut from_details = self.block_details(&from)?;
        let mut to_details = self.block_details(&to)?;
        let mut current_from = from;
        let mut current_to = to;

        // reset from && to to the same level
        while from_details.number > to_details.number {
            from_branch.push(current_from);
            current_from = from_details.parent.clone();
            from_details = self.block_details(&from_details.parent)?;
        }

        while to_details.number > from_details.number {
            to_branch.push(current_to);
            current_to = to_details.parent.clone();
            to_details = self.block_details(&to_details.parent)?;
        }

        assert_eq!(from_details.number, to_details.number);

        // move to shared parent
        while current_from != current_to {
            from_branch.push(current_from);
            current_from = from_details.parent.clone();
            from_details = self.block_details(&from_details.parent)?;

            to_branch.push(current_to);
            current_to = to_details.parent.clone();
            to_details = self.block_details(&to_details.parent)?;
        }

        let index = from_branch.len();

        from_branch.extend(to_branch.into_iter().rev());

        Some(TreeRoute {
            blocks: from_branch,
            ancestor: current_from,
            index: index,
        })
    }

    /// Inserts a verified, known block from the canonical chain.
    ///
    /// Can be performed out-of-order, but care must be taken that the final chain is in a correct state.
    /// `is_best` forces the best block to be updated to this block.
    /// `is_ancient` forces the best block of the first block sequence to be updated to this block.
    /// `parent_td` is a parent total diffuculty
    /// Supply a dummy parent total difficulty when the parent block may not be in the chain.
    /// Returns true if the block is disconnected.
    pub fn insert_unordered_block(
        &self,
        batch: &mut DBTransaction,
        bytes: &[u8],
        receipts: Vec<Receipt>,
        parent_td: Option<U256>,
        is_best: bool,
        is_ancient: bool,
    ) -> bool
    {
        let block = BlockView::new(bytes);
        let header = block.header_view();
        let hash = header.hash();

        if self.is_known(&hash) {
            return false;
        }

        assert!(self.pending_best_block.read().is_none());

        let compressed_header = compress(block.header_rlp().as_raw(), blocks_swapper());
        let compressed_body = compress(&Self::block_to_body(bytes), blocks_swapper());

        // store block in db
        batch.put(db::COL_HEADERS, &hash, &compressed_header);
        batch.put(db::COL_BODIES, &hash, &compressed_body);

        let maybe_parent = self.block_details(&header.parent_hash());

        if let Some(parent_details) = maybe_parent {
            // parent known to be in chain.
            let info = BlockInfo {
                hash: hash,
                number: header.number(),
                total_difficulty: parent_details.total_difficulty + header.difficulty(),
                location: BlockLocation::CanonChain,
            };

            self.prepare_update(
                batch,
                ExtrasUpdate {
                    block_hashes: self.prepare_block_hashes_update(bytes, &info),
                    block_details: self.prepare_block_details_update(bytes, &info),
                    block_receipts: self.prepare_block_receipts_update(receipts, &info),
                    blocks_blooms: self.prepare_block_blooms_update(bytes, &info),
                    transactions_addresses: self.prepare_transaction_addresses_update(bytes, &info),
                    info: info,
                    timestamp: header.timestamp(),
                    block: bytes,
                },
                is_best,
            );

            if is_ancient {
                let mut best_ancient_block = self.best_ancient_block.write();
                let ancient_number = best_ancient_block.as_ref().map_or(0, |b| b.number);
                if self.block_hash(header.number() + 1).is_some() {
                    batch.delete(db::COL_EXTRA, b"ancient");
                    *best_ancient_block = None;
                } else if header.number() > ancient_number {
                    batch.put(db::COL_EXTRA, b"ancient", &hash);
                    *best_ancient_block = Some(BestAncientBlock {
                        hash: hash,
                        number: header.number(),
                    });
                }
            }

            false
        } else {
            // parent not in the chain yet. we need the parent difficulty to proceed.
            let d = parent_td.expect(
                "parent total difficulty always supplied for first block in chunk. only first \
                 block can have missing parent; qed",
            );

            let info = BlockInfo {
                hash: hash,
                number: header.number(),
                total_difficulty: d + header.difficulty(),
                location: BlockLocation::CanonChain,
            };

            let block_details = BlockDetails {
                number: header.number(),
                total_difficulty: info.total_difficulty,
                parent: header.parent_hash(),
                children: Vec::new(),
            };

            let mut update = HashMap::new();
            update.insert(hash, block_details);

            self.prepare_update(
                batch,
                ExtrasUpdate {
                    block_hashes: self.prepare_block_hashes_update(bytes, &info),
                    block_details: update,
                    block_receipts: self.prepare_block_receipts_update(receipts, &info),
                    blocks_blooms: self.prepare_block_blooms_update(bytes, &info),
                    transactions_addresses: self.prepare_transaction_addresses_update(bytes, &info),
                    info: info,
                    timestamp: header.timestamp(),
                    block: bytes,
                },
                is_best,
            );
            true
        }
    }

    /// Insert an epoch transition. Provide an epoch number being transitioned to
    /// and epoch transition object.
    ///
    /// The block the transition occurred at should have already been inserted into the chain.
    pub fn insert_epoch_transition(
        &self,
        batch: &mut DBTransaction,
        epoch_num: u64,
        transition: EpochTransition,
    )
    {
        let mut transitions = match self.db.read(db::COL_EXTRA, &epoch_num) {
            Some(existing) => existing,
            None => {
                EpochTransitions {
                    number: epoch_num,
                    candidates: Vec::with_capacity(1),
                }
            }
        };

        // ensure we don't write any duplicates.
        if transitions
            .candidates
            .iter()
            .find(|c| c.block_hash == transition.block_hash)
            .is_none()
        {
            transitions.candidates.push(transition);
            batch.write(db::COL_EXTRA, &epoch_num, &transitions);
        }
    }

    /// Iterate over all epoch transitions.
    /// This will only return transitions within the canonical chain.
    pub fn epoch_transitions(&self) -> EpochTransitionIter {
        let iter = self
            .db
            .iter_from_prefix(db::COL_EXTRA, &EPOCH_KEY_PREFIX[..]);
        EpochTransitionIter {
            chain: self,
            prefix_iter: iter,
        }
    }

    /// Get a specific epoch transition by block number and provided block hash.
    pub fn epoch_transition(&self, block_num: u64, block_hash: H256) -> Option<EpochTransition> {
        trace!(target: "blockchain", "Loading epoch transition at block {}, {}",
            block_num, block_hash);

        self.db
            .read(db::COL_EXTRA, &block_num)
            .and_then(|transitions: EpochTransitions| {
                transitions
                    .candidates
                    .into_iter()
                    .find(|c| c.block_hash == block_hash)
            })
    }

    /// Get the transition to the epoch the given parent hash is part of
    /// or transitions to.
    /// This will give the epoch that any children of this parent belong to.
    ///
    /// The block corresponding the the parent hash must be stored already.
    pub fn epoch_transition_for(&self, parent_hash: H256) -> Option<EpochTransition> {
        // slow path: loop back block by block
        for hash in self.ancestry_iter(parent_hash)? {
            let details = self.block_details(&hash)?;

            // look for transition in database.
            if let Some(transition) = self.epoch_transition(details.number, hash) {
                return Some(transition);
            }

            // canonical hash -> fast breakout:
            // get the last epoch transition up to this block.
            //
            // if `block_hash` is canonical it will only return transitions up to
            // the parent.
            if self.block_hash(details.number)? == hash {
                return self
                    .epoch_transitions()
                    .map(|(_, t)| t)
                    .take_while(|t| t.block_number <= details.number)
                    .last();
            }
        }

        // should never happen as the loop will encounter genesis before concluding.
        None
    }

    /// Write a pending epoch transition by block hash.
    pub fn insert_pending_transition(
        &self,
        batch: &mut DBTransaction,
        hash: H256,
        t: PendingEpochTransition,
    )
    {
        batch.write(db::COL_EXTRA, &hash, &t);
    }

    /// Get a pending epoch transition by block hash.
    // TODO: implement removal safely: this can only be done upon finality of a block
    // that _uses_ the pending transition.
    pub fn get_pending_transition(&self, hash: H256) -> Option<PendingEpochTransition> {
        self.db.read(db::COL_EXTRA, &hash)
    }

    /// Add a child to a given block. Assumes that the block hash is in
    /// the chain and the child's parent is this block.
    pub fn add_child(&self, batch: &mut DBTransaction, block_hash: H256, child_hash: H256) {
        let mut parent_details = self
            .block_details(&block_hash)
            .unwrap_or_else(|| panic!("Invalid block hash: {:?}", block_hash));

        parent_details.children.push(child_hash);

        let mut update = HashMap::new();
        update.insert(block_hash, parent_details);

        let mut write_details = self.block_details.write();
        batch.extend_with_cache(
            db::COL_EXTRA,
            &mut *write_details,
            update,
            CacheUpdatePolicy::Overwrite,
        );

        self.cache_man
            .lock()
            .note_used(CacheId::BlockDetails(block_hash));
    }

    /// Inserts the block into backing cache database.
    /// Expects the block to be valid and already verified.
    /// If the block is already known, does nothing.
    pub fn insert_block(
        &self,
        batch: &mut DBTransaction,
        bytes: &[u8],
        receipts: Vec<Receipt>,
    ) -> ImportRoute
    {
        // create views onto rlp
        let block = BlockView::new(bytes);
        let header = block.header_view();
        let hash = header.hash();

        if self.is_known_child(&header.parent_hash(), &hash) {
            return ImportRoute::none();
        }

        assert!(self.pending_best_block.read().is_none());

        let compressed_header = compress(block.header_rlp().as_raw(), blocks_swapper());
        let compressed_body = compress(&Self::block_to_body(bytes), blocks_swapper());

        // store block in db
        batch.put(db::COL_HEADERS, &hash, &compressed_header);
        batch.put(db::COL_BODIES, &hash, &compressed_body);

        let info = self.block_info(&header);

        if let BlockLocation::BranchBecomingCanonChain(ref d) = info.location {
            info!(target: "reorg", "Reorg to {} ({} {} {})",
                Colour::Yellow.bold().paint(format!("#{} {}", info.number, info.hash)),
                Colour::Red.paint(d.retracted.iter().join(" ")),
                Colour::White.paint(format!("#{} {}", self.block_details(&d.ancestor).expect("`ancestor` is in the route; qed").number, d.ancestor)),
                Colour::Green.paint(d.enacted.iter().join(" "))
            );
        }

        self.prepare_update(
            batch,
            ExtrasUpdate {
                block_hashes: self.prepare_block_hashes_update(bytes, &info),
                block_details: self.prepare_block_details_update(bytes, &info),
                block_receipts: self.prepare_block_receipts_update(receipts, &info),
                blocks_blooms: self.prepare_block_blooms_update(bytes, &info),
                transactions_addresses: self.prepare_transaction_addresses_update(bytes, &info),
                info: info.clone(),
                timestamp: header.timestamp(),
                block: bytes,
            },
            true,
        );

        ImportRoute::from(info)
    }

    /// Get inserted block info which is critical to prepare extras updates.
    fn block_info(&self, header: &HeaderView) -> BlockInfo {
        let hash = header.hash();
        let number = header.number();
        let parent_hash = header.parent_hash();
        let parent_details = self
            .block_details(&parent_hash)
            .unwrap_or_else(|| panic!("Invalid parent hash: {:?}", parent_hash));
        let is_new_best = parent_details.total_difficulty + header.difficulty()
            > self.best_block_total_difficulty();

        BlockInfo {
            hash: hash,
            number: number,
            total_difficulty: parent_details.total_difficulty + header.difficulty(),
            location: if is_new_best {
                // on new best block we need to make sure that all ancestors
                // are moved to "canon chain"
                // find the route between old best block and the new one
                let best_hash = self.best_block_hash();
                let route = self
                    .tree_route(best_hash, parent_hash)
                    .expect("blocks being imported always within recent history; qed");

                assert_eq!(number, parent_details.number + 1);

                match route.blocks.len() {
                    0 => BlockLocation::CanonChain,
                    _ => {
                        let retracted = route
                            .blocks
                            .iter()
                            .take(route.index)
                            .cloned()
                            .collect::<Vec<_>>()
                            .into_iter()
                            .collect::<Vec<_>>();
                        let enacted = route
                            .blocks
                            .into_iter()
                            .skip(route.index)
                            .collect::<Vec<_>>();
                        BlockLocation::BranchBecomingCanonChain(BranchBecomingCanonChainData {
                            ancestor: route.ancestor,
                            enacted: enacted,
                            retracted: retracted,
                        })
                    }
                }
            } else {
                BlockLocation::Branch
            },
        }
    }

    /// Prepares extras update.
    fn prepare_update(&self, batch: &mut DBTransaction, update: ExtrasUpdate, is_best: bool) {
        {
            let mut write_receipts = self.block_receipts.write();
            batch.extend_with_cache(
                db::COL_EXTRA,
                &mut *write_receipts,
                update.block_receipts,
                CacheUpdatePolicy::Remove,
            );
        }

        {
            let mut write_blocks_blooms = self.blocks_blooms.write();
            // update best block
            match update.info.location {
                BlockLocation::Branch => (),
                BlockLocation::BranchBecomingCanonChain(_) => {
                    // clear all existing blooms, cause they may be created for block
                    // number higher than current best block
                    *write_blocks_blooms = update.blocks_blooms;
                    for (key, value) in write_blocks_blooms.iter() {
                        batch.write(db::COL_EXTRA, key, value);
                    }
                }
                BlockLocation::CanonChain => {
                    // update all existing blooms groups
                    for (key, value) in update.blocks_blooms {
                        match write_blocks_blooms.entry(key) {
                            hash_map::Entry::Occupied(mut entry) => {
                                entry.get_mut().accrue_bloom_group(&value);
                                batch.write(db::COL_EXTRA, entry.key(), entry.get());
                            }
                            hash_map::Entry::Vacant(entry) => {
                                batch.write(db::COL_EXTRA, entry.key(), &value);
                                entry.insert(value);
                            }
                        }
                    }
                }
            }
        }

        // These cached values must be updated last with all four locks taken to avoid
        // cache decoherence
        {
            let mut best_block = self.pending_best_block.write();
            if is_best && update.info.location != BlockLocation::Branch {
                batch.put(db::COL_EXTRA, b"best", &update.info.hash);
                *best_block = Some(BestBlock {
                    hash: update.info.hash,
                    number: update.info.number,
                    total_difficulty: update.info.total_difficulty,
                    timestamp: update.timestamp,
                    block: update.block.to_vec(),
                });
            }

            let mut write_hashes = self.pending_block_hashes.write();
            let mut write_details = self.pending_block_details.write();
            let mut write_txs = self.pending_transaction_addresses.write();

            batch.extend_with_cache(
                db::COL_EXTRA,
                &mut *write_details,
                update.block_details,
                CacheUpdatePolicy::Overwrite,
            );
            batch.extend_with_cache(
                db::COL_EXTRA,
                &mut *write_hashes,
                update.block_hashes,
                CacheUpdatePolicy::Overwrite,
            );
            batch.extend_with_option_cache(
                db::COL_EXTRA,
                &mut *write_txs,
                update.transactions_addresses,
                CacheUpdatePolicy::Overwrite,
            );
        }
    }

    /// Apply pending insertion updates
    pub fn commit(&self) {
        let mut pending_best_block = self.pending_best_block.write();
        let mut pending_write_hashes = self.pending_block_hashes.write();
        let mut pending_block_details = self.pending_block_details.write();
        let mut pending_write_txs = self.pending_transaction_addresses.write();

        let mut best_block = self.best_block.write();
        let mut write_block_details = self.block_details.write();
        let mut write_hashes = self.block_hashes.write();
        let mut write_txs = self.transaction_addresses.write();
        // update best block
        if let Some(block) = pending_best_block.take() {
            *best_block = block;
        }

        let pending_txs = mem::replace(&mut *pending_write_txs, HashMap::new());
        let (retracted_txs, enacted_txs) = pending_txs
            .into_iter()
            .partition::<HashMap<_, _>, _>(|&(_, ref value)| value.is_none());

        let pending_hashes_keys: Vec<_> = pending_write_hashes.keys().cloned().collect();
        let enacted_txs_keys: Vec<_> = enacted_txs.keys().cloned().collect();
        let pending_block_hashes: Vec<_> = pending_block_details.keys().cloned().collect();

        write_hashes.extend(mem::replace(&mut *pending_write_hashes, HashMap::new()));
        write_txs.extend(
            enacted_txs
                .into_iter()
                .map(|(k, v)| (k, v.expect("Transactions were partitioned; qed"))),
        );
        write_block_details.extend(mem::replace(&mut *pending_block_details, HashMap::new()));

        for hash in retracted_txs.keys() {
            write_txs.remove(hash);
        }

        let mut cache_man = self.cache_man.lock();
        for n in pending_hashes_keys {
            cache_man.note_used(CacheId::BlockHashes(n));
        }

        for hash in enacted_txs_keys {
            cache_man.note_used(CacheId::TransactionAddresses(hash));
        }

        for hash in pending_block_hashes {
            cache_man.note_used(CacheId::BlockDetails(hash));
        }
    }

    /// Iterator that lists `first` and then all of `first`'s ancestors, by hash.
    pub fn ancestry_iter(&self, first: H256) -> Option<AncestryIter> {
        if self.is_known(&first) {
            Some(AncestryIter {
                current: first,
                chain: self,
            })
        } else {
            None
        }
    }

    /// This function returns modified block hashes.
    fn prepare_block_hashes_update(
        &self,
        block_bytes: &[u8],
        info: &BlockInfo,
    ) -> HashMap<BlockNumber, H256>
    {
        let mut block_hashes = HashMap::new();
        let block = BlockView::new(block_bytes);
        let header = block.header_view();
        let number = header.number();

        match info.location {
            BlockLocation::Branch => (),
            BlockLocation::CanonChain => {
                block_hashes.insert(number, info.hash);
            }
            BlockLocation::BranchBecomingCanonChain(ref data) => {
                let ancestor_number = self
                    .block_number(&data.ancestor)
                    .expect("Block number of ancestor is always in DB");
                let start_number = ancestor_number + 1;

                for (index, hash) in data.enacted.iter().cloned().enumerate() {
                    block_hashes.insert(start_number + index as BlockNumber, hash);
                }

                block_hashes.insert(number, info.hash);
            }
        }

        block_hashes
    }

    /// This function returns modified block details.
    /// Uses the given parent details or attempts to load them from the database.
    fn prepare_block_details_update(
        &self,
        block_bytes: &[u8],
        info: &BlockInfo,
    ) -> HashMap<H256, BlockDetails>
    {
        let block = BlockView::new(block_bytes);
        let header = block.header_view();
        let parent_hash = header.parent_hash();
        let mut parent_details = self
            .block_details(&parent_hash)
            .unwrap_or_else(|| panic!("Invalid parent hash: {:?}", parent_hash));
        parent_details.children.push(info.hash);

        // create current block details.
        let details = BlockDetails {
            number: header.number(),
            total_difficulty: info.total_difficulty,
            parent: parent_hash,
            children: vec![],
        };

        // write to batch
        let mut block_details = HashMap::new();
        block_details.insert(parent_hash, parent_details);
        block_details.insert(info.hash, details);
        block_details
    }

    /// This function returns modified block receipts.
    fn prepare_block_receipts_update(
        &self,
        receipts: Vec<Receipt>,
        info: &BlockInfo,
    ) -> HashMap<H256, BlockReceipts>
    {
        let mut block_receipts = HashMap::new();
        block_receipts.insert(info.hash, BlockReceipts::new(receipts));
        block_receipts
    }

    /// This function returns modified transaction addresses.
    fn prepare_transaction_addresses_update(
        &self,
        block_bytes: &[u8],
        info: &BlockInfo,
    ) -> HashMap<H256, Option<TransactionAddress>>
    {
        let block = BlockView::new(block_bytes);
        let transaction_hashes = block.transaction_hashes();

        match info.location {
            BlockLocation::CanonChain => {
                transaction_hashes
                    .into_iter()
                    .enumerate()
                    .map(|(i, tx_hash)| {
                        (
                            tx_hash,
                            Some(TransactionAddress {
                                block_hash: info.hash,
                                index: i,
                            }),
                        )
                    })
                    .collect()
            }
            BlockLocation::BranchBecomingCanonChain(ref data) => {
                let addresses = data.enacted.iter().flat_map(|hash| {
                    let body = self
                        .block_body(hash)
                        .expect("Enacted block must be in database.");
                    let hashes = body.transaction_hashes();
                    hashes
                        .into_iter()
                        .enumerate()
                        .map(|(i, tx_hash)| {
                            (
                                tx_hash,
                                Some(TransactionAddress {
                                    block_hash: *hash,
                                    index: i,
                                }),
                            )
                        })
                        .collect::<HashMap<H256, Option<TransactionAddress>>>()
                });

                let current_addresses =
                    transaction_hashes
                        .into_iter()
                        .enumerate()
                        .map(|(i, tx_hash)| {
                            (
                                tx_hash,
                                Some(TransactionAddress {
                                    block_hash: info.hash,
                                    index: i,
                                }),
                            )
                        });

                let retracted = data.retracted.iter().flat_map(|hash| {
                    let body = self
                        .block_body(hash)
                        .expect("Retracted block must be in database.");
                    let hashes = body.transaction_hashes();
                    hashes
                        .into_iter()
                        .map(|hash| (hash, None))
                        .collect::<HashMap<H256, Option<TransactionAddress>>>()
                });

                // The order here is important! Don't remove transaction if it was part of enacted blocks as well.
                retracted
                    .chain(addresses)
                    .chain(current_addresses)
                    .collect()
            }
            BlockLocation::Branch => HashMap::new(),
        }
    }

    /// This functions returns modified blocks blooms.
    ///
    /// To accelerate blooms lookups, blomms are stored in multiple
    /// layers (BLOOM_LEVELS, currently 3).
    /// ChainFilter is responsible for building and rebuilding these layers.
    /// It returns them in HashMap, where values are Blooms and
    /// keys are BloomIndexes. BloomIndex represents bloom location on one
    /// of these layers.
    ///
    /// To reduce number of queries to databse, block blooms are stored
    /// in BlocksBlooms structure which contains info about several
    /// (BLOOM_INDEX_SIZE, currently 16) consecutive blocks blooms.
    ///
    /// Later, BloomIndexer is used to map bloom location on filter layer (BloomIndex)
    /// to bloom location in database (BlocksBloomLocation).
    ///
    fn prepare_block_blooms_update(
        &self,
        block_bytes: &[u8],
        info: &BlockInfo,
    ) -> HashMap<GroupPosition, BloomGroup>
    {
        let block = BlockView::new(block_bytes);
        let header = block.header_view();

        let log_blooms = match info.location {
            BlockLocation::Branch => HashMap::new(),
            BlockLocation::CanonChain => {
                let log_bloom = header.log_bloom();
                if log_bloom.is_zero() {
                    HashMap::new()
                } else {
                    let chain = bc::group::BloomGroupChain::new(self.blooms_config, self);
                    chain.insert(info.number as bc::Number, log_bloom)
                }
            }
            BlockLocation::BranchBecomingCanonChain(ref data) => {
                let ancestor_number = self
                    .block_number(&data.ancestor)
                    .expect("block ancestor not found, db may crashed");
                let start_number = ancestor_number + 1;
                let range = start_number as bc::Number..self.best_block_number() as bc::Number;

                let mut blooms: Vec<Bloom> = data
                    .enacted
                    .iter()
                    .map(|hash| {
                        self.block_header_data(hash)
                            .expect("block ancestor not found, db may crashed")
                    })
                    .map(|h| h.log_bloom())
                    .collect();

                blooms.push(header.log_bloom());

                let chain = bc::group::BloomGroupChain::new(self.blooms_config, self);
                chain.replace(&range, blooms)
            }
        };

        log_blooms
            .into_iter()
            .map(|p| (From::from(p.0), From::from(p.1)))
            .collect()
    }

    /// Get best block hash.
    pub fn best_block_hash(&self) -> H256 { self.best_block.read().hash }

    /// Get best block number.
    pub fn best_block_number(&self) -> BlockNumber { self.best_block.read().number }

    /// Get best block timestamp.
    pub fn best_block_timestamp(&self) -> u64 { self.best_block.read().timestamp }

    /// Get best block total difficulty.
    pub fn best_block_total_difficulty(&self) -> U256 { self.best_block.read().total_difficulty }

    /// Get best block header
    pub fn best_block_header(&self) -> encoded::Header {
        let block = self.best_block.read();
        let raw = BlockView::new(&block.block)
            .header_view()
            .rlp()
            .as_raw()
            .to_vec();
        encoded::Header::new(raw)
    }

    /// Get current cache size.
    pub fn cache_size(&self) -> CacheSize {
        CacheSize {
            blocks: self.block_headers.read().heap_size_of_children()
                + self.block_bodies.read().heap_size_of_children(),
            block_details: self.block_details.read().heap_size_of_children(),
            transaction_addresses: self.transaction_addresses.read().heap_size_of_children(),
            blocks_blooms: self.blocks_blooms.read().heap_size_of_children(),
            block_receipts: self.block_receipts.read().heap_size_of_children(),
        }
    }

    /// Ticks our cache system and throws out any old data.
    pub fn collect_garbage(&self) {
        let current_size = self.cache_size().total();

        let mut block_headers = self.block_headers.write();
        let mut block_bodies = self.block_bodies.write();
        let mut block_details = self.block_details.write();
        let mut block_hashes = self.block_hashes.write();
        let mut transaction_addresses = self.transaction_addresses.write();
        let mut blocks_blooms = self.blocks_blooms.write();
        let mut block_receipts = self.block_receipts.write();

        let mut cache_man = self.cache_man.lock();
        cache_man.collect_garbage(current_size, |ids| {
            for id in &ids {
                match *id {
                    CacheId::BlockHeader(ref h) => {
                        block_headers.remove(h);
                    }
                    CacheId::BlockBody(ref h) => {
                        block_bodies.remove(h);
                    }
                    CacheId::BlockDetails(ref h) => {
                        block_details.remove(h);
                    }
                    CacheId::BlockHashes(ref h) => {
                        block_hashes.remove(h);
                    }
                    CacheId::TransactionAddresses(ref h) => {
                        transaction_addresses.remove(h);
                    }
                    CacheId::BlocksBlooms(ref h) => {
                        blocks_blooms.remove(h);
                    }
                    CacheId::BlockReceipts(ref h) => {
                        block_receipts.remove(h);
                    }
                }
            }

            block_headers.shrink_to_fit();
            block_bodies.shrink_to_fit();
            block_details.shrink_to_fit();
            block_hashes.shrink_to_fit();
            transaction_addresses.shrink_to_fit();
            blocks_blooms.shrink_to_fit();
            block_receipts.shrink_to_fit();

            block_headers.heap_size_of_children()
                + block_bodies.heap_size_of_children()
                + block_details.heap_size_of_children()
                + block_hashes.heap_size_of_children()
                + transaction_addresses.heap_size_of_children()
                + blocks_blooms.heap_size_of_children()
                + block_receipts.heap_size_of_children()
        });
    }

    /// Create a block body from a block.
    pub fn block_to_body(block: &[u8]) -> Bytes {
        let mut body = RlpStream::new_list(1);
        let block_rlp = Rlp::new(block);
        body.append_raw(block_rlp.at(1).as_raw(), 1);
        body.out()
    }

    /// Returns general blockchain information
    pub fn chain_info(&self) -> BlockChainInfo {
        // ensure data consistencly by locking everything first
        let best_block = self.best_block.read();
        let best_ancient_block = self.best_ancient_block.read();
        BlockChainInfo {
            total_difficulty: best_block.total_difficulty.clone(),
            pending_total_difficulty: best_block.total_difficulty.clone(),
            genesis_hash: self.genesis_hash(),
            best_block_hash: best_block.hash,
            best_block_number: best_block.number,
            best_block_timestamp: best_block.timestamp,
            first_block_hash: self.first_block(),
            first_block_number: From::from(self.first_block_number()),
            ancient_block_hash: best_ancient_block.as_ref().map(|b| b.hash),
            ancient_block_number: best_ancient_block.as_ref().map(|b| b.number),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::iter;
    use std::sync::Arc;
    use rustc_hex::FromHex;
    use kvdb::{KeyValueDB, MockDbRepository, DBTransaction};
    use aion_types::*;
    use ethbloom::Bloom;
    use receipt::{Receipt, SimpleReceipt};
    use blockchain::{BlockProvider, BlockChain, Config, ImportRoute};
    use tests::helpers::*;
    use blockchain::generator::{BlockGenerator, BlockBuilder, BlockOptions};
    use blockchain::extras::TransactionAddress;
    use transaction::{Transaction, Action, DEFAULT_TRANSACTION_TYPE};
    use log_entry::{LogEntry, LocalizedLogEntry};
    use bytes::Bytes;
    use keychain;
    use db;

    fn new_db() -> Arc<KeyValueDB> {
        let mut db_configs = Vec::new();
        for db_name in db::DB_NAMES.to_vec() {
            db_configs.push(db_name.into());
        }
        Arc::new(MockDbRepository::init(db_configs))
    }

    fn new_chain(genesis: &[u8], db: Arc<KeyValueDB>) -> BlockChain {
        BlockChain::new(Config::default(), genesis, db)
    }

    #[test]
    fn should_cache_best_block() {
        // given
        let genesis = BlockBuilder::genesis();
        let first = genesis.add_block();

        let db = new_db();
        let bc = new_chain(&genesis.last().encoded(), db.clone());
        assert_eq!(bc.best_block_number(), 0);

        // when
        let mut batch = DBTransaction::new();
        bc.insert_block(&mut batch, &first.last().encoded(), vec![]);
        assert_eq!(bc.best_block_number(), 0);
        bc.commit();
        // NOTE no db.write here (we want to check if best block is cached)

        // then
        assert_eq!(bc.best_block_number(), 1);
        assert!(
            bc.block(&bc.best_block_hash()).is_some(),
            "Best block should be queryable even without DB write."
        );
    }

    #[test]
    fn basic_blockchain_insert() {
        let genesis = BlockBuilder::genesis();
        let first = genesis.add_block();

        let genesis = genesis.last();
        let first = first.last();
        let genesis_hash = genesis.hash();
        let first_hash = first.hash();

        let db = new_db();
        let bc = new_chain(&genesis.encoded(), db.clone());

        assert_eq!(bc.genesis_hash(), genesis_hash);
        assert_eq!(bc.best_block_hash(), genesis_hash);
        assert_eq!(bc.block_hash(0), Some(genesis_hash));
        assert_eq!(bc.block_hash(1), None);
        assert_eq!(bc.block_details(&genesis_hash).unwrap().children, vec![]);

        let mut batch = DBTransaction::new();
        bc.insert_block(&mut batch, &first.encoded(), vec![]);
        db.write(batch).unwrap();
        bc.commit();

        assert_eq!(bc.block_hash(0), Some(genesis_hash));
        assert_eq!(bc.best_block_number(), 1);
        assert_eq!(bc.best_block_hash(), first_hash);
        assert_eq!(bc.block_hash(1), Some(first_hash));
        assert_eq!(bc.block_details(&first_hash).unwrap().parent, genesis_hash);
        assert_eq!(
            bc.block_details(&genesis_hash).unwrap().children,
            vec![first_hash]
        );
        assert_eq!(bc.block_hash(2), None);
    }

    #[test]
    fn check_ancestry_iter() {
        let genesis = BlockBuilder::genesis();
        let first_10 = genesis.add_blocks(10);
        let generator = BlockGenerator::new(vec![first_10]);

        let db = new_db();
        let bc = new_chain(&genesis.last().encoded(), db.clone());

        let mut block_hashes = vec![genesis.last().hash()];
        let mut batch = DBTransaction::new();
        for block in generator {
            block_hashes.push(block.hash());
            bc.insert_block(&mut batch, &block.encoded(), vec![]);
            bc.commit();
        }
        db.write(batch).unwrap();

        block_hashes.reverse();

        assert_eq!(
            bc.ancestry_iter(block_hashes[0].clone())
                .unwrap()
                .collect::<Vec<_>>(),
            block_hashes
        );
        assert_eq!(block_hashes.len(), 11);
    }

    #[test]
    fn test_fork_transaction_addresses() {
        let t1 = Transaction {
            nonce: 0.into(),
            gas_price: 0.into(),
            gas: 100_000.into(),
            action: Action::Create,
            value: 100.into(),
            data: "601080600c6000396000f3006000355415600957005b60203560003555"
                .from_hex()
                .unwrap(),
            transaction_type: DEFAULT_TRANSACTION_TYPE,
            nonce_bytes: Vec::new(),
            gas_bytes: Vec::new(),
            gas_price_bytes: Vec::new(),
            value_bytes: Vec::new(),
        }
        .sign(keychain::ethkey::generate_keypair().secret(), None);

        let t1_hash = t1.hash();

        let genesis = BlockBuilder::genesis();
        let b1a = genesis.add_block_with_transactions(iter::once(t1));
        let b1b = genesis.add_block_with_difficulty(9);
        let b2 = b1b.add_block();

        let b1a_hash = b1a.last().hash();
        let b2_hash = b2.last().hash();

        let db = new_db();
        let bc = new_chain(&genesis.last().encoded(), db.clone());

        let mut batch = DBTransaction::new();
        let _ = bc.insert_block(&mut batch, &b1a.last().encoded(), vec![]);
        bc.commit();
        let _ = bc.insert_block(&mut batch, &b1b.last().encoded(), vec![]);
        bc.commit();
        db.write(batch).unwrap();

        assert_eq!(bc.best_block_hash(), b1a_hash);
        assert_eq!(
            bc.transaction_address(&t1_hash),
            Some(TransactionAddress {
                block_hash: b1a_hash,
                index: 0,
            })
        );

        // now let's make forked chain the canon chain
        let mut batch = DBTransaction::new();
        let _ = bc.insert_block(&mut batch, &b2.last().encoded(), vec![]);
        bc.commit();
        db.write(batch).unwrap();

        // Transaction should be retracted
        assert_eq!(bc.best_block_hash(), b2_hash);
        assert_eq!(bc.transaction_address(&t1_hash), None);
    }

    #[test]
    fn test_overwriting_transaction_addresses() {
        let keypair = keychain::ethkey::generate_keypair();
        let t1 = Transaction {
            nonce: 0.into(),
            gas_price: 0.into(),
            gas: 100_000.into(),
            action: Action::Create,
            value: 100.into(),
            data: "601080600c6000396000f3006000355415600957005b60203560003555"
                .from_hex()
                .unwrap(),
            transaction_type: DEFAULT_TRANSACTION_TYPE,
            gas_price_bytes: Vec::new(),
            gas_bytes: Vec::new(),
            value_bytes: Vec::new(),
            nonce_bytes: Vec::new(),
        }
        .sign(&keypair.secret(), None);

        let t2 = Transaction {
            nonce: 1.into(),
            gas_price: 0.into(),
            gas: 100_000.into(),
            action: Action::Create,
            value: 100.into(),
            data: "601080600c6000396000f3006000355415600957005b60203560003555"
                .from_hex()
                .unwrap(),
            gas_price_bytes: Vec::new(),
            gas_bytes: Vec::new(),
            value_bytes: Vec::new(),
            nonce_bytes: Vec::new(),
            transaction_type: DEFAULT_TRANSACTION_TYPE,
        }
        .sign(&keypair.secret(), None);
        let t3 = Transaction {
            nonce: 2.into(),
            gas_price: 0.into(),
            gas: 100_000.into(),
            action: Action::Create,
            value: 100.into(),
            data: "601080600c6000396000f3006000355415600957005b60203560003555"
                .from_hex()
                .unwrap(),
            gas_price_bytes: Vec::new(),
            gas_bytes: Vec::new(),
            value_bytes: Vec::new(),
            nonce_bytes: Vec::new(),
            transaction_type: DEFAULT_TRANSACTION_TYPE,
        }
        .sign(&keypair.secret(), None);

        let genesis = BlockBuilder::genesis();
        let b1a = genesis.add_block_with_transactions(vec![t1.clone(), t2.clone()]);
        // insert transactions in different order,
        // the block has lower difficulty, so the hash is also different
        let b1b = genesis.add_block_with(|| {
            BlockOptions {
                difficulty: 9.into(),
                transactions: vec![t2.clone(), t1.clone()],
                ..Default::default()
            }
        });
        let b2 = b1b.add_block_with_transactions(iter::once(t3.clone()));

        let b1a_hash = b1a.last().hash();
        let b1b_hash = b1b.last().hash();
        let b2_hash = b2.last().hash();

        let t1_hash = t1.hash();
        let t2_hash = t2.hash();
        let t3_hash = t3.hash();

        let db = new_db();
        let bc = new_chain(&genesis.last().encoded(), db.clone());

        let mut batch = DBTransaction::new();
        let _ = bc.insert_block(&mut batch, &b1a.last().encoded(), vec![]);
        bc.commit();
        let _ = bc.insert_block(&mut batch, &b1b.last().encoded(), vec![]);
        bc.commit();
        db.write(batch).unwrap();

        assert_eq!(bc.best_block_hash(), b1a_hash);
        assert_eq!(
            bc.transaction_address(&t1_hash),
            Some(TransactionAddress {
                block_hash: b1a_hash,
                index: 0,
            })
        );
        assert_eq!(
            bc.transaction_address(&t2_hash),
            Some(TransactionAddress {
                block_hash: b1a_hash,
                index: 1,
            })
        );

        // now let's make forked chain the canon chain
        let mut batch = DBTransaction::new();
        let _ = bc.insert_block(&mut batch, &b2.last().encoded(), vec![]);
        bc.commit();
        db.write(batch).unwrap();

        assert_eq!(bc.best_block_hash(), b2_hash);
        assert_eq!(
            bc.transaction_address(&t1_hash),
            Some(TransactionAddress {
                block_hash: b1b_hash,
                index: 1,
            })
        );
        assert_eq!(
            bc.transaction_address(&t2_hash),
            Some(TransactionAddress {
                block_hash: b1b_hash,
                index: 0,
            })
        );
        assert_eq!(
            bc.transaction_address(&t3_hash),
            Some(TransactionAddress {
                block_hash: b2_hash,
                index: 0,
            })
        );
    }

    #[test]
    fn test_small_fork() {
        let genesis = BlockBuilder::genesis();
        let b1 = genesis.add_block();
        let b2 = b1.add_block();
        let b3a = b2.add_block();
        let b3b = b2.add_block_with_difficulty(9);

        let genesis_hash = genesis.last().hash();
        let b1_hash = b1.last().hash();
        let b2_hash = b2.last().hash();
        let b3a_hash = b3a.last().hash();
        let b3b_hash = b3b.last().hash();

        // b3a is a part of canon chain, whereas b3b is part of sidechain
        let best_block_hash = b3a_hash;

        let db = new_db();
        let bc = new_chain(&genesis.last().encoded(), db.clone());

        let mut batch = DBTransaction::new();
        let ir1 = bc.insert_block(&mut batch, &b1.last().encoded(), vec![]);
        bc.commit();
        let ir2 = bc.insert_block(&mut batch, &b2.last().encoded(), vec![]);
        bc.commit();
        let ir3b = bc.insert_block(&mut batch, &b3b.last().encoded(), vec![]);
        bc.commit();
        db.write(batch).unwrap();
        assert_eq!(bc.block_hash(3).unwrap(), b3b_hash);
        let mut batch = DBTransaction::new();
        let ir3a = bc.insert_block(&mut batch, &b3a.last().encoded(), vec![]);
        bc.commit();
        db.write(batch).unwrap();

        assert_eq!(
            ir1,
            ImportRoute {
                enacted: vec![b1_hash],
                retracted: vec![],
                omitted: vec![],
            }
        );

        assert_eq!(
            ir2,
            ImportRoute {
                enacted: vec![b2_hash],
                retracted: vec![],
                omitted: vec![],
            }
        );

        assert_eq!(
            ir3b,
            ImportRoute {
                enacted: vec![b3b_hash],
                retracted: vec![],
                omitted: vec![],
            }
        );

        assert_eq!(
            ir3a,
            ImportRoute {
                enacted: vec![b3a_hash],
                retracted: vec![b3b_hash],
                omitted: vec![],
            }
        );

        assert_eq!(bc.best_block_hash(), best_block_hash);
        assert_eq!(bc.block_number(&genesis_hash).unwrap(), 0);
        assert_eq!(bc.block_number(&b1_hash).unwrap(), 1);
        assert_eq!(bc.block_number(&b2_hash).unwrap(), 2);
        assert_eq!(bc.block_number(&b3a_hash).unwrap(), 3);
        assert_eq!(bc.block_number(&b3b_hash).unwrap(), 3);

        assert_eq!(bc.block_hash(0).unwrap(), genesis_hash);
        assert_eq!(bc.block_hash(1).unwrap(), b1_hash);
        assert_eq!(bc.block_hash(2).unwrap(), b2_hash);
        assert_eq!(bc.block_hash(3).unwrap(), b3a_hash);

        // test trie route
        let r0_1 = bc.tree_route(genesis_hash, b1_hash).unwrap();
        assert_eq!(r0_1.ancestor, genesis_hash);
        assert_eq!(r0_1.blocks, [b1_hash]);
        assert_eq!(r0_1.index, 0);

        let r0_2 = bc.tree_route(genesis_hash, b2_hash).unwrap();
        assert_eq!(r0_2.ancestor, genesis_hash);
        assert_eq!(r0_2.blocks, [b1_hash, b2_hash]);
        assert_eq!(r0_2.index, 0);

        let r1_3a = bc.tree_route(b1_hash, b3a_hash).unwrap();
        assert_eq!(r1_3a.ancestor, b1_hash);
        assert_eq!(r1_3a.blocks, [b2_hash, b3a_hash]);
        assert_eq!(r1_3a.index, 0);

        let r1_3b = bc.tree_route(b1_hash, b3b_hash).unwrap();
        assert_eq!(r1_3b.ancestor, b1_hash);
        assert_eq!(r1_3b.blocks, [b2_hash, b3b_hash]);
        assert_eq!(r1_3b.index, 0);

        let r3a_3b = bc.tree_route(b3a_hash, b3b_hash).unwrap();
        assert_eq!(r3a_3b.ancestor, b2_hash);
        assert_eq!(r3a_3b.blocks, [b3a_hash, b3b_hash]);
        assert_eq!(r3a_3b.index, 1);

        let r1_0 = bc.tree_route(b1_hash, genesis_hash).unwrap();
        assert_eq!(r1_0.ancestor, genesis_hash);
        assert_eq!(r1_0.blocks, [b1_hash]);
        assert_eq!(r1_0.index, 1);

        let r2_0 = bc.tree_route(b2_hash, genesis_hash).unwrap();
        assert_eq!(r2_0.ancestor, genesis_hash);
        assert_eq!(r2_0.blocks, [b2_hash, b1_hash]);
        assert_eq!(r2_0.index, 2);

        let r3a_1 = bc.tree_route(b3a_hash, b1_hash).unwrap();
        assert_eq!(r3a_1.ancestor, b1_hash);
        assert_eq!(r3a_1.blocks, [b3a_hash, b2_hash]);
        assert_eq!(r3a_1.index, 2);

        let r3b_1 = bc.tree_route(b3b_hash, b1_hash).unwrap();
        assert_eq!(r3b_1.ancestor, b1_hash);
        assert_eq!(r3b_1.blocks, [b3b_hash, b2_hash]);
        assert_eq!(r3b_1.index, 2);

        let r3b_3a = bc.tree_route(b3b_hash, b3a_hash).unwrap();
        assert_eq!(r3b_3a.ancestor, b2_hash);
        assert_eq!(r3b_3a.blocks, [b3b_hash, b3a_hash]);
        assert_eq!(r3b_3a.index, 1);
    }

    #[test]
    fn test_reopen_blockchain_db() {
        let genesis = BlockBuilder::genesis();
        let first = genesis.add_block();
        let genesis_hash = genesis.last().hash();
        let first_hash = first.last().hash();

        let db = new_db();

        {
            let bc = new_chain(&genesis.last().encoded(), db.clone());
            assert_eq!(bc.best_block_hash(), genesis_hash);
            let mut batch = DBTransaction::new();
            bc.insert_block(&mut batch, &first.last().encoded(), vec![]);
            db.write(batch).unwrap();
            bc.commit();
            assert_eq!(bc.best_block_hash(), first_hash);
        }

        {
            let bc = new_chain(&genesis.last().encoded(), db.clone());

            assert_eq!(bc.best_block_hash(), first_hash);
        }
    }

    #[test]
    fn can_contain_arbitrary_block_sequence() {
        let bc = generate_dummy_blockchain(50);
        assert_eq!(bc.best_block_number(), 49);
    }

    #[test]
    fn can_collect_garbage() {
        let bc = generate_dummy_blockchain(3000);

        assert_eq!(bc.best_block_number(), 2999);
        let best_hash = bc.best_block_hash();
        let mut block_header = bc.block_header(&best_hash);

        while !block_header.is_none() {
            block_header = bc.block_header(block_header.unwrap().parent_hash());
        }
        assert!(bc.cache_size().blocks > 1024 * 1024);

        for _ in 0..2 {
            bc.collect_garbage();
        }
        assert!(bc.cache_size().blocks < 1024 * 1024);
    }

    #[test]
    fn can_contain_arbitrary_block_sequence_with_extra() {
        let bc = generate_dummy_blockchain_with_extra(25);
        assert_eq!(bc.best_block_number(), 24);
    }

    #[test]
    fn can_contain_only_genesis_block() {
        let bc = generate_dummy_empty_blockchain();
        assert_eq!(bc.best_block_number(), 0);
    }

    #[test]
    fn find_transaction_by_hash() {
        let genesis = "f9077ef9077a0180a06a6d99a2ef14ab3b835dfc92fb918d76c37f6578a69825fbe19cd366485604b1a00000000000000000000000000000000000000000000000000000000000000000a03663a3a8bc1204f4c3ac972278493e26a339b7fb720c94a777a86a39debdf810a045b0cfc220ceec5b7c1c62c4d4193d38e4eba48e8815729ce75f9c0ab0e4c1c0a045b0cfc220ceec5b7c1c62c4d4193d38e4eba48e8815729ce75f9c0ab0e4c1c0b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010a000000000000000000000000000000000000000000000000000000000000001008083e4e1c0845ade7380a00000000000000000000000000000000000000000000000000000000000000000b9058000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0".from_hex().unwrap();
        let b1 = "f908ccf9078c0101a0ef32028308d0dc0376be3ddde8ec56fd23d1142300441afc459c3c6bdc39a7d1a00000000000000000000000000000000000000000000000000000000000000000a08f3b78418265c4112d517180089ca78ebdfa005610b4890d0ec3a05b4894e6aea013f0924f46521a109a46d1c30a79b754e7f1cc5e234366f2454ebf0f135622bda00e6a1d518ad68354e3efdabe300ff14dee3a47d77309cf275f9d1e49359d41f8b90100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000009000000000000000000000000000000010a041494f4e0000000000000000000000000000000000000000000000000000000082a41083e4a889845b864af2a00400000000000000000000000000000000000001000000000000000000000000b9058000a7e2ebbd73cdac14f8c01cfaca912e63c7cd63c192169b76790863f1757a34e06f68aa204a9860b43997940d741a94ee98991ff6ad1820527f81890823473785eb25b5ef1e9dad79c47d6e195fc04b30a10b80affeac5511d74b3ba501ab2256dd616d2a02be1287e7ce7557ec4f21aede56bc129330c2e370db2bbcffb97c7efddccc313ee94c4ff55081a7bfe65e75f68f7eb80ff62aaea341bff9b01ba71fae7db7106d972b9e4c99848bf0e2d502a3144e2c77ec91b41b9c2728b5d24682d180a861b6058565e2e68076a6e7d8463c33ed28dc171276fe1dcb07a3ae9fa6ba8066f47b82092f39b5b525ff75f637194e37a67d92972ef2fe121e5e0ef60371ac6550388e75163c75886dd38eacf56fa8246cf14aed2e3918fb904f16592af2eb0eeda87cd20920b4ce8acafdda94b7e6741bb9fb67c336e05faa69db5c6f75a94c4b0e667330c1440cfd54e03447045ad442a972c780c04d8ddafc2c1e0128b3055e340760a0812a3fa7f9086fb7e2bc72acef0bf5d1e431eb640ab2c4852bfe5e58ada6df066fb90e06928f161f6392ceedda894b4abbd9b266cf9ea3a87b1bea90b1cc3c6781bdb47e54242ac70928ca5de81470012e152dc10b0080be3d0a1a9f387d87bbb2b9bb5e650eef97644939328bb19a4d528162f92f1b91e3fdd5ba05dc45bda431ab1d738b7677eb435ff1ba9738b6ba9362c447699a180d00f7c1ce6453da239aea645fbe448602ed881fc476569c1a4421445c560f1b57cfdca8904e088a674e13f8a79a752c4973ff638a331b4b3a7ea5ca09367e262664c538a312b90a3499b97ea3b04d631cc94df593ed13c9018eb1d7305ec4163b73076940a058a71e1cabed5b84edc9735f87463e9180f33a4b367855b979b96b584aad24db78285088ba976e3c8a4bdba9d3d83cec02c1b734f5601886b674e8b6b38eb7c14aa4b13d7e51f2aba6b8a6e06b55648c9843617f1b5df62e6ec801f065bb8c81640b71561508ccd12290f28e666028e507b147aa5bf75846fdb724d021bb65143fd6ffbf926f1c64b674efca2b3171546954f175a0bed6bd862c552831091bffd52660e56373a842319e40117690e29d2ac1071a3a48d389804e79aa920e6ff179e3f0ff455900a52cfd2fcd4f44232475840d6d88de75c8a8d1783d59560d5d420fd57223b9271c033f072f611d4c9465b86fd027ff4cfb48560f8bb9c6b63ab76ef49454ca0d1ca6ce06a913b123131f2a1a105b5e6fe3705295e7e4ffbb593d62f30cda47c402f41afa74c3b25a6e6b4408ef5ba60a0f7ce21a61b45561c2790f430ded3ab4c743738ee7281151df1552bab96facd5ad4b330bca7d3a7477ad3e0792ed488925fc31eed2828f35029fbc0a3f90f3747a20eaebb1f9669bc2a6955025a346a175e374449c026422f473483f094c872b23d34a7c22a2255712ba7af9635ffa7185358aeb91320e0869223df12fa82d416a6026039785792351219be47249566a26288df6929db2e3134a77b60a42d6aaa39bf4d65b53c9cc8576f9896f43b70983505eb0741d639b02151927255b871a347b36f1943d76f5618ea9912febe3fc7903dabdc3b99607371b4b0e7887599851e53750d35c6456eefccb7d5ee43b9f02377dc631e7b4fbc9d6e8b149827a54457bef1a79b4001283e7183c0173418c3e1b27e557d3ee727e9e3b3ed5366eaa21e66aeb4776c6a974d432bedd276f8461f7eb09b8aecd95a0b535502cc6136a87985a6354cc99ecbd440c038b0f197ff32efbbc4c80bb679d18c3102edcc41b1c73c445a30853b3f2d34bc743964547d26e6e17cc38fb22f46147b7f7e39cf5429f05f7bb28f361ebda3610d6e54b24ccb5bcf6c13864ed06546018863fa25bf311399db17353f253a065bf25b211ff0d8bade1b2cef627f0ab8d33f472fde7ef0955b5b3bde869e74e765b6e3861b968bdb7d2a274e1e05b2417643f18354de1ce23f9013af89b80a0a054340a3152d10006b66c4248cfa73e5725056294081c476c0e67ef5ad25334820fff80880005748de2c04d69830e57e0841f38b2e601b8608bc5c4e5599afac7cb0efcb0010540017dda3e80870bb543b356867b2a8cacbfcdffb6e1b3784f4497b6121502a0991077c657e4f8e5b68f24b3644964fcf6935a3d6735521ae94c1a361d692c04769e8e8fb19392a9badd73002ce13dbf5c08f89b01a0a054340a3152d10006b66c4248cfa73e5725056294081c476c0e67ef5ad25334820fff80880005748de73f18bb830e57e0841f38b2e601b8608bc5c4e5599afac7cb0efcb0010540017dda3e80870bb543b356867b2a8cacbf516f28ee029ef5bf3231862b4065ddd9195ae560e42c216918b4d045889a37e8b7c5b0648c3b5d4190382ec34a22179c1cca4572b2ad5d5c431370c9d4a91c05".from_hex().unwrap();
        let b1_hash: H256 =
            "e6a15bb33f19c1292aec97acc24b35b8d2b3312619102f4887a9e4eee5171f0e".into();

        let db = new_db();
        let bc = new_chain(&genesis, db.clone());
        let mut batch = DBTransaction::new();
        bc.insert_block(&mut batch, &b1, vec![]);
        db.write(batch).unwrap();
        bc.commit();

        let transactions = bc.transactions(&b1_hash).unwrap();
        assert_eq!(transactions.len(), 2);
        for t in transactions {
            assert_eq!(
                bc.transaction(&bc.transaction_address(&t.hash()).unwrap())
                    .unwrap(),
                t
            );
        }
    }

    fn insert_block(
        db: &Arc<KeyValueDB>,
        bc: &BlockChain,
        bytes: &[u8],
        receipts: Vec<Receipt>,
    ) -> ImportRoute
    {
        let mut batch = DBTransaction::new();
        let res = bc.insert_block(&mut batch, bytes, receipts);
        db.write(batch).unwrap();
        bc.commit();
        res
    }

    #[test]
    fn test_logs() {
        let keypair = keychain::ethkey::generate_keypair();
        let t1 = Transaction {
            nonce: 0.into(),
            gas_price: 0.into(),
            gas: 100_000.into(),
            action: Action::Create,
            value: 101.into(),
            data: "601080600c6000396000f3006000355415600957005b60203560003555"
                .from_hex()
                .unwrap(),
            nonce_bytes: Vec::new(),
            gas_price_bytes: Vec::new(),
            gas_bytes: Vec::new(),
            value_bytes: Vec::new(),
            transaction_type: DEFAULT_TRANSACTION_TYPE,
        }
        .sign(keypair.secret(), None);
        let t2 = Transaction {
            nonce: 0.into(),
            gas_price: 0.into(),
            gas: 100_000.into(),
            action: Action::Create,
            value: 102.into(),
            data: "601080600c6000396000f3006000355415600957005b60203560003555"
                .from_hex()
                .unwrap(),
            nonce_bytes: Vec::new(),
            gas_price_bytes: Vec::new(),
            gas_bytes: Vec::new(),
            value_bytes: Vec::new(),
            transaction_type: DEFAULT_TRANSACTION_TYPE,
        }
        .sign(keypair.secret(), None);
        let t3 = Transaction {
            nonce: 0.into(),
            gas_price: 0.into(),
            gas: 100_000.into(),
            action: Action::Create,
            value: 103.into(),
            data: "601080600c6000396000f3006000355415600957005b60203560003555"
                .from_hex()
                .unwrap(),
            nonce_bytes: Vec::new(),
            gas_price_bytes: Vec::new(),
            gas_bytes: Vec::new(),
            value_bytes: Vec::new(),
            transaction_type: DEFAULT_TRANSACTION_TYPE,
        }
        .sign(keypair.secret(), None);
        let tx_hash1 = t1.hash();
        let tx_hash2 = t2.hash();
        let tx_hash3 = t3.hash();

        let genesis = BlockBuilder::genesis();
        let b1 = genesis.add_block_with_transactions(vec![t1, t2]);
        let b2 = b1.add_block_with_transactions(iter::once(t3));
        let b1_hash = b1.last().hash();
        let b1_number = b1.last().number();
        let b2_hash = b2.last().hash();
        let b2_number = b2.last().number();

        let db = new_db();
        let bc = new_chain(&genesis.last().encoded(), db.clone());
        insert_block(
            &db,
            &bc,
            &b1.last().encoded(),
            vec![
                Receipt {
                    simple_receipt: SimpleReceipt {
                        state_root: H256::default(),
                        log_bloom: Default::default(),
                        logs: vec![
                            LogEntry {
                                address: Default::default(),
                                topics: vec![],
                                data: vec![1],
                            },
                            LogEntry {
                                address: Default::default(),
                                topics: vec![],
                                data: vec![2],
                            },
                        ],
                    },
                    gas_used: 10_000.into(),
                    transaction_fee: U256::zero(),
                    output: Bytes::default(),
                    error_message: String::default(),
                },
                Receipt {
                    simple_receipt: SimpleReceipt {
                        state_root: H256::default(),
                        log_bloom: Default::default(),
                        logs: vec![LogEntry {
                            address: Default::default(),
                            topics: vec![],
                            data: vec![3],
                        }],
                    },
                    gas_used: 10_000.into(),
                    transaction_fee: U256::zero(),
                    output: Bytes::default(),
                    error_message: String::default(),
                },
            ],
        );
        insert_block(
            &db,
            &bc,
            &b2.last().encoded(),
            vec![Receipt {
                simple_receipt: SimpleReceipt {
                    state_root: H256::default(),
                    log_bloom: Default::default(),
                    logs: vec![LogEntry {
                        address: Default::default(),
                        topics: vec![],
                        data: vec![4],
                    }],
                },
                gas_used: 10_000.into(),
                transaction_fee: U256::zero(),
                output: Bytes::default(),
                error_message: String::default(),
            }],
        );

        // when
        let logs1 = bc.logs(vec![1, 2], |_| true, None);
        let logs2 = bc.logs(vec![1, 2], |_| true, Some(1));

        // then
        assert_eq!(
            logs1,
            vec![
                LocalizedLogEntry {
                    entry: LogEntry {
                        address: Default::default(),
                        topics: vec![],
                        data: vec![1],
                    },
                    block_hash: b1_hash,
                    block_number: b1_number,
                    transaction_hash: tx_hash1,
                    transaction_index: 0,
                    transaction_log_index: 0,
                    log_index: 0,
                },
                LocalizedLogEntry {
                    entry: LogEntry {
                        address: Default::default(),
                        topics: vec![],
                        data: vec![2],
                    },
                    block_hash: b1_hash,
                    block_number: b1_number,
                    transaction_hash: tx_hash1,
                    transaction_index: 0,
                    transaction_log_index: 1,
                    log_index: 1,
                },
                LocalizedLogEntry {
                    entry: LogEntry {
                        address: Default::default(),
                        topics: vec![],
                        data: vec![3],
                    },
                    block_hash: b1_hash,
                    block_number: b1_number,
                    transaction_hash: tx_hash2,
                    transaction_index: 1,
                    transaction_log_index: 0,
                    log_index: 2,
                },
                LocalizedLogEntry {
                    entry: LogEntry {
                        address: Default::default(),
                        topics: vec![],
                        data: vec![4],
                    },
                    block_hash: b2_hash,
                    block_number: b2_number,
                    transaction_hash: tx_hash3,
                    transaction_index: 0,
                    transaction_log_index: 0,
                    log_index: 0,
                },
            ]
        );
        assert_eq!(
            logs2,
            vec![LocalizedLogEntry {
                entry: LogEntry {
                    address: Default::default(),
                    topics: vec![],
                    data: vec![4],
                },
                block_hash: b2_hash,
                block_number: b2_number,
                transaction_hash: tx_hash3,
                transaction_index: 0,
                transaction_log_index: 0,
                log_index: 0,
            }]
        );
    }

    #[test]
    fn test_bloom_filter_simple() {
        let bloom_b1: Bloom = "00000020000000000000000000000000000000000000000002000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008000400000000000000000000002000".into();

        let bloom_b2: Bloom = "00000000000000000000000000000000000000000000020000001000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000008000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".into();

        let bloom_ba: Bloom = "00000000000000000000000000000000000000000000020000000800000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000008000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".into();

        let genesis = BlockBuilder::genesis();
        let b1 = genesis.add_block_with(|| {
            BlockOptions {
                bloom: bloom_b1.clone(),
                difficulty: 9.into(),
                ..Default::default()
            }
        });
        let b2 = b1.add_block_with_bloom(bloom_b2);
        let b3 = b2.add_block_with_bloom(bloom_ba);

        let b1a = genesis.add_block_with_bloom(bloom_ba);
        let b2a = b1a.add_block_with_bloom(bloom_ba);

        let db = new_db();
        let bc = new_chain(&genesis.last().encoded(), db.clone());

        let blocks_b1 = bc.blocks_with_bloom(&bloom_b1, 0, 5);
        let blocks_b2 = bc.blocks_with_bloom(&bloom_b2, 0, 5);
        assert!(blocks_b1.is_empty());
        assert!(blocks_b2.is_empty());

        insert_block(&db, &bc, &b1.last().encoded(), vec![]);
        let blocks_b1 = bc.blocks_with_bloom(&bloom_b1, 0, 5);
        let blocks_b2 = bc.blocks_with_bloom(&bloom_b2, 0, 5);
        assert_eq!(blocks_b1, vec![1]);
        assert!(blocks_b2.is_empty());

        insert_block(&db, &bc, &b2.last().encoded(), vec![]);
        let blocks_b1 = bc.blocks_with_bloom(&bloom_b1, 0, 5);
        let blocks_b2 = bc.blocks_with_bloom(&bloom_b2, 0, 5);
        assert_eq!(blocks_b1, vec![1]);
        assert_eq!(blocks_b2, vec![2]);

        // hasn't been forked yet
        insert_block(&db, &bc, &b1a.last().encoded(), vec![]);
        let blocks_b1 = bc.blocks_with_bloom(&bloom_b1, 0, 5);
        let blocks_b2 = bc.blocks_with_bloom(&bloom_b2, 0, 5);
        let blocks_ba = bc.blocks_with_bloom(&bloom_ba, 0, 5);
        assert_eq!(blocks_b1, vec![1]);
        assert_eq!(blocks_b2, vec![2]);
        assert!(blocks_ba.is_empty());

        // fork has happend
        insert_block(&db, &bc, &b2a.last().encoded(), vec![]);
        let blocks_b1 = bc.blocks_with_bloom(&bloom_b1, 0, 5);
        let blocks_b2 = bc.blocks_with_bloom(&bloom_b2, 0, 5);
        let blocks_ba = bc.blocks_with_bloom(&bloom_ba, 0, 5);
        assert!(blocks_b1.is_empty());
        assert!(blocks_b2.is_empty());
        assert_eq!(blocks_ba, vec![1, 2]);

        // fork back
        insert_block(&db, &bc, &b3.last().encoded(), vec![]);
        let blocks_b1 = bc.blocks_with_bloom(&bloom_b1, 0, 5);
        let blocks_b2 = bc.blocks_with_bloom(&bloom_b2, 0, 5);
        let blocks_ba = bc.blocks_with_bloom(&bloom_ba, 0, 5);
        assert_eq!(blocks_b1, vec![1]);
        assert_eq!(blocks_b2, vec![2]);
        assert_eq!(blocks_ba, vec![3]);
    }

    #[test]
    fn test_insert_unordered() {
        let bloom_b1: Bloom = "00000020000000000000000000000000000000000000000002000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008000400000000000000000000002000".into();

        let bloom_b2: Bloom = "00000000000000000000000000000000000000000000020000001000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000008000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".into();

        let bloom_b3: Bloom = "00000000000000000000000000000000000000000000020000000800000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000008000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".into();

        let genesis = BlockBuilder::genesis();
        let b1 = genesis.add_block_with_bloom(bloom_b1);
        let b2 = b1.add_block_with_bloom(bloom_b2);
        let b3 = b2.add_block_with_bloom(bloom_b3);
        let b1_total_difficulty = genesis.last().difficulty() + b1.last().difficulty();

        let db = new_db();
        let bc = new_chain(&genesis.last().encoded(), db.clone());
        let mut batch = DBTransaction::new();
        bc.insert_unordered_block(
            &mut batch,
            &b2.last().encoded(),
            vec![],
            Some(b1_total_difficulty),
            false,
            false,
        );
        bc.commit();
        bc.insert_unordered_block(&mut batch, &b3.last().encoded(), vec![], None, true, false);
        bc.commit();
        bc.insert_unordered_block(&mut batch, &b1.last().encoded(), vec![], None, false, false);
        bc.commit();
        db.write(batch).unwrap();

        assert_eq!(bc.best_block_hash(), b3.last().hash());
        assert_eq!(bc.block_hash(1).unwrap(), b1.last().hash());
        assert_eq!(bc.block_hash(2).unwrap(), b2.last().hash());
        assert_eq!(bc.block_hash(3).unwrap(), b3.last().hash());

        let blocks_b1 = bc.blocks_with_bloom(&bloom_b1, 0, 3);
        let blocks_b2 = bc.blocks_with_bloom(&bloom_b2, 0, 3);
        let blocks_b3 = bc.blocks_with_bloom(&bloom_b3, 0, 3);

        assert_eq!(blocks_b1, vec![1]);
        assert_eq!(blocks_b2, vec![2]);
        assert_eq!(blocks_b3, vec![3]);
    }

    #[test]
    fn test_best_block_update() {
        let genesis = BlockBuilder::genesis();
        let next_5 = genesis.add_blocks(5);
        let uncle = genesis.add_block_with_difficulty(9);
        let generator = BlockGenerator::new(iter::once(next_5));

        let db = new_db();
        {
            let bc = new_chain(&genesis.last().encoded(), db.clone());

            let mut batch = DBTransaction::new();
            // create a longer fork
            for block in generator {
                bc.insert_block(&mut batch, &block.encoded(), vec![]);
                bc.commit();
            }

            assert_eq!(bc.best_block_number(), 5);
            bc.insert_block(&mut batch, &uncle.last().encoded(), vec![]);
            db.write(batch).unwrap();
            bc.commit();
        }

        // re-loading the blockchain should load the correct best block.
        let bc = new_chain(&genesis.last().encoded(), db);
        assert_eq!(bc.best_block_number(), 5);
    }

    #[test]
    fn epoch_transitions_iter() {
        use engines::EpochTransition;

        let genesis = BlockBuilder::genesis();
        let next_5 = genesis.add_blocks(5);
        let uncle = genesis.add_block_with_difficulty(9);
        let generator = BlockGenerator::new(iter::once(next_5));

        let db = new_db();
        {
            let bc = new_chain(&genesis.last().encoded(), db.clone());

            let mut batch = DBTransaction::new();
            // create a longer fork
            for (i, block) in generator.into_iter().enumerate() {
                bc.insert_block(&mut batch, &block.encoded(), vec![]);
                bc.insert_epoch_transition(
                    &mut batch,
                    i as u64,
                    EpochTransition {
                        block_hash: block.hash(),
                        block_number: i as u64 + 1,
                        proof: vec![],
                    },
                );
                bc.commit();
            }

            assert_eq!(bc.best_block_number(), 5);

            bc.insert_block(&mut batch, &uncle.last().encoded(), vec![]);
            bc.insert_epoch_transition(
                &mut batch,
                999,
                EpochTransition {
                    block_hash: uncle.last().hash(),
                    block_number: 1,
                    proof: vec![],
                },
            );

            db.write(batch).unwrap();
            bc.commit();

            // epoch 999 not in canonical chain.
            assert_eq!(
                bc.epoch_transitions().map(|(i, _)| i).collect::<Vec<_>>(),
                vec![0, 1, 2, 3, 4]
            );
        }

        // re-loading the blockchain should load the correct best block.
        let bc = new_chain(&genesis.last().encoded(), db);

        assert_eq!(bc.best_block_number(), 5);
        assert_eq!(
            bc.epoch_transitions().map(|(i, _)| i).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
    }

    #[test]
    fn epoch_transition_for() {
        use engines::EpochTransition;

        let genesis = BlockBuilder::genesis();
        let fork_7 = genesis.add_blocks_with(7, || {
            BlockOptions {
                difficulty: 9.into(),
                ..Default::default()
            }
        });
        let next_10 = genesis.add_blocks(10);
        let fork_generator = BlockGenerator::new(iter::once(fork_7));
        let next_generator = BlockGenerator::new(iter::once(next_10));

        let db = new_db();

        let bc = new_chain(&genesis.last().encoded(), db.clone());

        let mut batch = DBTransaction::new();
        bc.insert_epoch_transition(
            &mut batch,
            0,
            EpochTransition {
                block_hash: bc.genesis_hash(),
                block_number: 0,
                proof: vec![],
            },
        );
        db.write(batch).unwrap();

        // set up a chain where we have a canonical chain of 10 blocks
        // and a non-canonical fork of 8 from genesis.
        let fork_hash = {
            for block in fork_generator {
                let mut batch = DBTransaction::new();

                bc.insert_block(&mut batch, &block.encoded(), vec![]);
                bc.commit();
                db.write(batch).unwrap();
            }

            assert_eq!(bc.best_block_number(), 7);
            bc.chain_info().best_block_hash
        };

        for block in next_generator {
            let mut batch = DBTransaction::new();
            bc.insert_block(&mut batch, &block.encoded(), vec![]);
            bc.commit();

            db.write(batch).unwrap();
        }

        assert_eq!(bc.best_block_number(), 10);

        let mut batch = DBTransaction::new();
        bc.insert_epoch_transition(
            &mut batch,
            4,
            EpochTransition {
                block_hash: bc.block_hash(4).unwrap(),
                block_number: 4,
                proof: vec![],
            },
        );
        db.write(batch).unwrap();

        // blocks where the parent is one of the first 4 will be part of genesis epoch.
        for i in 0..4 {
            let hash = bc.block_hash(i).unwrap();
            assert_eq!(bc.epoch_transition_for(hash).unwrap().block_number, 0);
        }

        // blocks where the parent is the transition at 4 or after will be
        // part of that epoch.
        for i in 4..11 {
            let hash = bc.block_hash(i).unwrap();
            assert_eq!(bc.epoch_transition_for(hash).unwrap().block_number, 4);
        }

        let fork_hashes = bc.ancestry_iter(fork_hash).unwrap().collect::<Vec<_>>();
        assert_eq!(fork_hashes.len(), 8);

        // non-canonical fork blocks should all have genesis transition
        for fork_hash in fork_hashes {
            assert_eq!(bc.epoch_transition_for(fork_hash).unwrap().block_number, 0);
        }
    }
}
