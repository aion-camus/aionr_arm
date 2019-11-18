/*******************************************************************************
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

use traits::KeyValueDAO;
use std::collections::BTreeMap;
use super::{Key, DBValue};

/// Rocksdb mock instance in memory
pub struct Mockkvdb {
    db: BTreeMap<Key, DBValue>,
}

impl Mockkvdb {
    /// New instance in memory
    pub fn new_default() -> Self {
        Mockkvdb {
            db: BTreeMap::new(),
        }
    }
    pub fn open() -> Self { Mockkvdb::new_default() }
}

impl KeyValueDAO for Mockkvdb {
    fn get(&self, k: &[u8]) -> Option<DBValue> {
        match self.db.get(k) {
            Some(v) => Some(v.clone()),
            None => None,
        }
    }

    fn put(&mut self, k: &[u8], v: &DBValue) -> Option<DBValue> {
        let mut ekey = Key::new();
        ekey.append_slice(k);
        self.db.insert(ekey, v.clone())
    }

    fn delete(&mut self, k: &[u8]) -> Option<DBValue> {
        let mut ekey = Key::new();
        ekey.append_slice(k);
        self.db.remove(&ekey)
    }

    fn iter(&self) -> Box<Iterator<Item = (Box<[u8]>, Box<[u8]>)>> {
        Box::new(self.db.clone().into_iter().map(|(k, v)| {
            (
                k.into_vec().into_boxed_slice(),
                v.into_vec().into_boxed_slice(),
            )
        }))
    }

    fn get_by_prefix(&self, prefix: &[u8]) -> Option<Box<[u8]>> {
        self.db
            .clone()
            .iter()
            .find(|&(ref k, _)| k.starts_with(prefix))
            .map(|(_, v)| v.clone().into_vec().into_boxed_slice())
    }

    fn iter_from_prefix(
        &self,
        prefix: &'static [u8],
    ) -> Box<Iterator<Item = (Box<[u8]>, Box<[u8]>)>>
    {
        Box::new(
            self.db
                .clone()
                .into_iter()
                .skip_while(move |(k, _)| !k.starts_with(prefix))
                .map(|(k, v)| {
                    (
                        k.into_vec().into_boxed_slice(),
                        v.into_vec().into_boxed_slice(),
                    )
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crud_test() {
        let mut db = Mockkvdb::new_default();

        let key1: Vec<u8> = vec![1];
        let value1: Vec<u8> = vec![1];
        let key2: Vec<u8> = vec![2];
        let value2: Vec<u8> = vec![2];
        let value3: Vec<u8> = vec![3];

        db.put(&key1, &DBValue::from_vec(value1.clone()));
        assert_eq!(db.get(&key1).unwrap(), value1);

        db.put(&key2, &DBValue::from_vec(value2.clone()));
        assert_eq!(db.get(&key2).unwrap(), value2);

        db.put(&key1, &DBValue::from_vec(value3.clone()));
        assert_eq!(db.get(&key1).unwrap(), value3);

        db.delete(&key1);
        db.delete(&key2);

        assert_eq!(db.get(&key1), None);
    }
}
