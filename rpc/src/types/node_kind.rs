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

//! Description of the node.

/// Describes the kind of node. This information can provide a hint to
/// applications about how to utilize the RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeKind {
    /// The capability of the node.
    pub capability: Capability,
    /// Who the node is available to.
    pub availability: Availability,
}

/// Who the node is available to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Availability {
    /// A personal node, not intended to be available to everyone.
    #[serde(rename = "personal")]
    Personal,
    /// A public, open node.
    #[serde(rename = "public")]
    Public,
}

/// The capability of the node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Capability {
    /// A full node stores the full state and fully enacts incoming blocks.
    #[serde(rename = "full")]
    Full,
    /// A light node does a minimal header sync and fetches data as needed
    /// from the network.
    #[serde(rename = "light")]
    Light,
}

#[cfg(test)]
mod tests {
    use super::{NodeKind, Availability, Capability};
    use serde_json;

    #[test]
    fn availability() {
        let personal = r#""personal""#;
        let public = r#""public""#;

        assert_eq!(
            serde_json::to_string(&Availability::Personal).unwrap(),
            personal
        );
        assert_eq!(
            serde_json::to_string(&Availability::Public).unwrap(),
            public
        );

        assert_eq!(
            serde_json::from_str::<Availability>(personal).unwrap(),
            Availability::Personal
        );
        assert_eq!(
            serde_json::from_str::<Availability>(public).unwrap(),
            Availability::Public
        );
    }

    #[test]
    fn capability() {
        let light = r#""light""#;
        let full = r#""full""#;

        assert_eq!(serde_json::to_string(&Capability::Light).unwrap(), light);
        assert_eq!(serde_json::to_string(&Capability::Full).unwrap(), full);

        assert_eq!(
            serde_json::from_str::<Capability>(light).unwrap(),
            Capability::Light
        );
        assert_eq!(
            serde_json::from_str::<Capability>(full).unwrap(),
            Capability::Full
        );
    }

    #[test]
    fn node_kind() {
        let kind = NodeKind {
            capability: Capability::Full,
            availability: Availability::Public,
        };
        let s = r#"{"capability":"full","availability":"public"}"#;

        assert_eq!(serde_json::to_string(&kind).unwrap(), s);
        assert_eq!(serde_json::from_str::<NodeKind>(s).unwrap(), kind);
    }
}
