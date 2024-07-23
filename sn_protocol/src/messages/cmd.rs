// Copyright 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.
#![allow(clippy::mutable_key_type)] // for Bytes in NetworkAddress

use std::collections::HashMap;

use crate::{storage::RecordType, NetworkAddress};
use serde::{Deserialize, Serialize};
// TODO: remove this dependency and define these types herein.
pub use sn_transfers::{Hash, PaymentQuote};

/// Data and CashNote cmds - recording spends or creating, updating, and removing data.
///
/// See the [`protocol`] module documentation for more details of the types supported by the Safe
/// Network, and their semantics.
///
/// [`protocol`]: crate
#[allow(clippy::large_enum_variant)]
#[derive(Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum Cmd {
    /// Write operation to notify peer fetch a list of [`NetworkAddress`] from the holder.
    ///
    /// [`NetworkAddress`]: crate::NetworkAddress
    Replicate {
        /// Holder of the replication keys.
        holder: NetworkAddress,
        /// Keys of copy that shall be replicated.
        keys: Vec<(NetworkAddress, RecordType)>,
    },
    /// Ask the peer to trigger replication of the data that we are missing
    RequestReplication {
        /// Keys of the data that we currently hold.
        keys: HashMap<NetworkAddress, RecordType>,
    },
    /// Write operation to notify nodes a list of PaymentQuote collected.
    QuoteVerification {
        target: NetworkAddress,
        quotes: Vec<(NetworkAddress, PaymentQuote)>,
    },
    /// Notify the peer it is now being considered as BAD due to the included behaviour
    PeerConsideredAsBad {
        detected_by: NetworkAddress,
        bad_peer: NetworkAddress,
        bad_behaviour: String,
    },
}

impl std::fmt::Debug for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Cmd::Replicate { holder, keys } => {
                let first_ten_keys: Vec<_> = keys.iter().take(10).collect();
                f.debug_struct("Cmd::Replicate")
                    .field("holder", holder)
                    .field("keys_len", &keys.len())
                    .field("first_ten_keys", &first_ten_keys)
                    .finish()
            }
            Cmd::RequestReplication { keys: holding_keys } => {
                let first_ten_keys: Vec<_> = holding_keys.iter().take(10).collect();
                f.debug_struct("Cmd::RequestReplication")
                    .field("keys_len", &holding_keys.len())
                    .field("first_ten_keys", &first_ten_keys)
                    .finish()
            }
            Cmd::QuoteVerification { target, quotes } => f
                .debug_struct("Cmd::QuoteVerification")
                .field("target", target)
                .field("quotes_len", &quotes.len())
                .finish(),
            Cmd::PeerConsideredAsBad {
                detected_by,
                bad_peer,
                bad_behaviour,
            } => f
                .debug_struct("Cmd::PeerConsideredAsBad")
                .field("detected_by", detected_by)
                .field("bad_peer", bad_peer)
                .field("bad_behaviour", bad_behaviour)
                .finish(),
        }
    }
}

impl std::fmt::Display for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Cmd::Replicate { holder, keys } => {
                write!(
                    f,
                    "Cmd::Replicate({:?} has {} keys)",
                    holder.as_peer_id(),
                    keys.len()
                )
            }
            Cmd::RequestReplication { keys: holding_keys } => {
                write!(f, "Cmd::RequestReplication({} keys)", holding_keys.len())
            }
            Cmd::QuoteVerification { target, quotes } => {
                write!(
                    f,
                    "Cmd::QuoteVerification(sent to {target:?} has {} quotes)",
                    quotes.len()
                )
            }
            Cmd::PeerConsideredAsBad {
                detected_by,
                bad_peer,
                bad_behaviour,
            } => {
                write!(
                    f,
                    "Cmd::PeerConsideredAsBad({detected_by:?} consider peer {bad_peer:?} as bad, due to {bad_behaviour:?})")
            }
        }
    }
}
