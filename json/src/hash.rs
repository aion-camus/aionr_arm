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

//! Lenient hash json deserialization for test json files.

use std::str::FromStr;
use std::fmt;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde::de::{Error, Visitor};
use rustc_hex::ToHex;
use aion_types::{H64 as Hash64, H256 as Hash256, H520 as Hash520};
use ethbloom::Bloom as Hash2048;

macro_rules! impl_hash {
    ($name:ident, $inner:ident) => {
        /// Lenient hash json deserialization for test json files.
        #[derive(Default, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Clone)]
        pub struct $name(pub $inner);

        impl From<$name> for $inner {
            fn from(other: $name) -> $inner { other.0 }
        }

        impl From<$inner> for $name {
            fn from(i: $inner) -> Self { $name(i) }
        }

        impl<'a> Deserialize<'a> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where D: Deserializer<'a> {
                struct HashVisitor;

                impl<'b> Visitor<'b> for HashVisitor {
                    type Value = $name;

                    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                        write!(formatter, "a 0x-prefixed hex-encoded hash")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where E: Error {
                        let value = match value.len() {
                            0 => $inner::from(0),
                            2 if value == "0x" => $inner::from(0),
                            _ if value.starts_with("0x") => {
                                $inner::from_str(&value[2..]).map_err(|e| {
                                    Error::custom(
                                        format!("Invalid hex value {}: {}", value, e).as_str(),
                                    )
                                })?
                            }
                            _ => {
                                $inner::from_str(value).map_err(|e| {
                                    Error::custom(
                                        format!("Invalid hex value {}: {}", value, e).as_str(),
                                    )
                                })?
                            }
                        };

                        Ok($name(value))
                    }

                    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
                    where E: Error {
                        self.visit_str(value.as_ref())
                    }
                }

                deserializer.deserialize_any(HashVisitor)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where S: Serializer {
                let mut hex = "0x".to_owned();
                hex.push_str(&self.0.to_hex());
                serializer.serialize_str(&hex)
            }
        }
    };
}

impl_hash!(H64, Hash64);
impl_hash!(Address, Hash256);
impl_hash!(H256, Hash256);
impl_hash!(H520, Hash520);
impl_hash!(Bloom, Hash2048);

#[cfg(test)]
mod test {
    use std::str::FromStr;
    use serde_json;
    use aion_types;
    use hash::H256;

    #[test]
    fn hash_deserialization() {
        let s = r#"["", "5a39ed1020c04d4d84539975b893a4e7c53eab6c2965db8bc3468093a31bc5ae"]"#;
        let deserialized: Vec<H256> = serde_json::from_str(s).unwrap();
        assert_eq!(
            deserialized,
            vec![
                H256(aion_types::H256::from(0)),
                H256(
                    aion_types::H256::from_str(
                        "5a39ed1020c04d4d84539975b893a4e7c53eab6c2965db8bc3468093a31bc5ae",
                    )
                    .unwrap(),
                ),
            ]
        );
    }

    #[test]
    fn hash_into() {
        assert_eq!(
            aion_types::H256::from(0),
            H256(aion_types::H256::from(0)).into()
        );
    }
}
