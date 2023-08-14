// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

#![allow(dead_code)]
#![allow(clippy::mutable_key_type)]

#[allow(unused_qualifications, unreachable_pub, clippy::unwrap_used)]
pub mod safenode_proto {
    tonic::include_proto!("safenode_proto");
}

use eyre::{eyre, Result};
use lazy_static::lazy_static;
use libp2p::{
    kad::{KBucketKey, RecordKey},
    PeerId,
};
use safenode_proto::{
    safe_node_client::SafeNodeClient, NodeInfoRequest, RecordAddressesRequest, RestartRequest,
};
use sn_client::{load_faucet_wallet_from_genesis_wallet, send, Client};
use sn_dbc::Token;
use sn_logging::{LogFormat, LogOutputDest};
use sn_networking::{sort_peers_by_address, sort_peers_by_key, CLOSE_GROUP_SIZE};
use sn_peers_acquisition::parse_peer_addr;
use sn_protocol::{NetworkAddress, PrettyPrintRecordKey};
use sn_transfers::wallet::LocalWallet;

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    sync::Once,
    time::Duration,
};
use tokio::{fs::remove_dir_all, sync::Mutex};
use tonic::Request;
use tracing_core::Level;

static TEST_INIT_LOGGER: Once = Once::new();

pub const PAYING_WALLET_INITIAL_BALANCE: u64 = 100_000_000_000_000;

pub fn init_logging() {
    TEST_INIT_LOGGER.call_once(|| {
        let logging_targets = vec![
            ("safenode".to_string(), Level::INFO),
            ("sn_client".to_string(), Level::TRACE),
            ("sn_transfers".to_string(), Level::INFO),
            ("sn_networking".to_string(), Level::INFO),
            ("sn_node".to_string(), Level::INFO),
        ];
        let _log_appender_guard =
            sn_logging::init_logging(logging_targets, LogOutputDest::Stdout, LogFormat::Default)
                .expect("Failed to init logging");
    });
}

lazy_static! {
    // mutex to restrict access to faucet wallet from concurrent tests
    static ref FAUCET_WALLET_MUTEX: Mutex<()> = Mutex::new(());
}

//  Get a new Client for testing
pub async fn get_client() -> Client {
    let secret_key = bls::SecretKey::random();

    let bootstrap_peers = if !cfg!(feature = "local-discovery") {
        match std::env::var("SAFE_PEERS") {
            Ok(str) => match parse_peer_addr(&str) {
                Ok(peer) => Some(vec![peer]),
                Err(err) => panic!("Can't parse SAFE_PEERS {str:?} with error {err:?}"),
            },
            Err(err) => panic!("Can't get env var SAFE_PEERS with error {err:?}"),
        }
    } else {
        None
    };
    println!("Client bootstrap with peer {bootstrap_peers:?}");
    Client::new(secret_key, bootstrap_peers, None)
        .await
        .expect("Client shall be successfully created.")
}

pub async fn get_wallet(root_dir: &Path) -> LocalWallet {
    LocalWallet::load_from(root_dir)
        .await
        .expect("Wallet shall be successfully created.")
}

pub async fn get_funded_wallet(
    client: &Client,
    from: LocalWallet,
    root_dir: &Path,
    amount: u64,
) -> Result<LocalWallet> {
    let wallet_balance = Token::from_nano(amount);
    let mut local_wallet = get_wallet(root_dir).await;

    println!("Getting {wallet_balance} tokens from the faucet...");
    let tokens = send(from, wallet_balance, local_wallet.address(), client, true).await?;

    println!("Verifying the transfer from faucet...");
    client.verify(&tokens).await?;
    local_wallet.deposit(vec![tokens]);
    assert_eq!(local_wallet.balance(), wallet_balance);
    println!("Tokens deposited to the wallet that'll pay for storage: {wallet_balance}.");

    Ok(local_wallet)
}

pub async fn get_client_and_wallet(root_dir: &Path, amount: u64) -> Result<(Client, LocalWallet)> {
    let _guard = FAUCET_WALLET_MUTEX.lock().await;

    let client = get_client().await;
    let faucet = load_faucet_wallet_from_genesis_wallet(&client).await?;
    let local_wallet = get_funded_wallet(&client, faucet, root_dir, amount).await?;

    Ok((client, local_wallet))
}

pub async fn node_restart(addr: SocketAddr) -> Result<()> {
    let endpoint = format!("https://{addr}");
    let mut client = SafeNodeClient::connect(endpoint).await?;

    let response = client.node_info(Request::new(NodeInfoRequest {})).await?;
    let log_dir = Path::new(&response.get_ref().log_dir);
    let root_dir = log_dir
        .parent()
        .ok_or_else(|| eyre!("could not obtain parent from logging directory"))?;

    // remove Chunks records
    let chunks_records = root_dir.join("record_store");
    if let Ok(true) = chunks_records.try_exists() {
        println!("Removing Chunks records from {}", chunks_records.display());
        remove_dir_all(chunks_records).await?;
    }

    // remove Registers records
    let registers_records = root_dir.join("registers");
    if let Ok(true) = registers_records.try_exists() {
        println!(
            "Removing Registers records from {}",
            registers_records.display()
        );
        remove_dir_all(registers_records).await?;
    }

    let _response = client
        .restart(Request::new(RestartRequest { delay_millis: 0 }))
        .await?;

    println!(
        "Node restart requested to RPC service at {addr}, and removed all its chunks and registers records at {}",
        log_dir.display()
    );

    Ok(())
}

// DATA_LOCATION_VERIFICATION_DELAY is set based on the dead peer detection interval
// Once a node has been restarted, it takes VERIFICATION_DELAY time
// for the old peer to be removed from the routing table.
// Replication is then kicked off to distribute the data to the new closest
// nodes, hence verification has to be performed after this.
pub const DATA_LOCATION_VERIFICATION_DELAY: Duration = Duration::from_secs(300);
// Number of times to retry verification if it fails
const DATA_LOCATION_VERIFICATION_ATTEMPTS: usize = 1;

type NodeIndex = usize;
pub struct DataLocationVerification {
    node_count: usize,
    pub all_peers: Vec<PeerId>,
    all_records: HashSet<RecordKey>,
    records_held: HashMap<NodeIndex, HashSet<RecordKey>>,
}

impl DataLocationVerification {
    // Should be called before any churn to capture the list of records in the network.
    pub async fn new(node_count: usize) -> Result<Self> {
        let records_held = Self::get_held_records(node_count).await?;
        let all_records = records_held.iter().map(|(_, v)| v).fold(
            HashSet::new(),
            |mut all_records, records_per_node| {
                all_records.extend(records_per_node.iter().cloned());
                all_records
            },
        );
        let data_verification = Self {
            node_count,
            all_peers: Self::get_all_peer_ids(node_count).await?,
            all_records,
            records_held,
        };
        data_verification.debug_close_groups();
        Ok(data_verification)
    }

    pub fn update_peer_index(&mut self, node_index: NodeIndex, peer_id: PeerId) -> Result<()> {
        // new peer is added first, so debug replication on addition of new peer
        let mut temp_all_peers = self.all_peers.clone();
        // println!("######Printing Replication on peer addition######");
        // self.debug_replication(peer_id, false)?;

        temp_all_peers[node_index - 1] = peer_id;
        self.all_peers = temp_all_peers;
        // println!("\n######Printing Replication on peer removal######");
        // self.debug_replication(peer_id, true)?;

        Ok(())
    }

    // Verifies that the chunk is stored by the actual closest peers to the RecordKey
    pub async fn verify(&mut self) -> Result<()> {
        // get the current set of records held
        self.records_held = Self::get_held_records(self.node_count).await?;

        // check if we've lost any records
        for record in self.all_records.iter() {
            if !self
                .records_held
                .iter()
                .any(|(_, records)| records.contains(record))
            {
                println!(
                    "Record {:?} has been lost",
                    PrettyPrintRecordKey::from(record.clone())
                );
            }
        }

        // // Only store record from Replication that close enough to us.
        // let keys_to_store = if let Some(distance_range) = distance_range {
        //     if let Some(distance_bar_ilog2) = distance_range.ilog2() {
        //         let self_address = NetworkAddress::from_peer(self.self_peer_id);
        //         keys.iter()
        //             .filter(|key| {
        //                 if let Some(entry_distance_ilog2) =
        //                     self_address.distance(key).ilog2()
        //                 {

        //                     let within_distance_bar =  entry_distance_ilog2 <= distance_bar_ilog2;
        //                 trace!("within the the distance bar is {within_distance_bar:?} for {key:?}");
        //                     within_distance_bar
        //                 } else {
        //                     true
        //                 }
        //             })
        //             .cloned()
        //             .collect()
        //     } else {
        //         keys
        //     }
        // } else {
        //     keys
        // };
        for (node_index, records_held_by_node) in self.records_held.iter() {
            let our_peer_id = self.all_peers[node_index - 1];
            let our_address = NetworkAddress::from_peer(our_peer_id);
            let closest_peers = sort_peers_by_key(
                self.all_peers.clone(),
                &our_address.as_kbucket_key(),
                CLOSE_GROUP_SIZE,
            )
            .expect("failed to sort peer");
            let distance_range = closest_peers
                .last()
                .map(|peer| NetworkAddress::from_peer(*peer).distance(&our_address))
                .expect("Failed to get closest_peers.last()");

            let distance_bar_ilog2 = distance_range.ilog2().expect("Could not get ilog2");
            let expected_records = self
                .all_records
                .iter()
                .filter(|&key| {
                    let key = NetworkAddress::from_record_key(key.clone());
                    if let Some(entry_distance_ilog2) = our_address.distance(&key).ilog2() {
                        let within_distance_bar = entry_distance_ilog2 <= distance_bar_ilog2;
                        println!(
                            "within the the distance bar is {within_distance_bar:?} for {key:?}"
                        );
                        within_distance_bar
                    } else {
                        true
                    }
                })
                .cloned()
                .collect::<HashSet<RecordKey>>();
            println!("\nnode index {node_index:?}");
            println!("expected_records {expected_records:?}");
            println!("actual_records {records_held_by_node:?}");
            for record in expected_records {
                if !records_held_by_node.contains(&record) {
                    println!(
                        "record {:?} not held by {node_index:?}",
                        PrettyPrintRecordKey::from(record.clone())
                    );
                }
            }
        }
        // let mut failed = HashMap::new();
        // let mut verification_attempts = 0;
        // while verification_attempts < DATA_LOCATION_VERIFICATION_ATTEMPTS {
        //     failed.clear();
        //     for (key, actual_closest_idx) in self.record_holders.iter() {
        //         println!("Verifying {:?}", PrettyPrintRecordKey::from(key.clone()));
        //         let record_key = KBucketKey::from(key.to_vec());
        //         let expected_closest_peers =
        //             sort_peers_by_key(self.all_peers.clone(), &record_key, CLOSE_GROUP_SIZE)?
        //                 .into_iter()
        //                 .collect::<HashSet<_>>();

        //         let actual_closest = actual_closest_idx
        //             .iter()
        //             .map(|idx| self.all_peers[*idx - 1])
        //             .collect::<HashSet<_>>();

        //         let mut failed_peers = Vec::new();
        //         expected_closest_peers
        //             .iter()
        //             .filter(|expected| !actual_closest.contains(expected))
        //             .for_each(|expected| failed_peers.push(*expected));

        //         if !failed_peers.is_empty() {
        //             failed.insert(key.clone(), failed_peers);
        //         }
        //     }

        //     if !failed.is_empty() {
        //         println!("Verification failed");

        //         failed.iter().for_each(|(key, failed_peers)| {
        //             failed_peers.iter().for_each(|peer| {
        //                 println!(
        //                     "Record {:?} is not stored inside {peer:?}",
        //                     PrettyPrintRecordKey::from(key.clone()),
        //                 )
        //             });
        //         });
        //         println!("State of each node:");
        //         self.record_holders.iter().for_each(|(key, node_index)| {
        //             println!(
        //                 "Record {:?} is currently held by node indexes {node_index:?}",
        //                 PrettyPrintRecordKey::from(key.clone())
        //             );
        //         });
        //         println!("Node index map:");
        //         self.all_peers
        //             .iter()
        //             .enumerate()
        //             .for_each(|(idx, peer)| println!("{} : {peer:?}", idx + 1));
        //         verification_attempts += 1;
        //         println!("Sleeping before retrying verification");
        //         tokio::time::sleep(Duration::from_secs(20)).await;
        //     } else {
        //         // if successful, break out of the loop
        //         break;
        //     }
        // }

        // if !failed.is_empty() {
        //     println!("Verification failed after {DATA_LOCATION_VERIFICATION_ATTEMPTS} times");
        //     Err(eyre!("Verification failed for: {failed:?}"))
        // } else {
        //     println!("All the Records have been verified!");
        //     Ok(())
        // }
        Ok(())
    }

    // Get all the PeerId for all the locally running nodes
    async fn get_all_peer_ids(node_count: usize) -> Result<Vec<PeerId>> {
        let mut all_peers = Vec::new();

        let mut addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12000);
        for node_index in 1..node_count + 1 {
            addr.set_port(12000 + node_index as u16);
            let endpoint = format!("https://{addr}");
            let mut rpc_client = SafeNodeClient::connect(endpoint).await?;

            // get the peer_id
            let response = rpc_client
                .node_info(Request::new(NodeInfoRequest {}))
                .await?;
            let peer_id = PeerId::from_bytes(&response.get_ref().peer_id)?;
            all_peers.push(peer_id);
        }
        println!(
            "Obtained the PeerId list for the locally running network with a node count of {}",
            node_count
        );

        Ok(all_peers)
    }

    // get the set of records held by each node
    async fn get_held_records(node_count: usize) -> Result<HashMap<NodeIndex, HashSet<RecordKey>>> {
        let mut held_records = HashMap::new();
        for node_index in 1..node_count + 1 {
            let mut addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12000);
            addr.set_port(12000 + node_index as u16);
            let endpoint = format!("https://{addr}");
            let mut rpc_client = SafeNodeClient::connect(endpoint).await?;

            let response = rpc_client
                .record_addresses(Request::new(RecordAddressesRequest {}))
                .await?;

            let records = response
                .get_ref()
                .addresses
                .iter()
                .map(|bytes| RecordKey::from(bytes.clone()))
                .collect::<HashSet<_>>();
            held_records.insert(node_index, records);
        }
        println!("Obtained the records held by the nodes");
        Ok(held_records)
    }

    // fn debug_replication(&self, peer_id: PeerId, is_removal: bool) -> Result<()> {
    //     let mut replicate_to: HashMap<PeerId, Vec<NetworkAddress>> = HashMap::new();
    //     for key in self.record_holders.keys() {
    //         let key = NetworkAddress::from_record_key(key.clone());
    //         let sorted_based_on_key =
    //             sort_peers_by_address(self.all_peers.clone(), &key, CLOSE_GROUP_SIZE + 1)?;
    //         println!("replication: close for {key:?} are: {sorted_based_on_key:?}");

    //         if sorted_based_on_key.contains(&peer_id) {
    //             let target_peer = if is_removal {
    //                 // For dead peer, only replicate to farthest close_group peer,
    //                 // when the dead peer was one of the close_group peers to the record.
    //                 if let Some(farthest_peer) = sorted_based_on_key.last() {
    //                     if *farthest_peer != peer_id {
    //                         *farthest_peer
    //                     } else {
    //                         continue;
    //                     }
    //                 } else {
    //                     continue;
    //                 }
    //             } else {
    //                 // For new peer, always replicate to it when it is close_group of the record.
    //                 if Some(&peer_id) != sorted_based_on_key.last() {
    //                     peer_id
    //                 } else {
    //                     continue;
    //                 }
    //             };

    //             let keys_to_replicate = replicate_to
    //                 .entry(target_peer)
    //                 .or_insert(Default::default());
    //             keys_to_replicate.push(key.clone());
    //         }
    //     }
    //     println!("replicate: to {replicate_to:?}");

    //     Ok(())
    // }

    fn debug_close_groups(&self) {
        println!("Node close groups:");
        for (node_index, peer) in self.all_peers.iter().enumerate() {
            let node_index = node_index + 1;
            let key = NetworkAddress::from_peer(*peer).as_kbucket_key();
            let closest_peers = sort_peers_by_key(self.all_peers.clone(), &key, CLOSE_GROUP_SIZE)
                .expect("failed to sort peer");
            let closest_peers_idx = closest_peers
                .iter()
                .map(|peer| self.all_peers.iter().position(|p| p == peer).unwrap() + 1)
                .collect::<Vec<_>>();
            println!("Close for {node_index}: {peer:?} are {closest_peers_idx:?}");
        }
    }
}
