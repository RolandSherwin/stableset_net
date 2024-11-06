// Copyright 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use bytes::Bytes;
use eyre::Result;
use libp2p::{identity::Keypair, kad::Record, PeerId};
use sn_logging::LogBuilder;
use sn_networking::{GetRecordCfg, LocalSwarmCmd, NetworkBuilder, NetworkSwarmCmd};
use sn_protocol::{
    messages::{Cmd, Query, QueryResponse, Request, Response},
    storage::{try_serialize_record, Chunk, RecordKind, RecordType, RetryStrategy},
    NetworkAddress, PrettyPrintRecordKey,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use test_utils::gen_random_data;

#[tokio::test]
async fn unpaid_data() -> Result<()> {
    let _log_appender_guard = LogBuilder::init_multi_threaded_tokio_test("unpaid_data", false);

    let temp_dir = tempfile::tempdir()?;
    let keypair = Keypair::generate_ed25519();
    let self_peer_id = PeerId::from(keypair.public());

    println!("Our peer id: {self_peer_id:?}");

    let mut network_builder = NetworkBuilder::new(keypair, true);
    network_builder.listen_addr(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0));

    let (_network, mut network_event_receiver, swarm_driver) =
        network_builder.build_node(temp_dir.path().to_path_buf())?;

    let network_cmd_sender = swarm_driver.network_cmd_sender.clone();
    let local_cmd_sender = swarm_driver.local_cmd_sender.clone();

    let _handle = tokio::spawn(swarm_driver.run());

    // wait for enough peers to be added
    let mut peer_count = 0;
    loop {
        if let Some(sn_networking::NetworkEvent::PeerAdded(peer_id, _)) =
            network_event_receiver.recv().await
        {
            peer_count += 1;
            if peer_count > 10 {
                break;
            }
        }
    }

    let (closest_peers_tx, closest_peers_rx) = tokio::sync::oneshot::channel();
    local_cmd_sender
        .send(LocalSwarmCmd::GetClosestKLocalPeers {
            sender: closest_peers_tx,
        })
        .await?;
    let mut our_closest_peers = closest_peers_rx.await?;
    // remove our peer_id
    let _ = our_closest_peers.remove(0);
    let random_closest_peer = our_closest_peers[0];

    println!("Sleeping for a second to let the network settle");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Generate a random chunk that is closest to majority of our closest peers
    let close_chunk = loop {
        println!("Generating a random chunk that is close to majority of our closest peers");
        let close_data = gen_random_data(1024);
        let random_chunk = Chunk::new(close_data);
        let chunk_address = random_chunk.network_address();

        // sort the peers by distance to the chunk address
        let mut sorted_peers = our_closest_peers.clone();
        sorted_peers.sort_by_key(|peer| chunk_address.distance(&NetworkAddress::from_peer(*peer)));

        // check if the top 5 peers that are close to us are also close to the chunk address
        let mut is_chunk_close = true;
        for peer in sorted_peers.iter().take(5) {
            if let Some(position) = our_closest_peers.iter().position(|p| p == peer) {
                if position > 5 {
                    is_chunk_close = false;
                }
            }
        }
        if is_chunk_close {
            break random_chunk;
        }
    };

    let key = close_chunk.network_address().to_record_key();
    let record = Record {
        value: try_serialize_record(&close_chunk, RecordKind::Chunk)?.to_vec(),
        key: key.clone(),
        publisher: None,
        expires: None,
    };

    let pretty_key = PrettyPrintRecordKey::from(&key);
    println!("Storing the record {pretty_key:?} in local store");

    local_cmd_sender
        .send(LocalSwarmCmd::PutLocalRecord {
            record: record.clone(),
        })
        .await?;

    println!("Stored the record in local store. Sleeping for a second to let it write to fs.");
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let (oneshot_sender, oneshot_receiver) = tokio::sync::oneshot::channel();
    network_cmd_sender
        .send(NetworkSwarmCmd::SendRequest {
            req: Request::Cmd(Cmd::Replicate {
                holder: NetworkAddress::from_peer(self_peer_id),
                keys: vec![(close_chunk.network_address(), RecordType::Chunk)],
            }),

            peer: random_closest_peer,
            sender: Some(oneshot_sender),
        })
        .await?;

    println!("Sent replicate cmd to {random_closest_peer:?} for the random key: {pretty_key:?}");
    let resp = oneshot_receiver.await??;
    println!("Got response from the random peer: {resp:?}",);

    loop {
        if let Some(sn_networking::NetworkEvent::QueryRequestReceived { query, channel }) =
            network_event_receiver.recv().await
        {
            println!("Received a query: {query:?}");
            if let Query::GetReplicatedRecord { requester, key } = query {
                println!(
                    "Received GetReplicatedRecord query from {requester:?} for the key: {key:?}"
                );
                let resp = Response::Query(QueryResponse::GetReplicatedRecord(Ok((
                    NetworkAddress::from_peer(self_peer_id),
                    Bytes::from(record.value.clone()),
                ))));
                network_cmd_sender
                    .send(NetworkSwarmCmd::SendResponse { resp, channel })
                    .await?;
                break;
            }
        }
    }

    println!("Responded to GetReplicatedRecord query. Sleeping 60s before fetching from network");
    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;

    local_cmd_sender
        .send(LocalSwarmCmd::RemoveFailedLocalRecord { key: key.clone() })
        .await?;

    println!("Removed the record from local store. Now try to fetch it from network");

    let mut retry_count = 0;
    loop {
        let (oneshot_sender, oneshot_receiver) = tokio::sync::oneshot::channel();
        network_cmd_sender
            .send(NetworkSwarmCmd::GetNetworkRecord {
                key: key.clone(),
                sender: oneshot_sender,
                cfg: GetRecordCfg {
                    // to see if others have the record with them
                    get_quorum: libp2p::kad::Quorum::Majority,
                    retry_strategy: Some(RetryStrategy::Persistent),
                    target_record: None,
                    expected_holders: Default::default(),
                    is_register: false,
                },
            })
            .await?;

        let result = oneshot_receiver.await?;
        if result.is_err() {
            retry_count += 1;
            println!("Failed to fetch the record from network. Sleeping for 10s before trying again. Retry count: {retry_count} {result:?}");
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

            if retry_count > 10 {
                println!("Failed to fetch the record from network after 10 retries: {result:?}");
                break;
            }
        } else {
            panic!("We fetched a unpaid record from network");
        }
    }

    Ok(())
}
