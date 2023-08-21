// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

#[macro_use]
extern crate tracing;
use libp2p::PeerId;
use sn_networking::Network;
use sn_networking::{sort_peers_by_address, Error as NetworkError, CLOSE_GROUP_SIZE};
use sn_protocol::{
    messages::{Query, QueryResponse, Request, Response},
    NetworkAddress,
};
use std::{collections::BTreeMap, time::Duration};
use tokio::task::JoinSet;

const PERIODIC_CHECK_DELAY: Duration = Duration::from_secs(20);

pub struct FaultDetection {
    network: Network,
}

impl FaultDetection {
    pub fn new(network: Network) -> Self {
        Self { network }
    }

    pub fn run(self) {
        tokio::spawn(async move {
            loop {
                if let Err(err) = self.record_check().await {
                    error!("Record periodic check err {err:?}");
                }
                tokio::time::sleep(PERIODIC_CHECK_DELAY).await;
            }
        });
    }

    async fn record_check(&self) -> Result<(), NetworkError> {
        debug!("Record periodic check triggered");
        let our_close_group = self.network.get_our_close_group().await?;
        let our_peer_id = self.network.peer_id;
        let all_peers = self.network.get_all_local_peers().await?;
        let all_records = self.network.get_all_local_record_addresses().await?;

        let mut ask_for_proof: BTreeMap<PeerId, NetworkAddress> = Default::default();
        for key in all_records {
            if ask_for_proof.len() == our_close_group.len() {
                break;
            }
            let sorted_based_on_key =
                sort_peers_by_address(all_peers.clone(), &key, CLOSE_GROUP_SIZE)?;

            for peer in our_close_group.iter().filter(|&p| p != &our_peer_id) {
                if sorted_based_on_key.contains(peer) && !ask_for_proof.contains_key(peer) {
                    let _ = ask_for_proof.insert(*peer, key.clone());
                }
            }
        }

        let mut tasks = JoinSet::new();
        for (to_peer, address) in ask_for_proof {
            debug!("checking record {address:?} from {to_peer:?}");
            let request = Request::Query(Query::GetReplicatedData {
                requester: NetworkAddress::from_peer(self.network.peer_id),
                address,
            });

            let network_clone = self.network.clone();
            let _ = tasks.spawn(async move {
                network_clone
                    .send_and_get_responses(vec![to_peer], &request, true)
                    .await
            });
        }

        while let Some(responses) = tasks.join_next().await {
            for response in responses.into_iter().flatten() {
                if let Ok(Response::Query(QueryResponse::GetReplicatedData(Ok((
                    _,
                    replicated_data,
                ))))) = response
                {
                    trace!("got on periodic record check {replicated_data:?}");
                } else {
                    error!(
                        "Error during periodic record check: response was {:?}",
                        response
                    );
                }
            }
        }

        Ok(())
    }
}
