// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

mod common;

use bytes::Bytes;
use common::{
    get_client, node_restart,
    safenode_proto::{safe_node_client::SafeNodeClient, NodeInfoRequest, RecordAddressesRequest},
};
use eyre::{bail, eyre, Result};
use itertools::Itertools;
use libp2p::{
    kad::{KBucketKey, RecordKey},
    PeerId,
};
use rand::{rngs::OsRng, Rng};
use sn_client::{Client, Error, Files};
use sn_logging::{init_logging, LogFormat, LogOutputDest};
use sn_networking::{sort_peers_by_key, CLOSE_GROUP_SIZE};
use sn_protocol::{
    storage::{ChunkAddress, RegisterAddress},
    NetworkAddress,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{sync::RwLock, time::sleep};
use tonic::Request;
use tracing::trace;
use tracing_core::Level;
use xor_name::XorName;

const NODE_COUNT: u32 = 25;
const CHUNKS_SIZE: usize = 1024 * 1024;
const MAX_NUM_OF_QUERY_ATTEMPTS: u8 = 5;

// Default total amount of time we run the checks for before reporting the outcome.
// It can be overriden by setting the 'TEST_DURATION_MINS' env var.
const TEST_DURATION: Duration = Duration::from_secs(60 * 60); // 1hr

// Churn interval is set based on the dead peer detection interval
// This is to make sure that the we can accurately verify the data location post churn
const CHURN_INTERVAL: Duration = Duration::from_secs(5 * 60); // 5 mins
const PUT_DELAY: Duration = Duration::from_secs(5);
const GET_DELAY: Duration = Duration::from_secs(3);

type ContentList = Arc<RwLock<VecDeque<NetworkAddress>>>;

struct ContentError {
    net_addr: NetworkAddress,
    attempts: u8,
    last_err: Error,
}

impl fmt::Debug for ContentError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:?}, attempts: {}, last error: {:?}",
            self.net_addr, self.attempts, self.last_err
        )
    }
}

type ContentErredList = Arc<RwLock<BTreeMap<NetworkAddress, ContentError>>>;

#[tokio::test(flavor = "multi_thread")]
async fn data_availability_during_churn() -> Result<()> {
    let test_duration = if let Ok(str) = std::env::var("TEST_DURATION_MINS") {
        Duration::from_secs(60 * str.parse::<u64>()?)
    } else {
        TEST_DURATION
    };

    if TEST_DURATION < CHURN_INTERVAL {
        bail!("The total test duration {TEST_DURATION:?} should be greater than the churn interval {CHURN_INTERVAL:?}");
    }

    println!("Nodes will churn every {CHURN_INTERVAL:?}");

    // Create a cross thread usize for tracking churned nodes
    let churn_count = Arc::new(RwLock::new(0_usize));

    // Allow to disable Registers data creation/checks, storing and querying only Chunks during churn.
    // Default to be not carry out chunks only during churn.
    let chunks_only = std::env::var("CHUNKS_ONLY").is_ok();

    println!(
        "Running this test for {test_duration:?}{}...",
        if chunks_only { " (Chunks only)" } else { "" }
    );

    let tmp_dir = std::env::temp_dir();
    let logging_targets = vec![
        ("safenode".to_string(), Level::TRACE),
        ("sn_transfers".to_string(), Level::TRACE),
        ("sn_networking".to_string(), Level::TRACE),
        ("sn_node".to_string(), Level::TRACE),
    ];
    let log_appender_guard = init_logging(
        logging_targets,
        LogOutputDest::Path(tmp_dir.join("safe-client")),
        LogFormat::Default,
    )?;

    println!("Creating a client...");
    let client = get_client().await;
    println!("Client created with signing key: {:?}", client.signer_pk());

    // Shared bucket where we keep track of content created/stored on the network
    let content = ContentList::default();
    let verification_failed = Arc::new(AtomicBool::new(false));

    // Upload some chunks before carry out any churning.

    // Spawn a task to store Chunks at random locations, at a higher frequency than the churning events
    store_chunks_task(client.clone(), content.clone());

    // Wait for sometime _before_ we start churning, to get some data PUT on the network
    sleep(5 * PUT_DELAY).await;

    // Spawn a task to churn nodes
    churn_nodes_task(
        churn_count.clone(),
        content.clone(),
        test_duration,
        verification_failed.clone(),
    );

    // Shared bucket where we keep track of the content which erred when creating/storing/fetching.
    // We remove them from this bucket if we are then able to query/fetch them successfully.
    // We only try to query them 'MAX_NUM_OF_QUERY_ATTEMPTS' times, then report them effectivelly as failures.
    let content_erred = ContentErredList::default();

    // Shared bucket where we keep track of the content we failed to fetch for 'MAX_NUM_OF_QUERY_ATTEMPTS' times.
    let failures = ContentErredList::default();

    // Spawn a task to create Registers at random locations, at a higher frequency than the churning events
    if !chunks_only {
        create_registers_task(client.clone(), content.clone());
    }

    // Spawn a task to randomly query/fetch the content we create/store
    query_content_task(client.clone(), content.clone(), content_erred.clone());

    // Spawn a task to retry querying the content that failed, up to 'MAX_NUM_OF_QUERY_ATTEMPTS' times,
    // and mark them as failures if they effectivelly cannot be retrieved.
    retry_query_content_task(client.clone(), content_erred.clone(), failures.clone());

    let start_time = Instant::now();
    while start_time.elapsed() < test_duration {
        let failed = failures.read().await;
        println!(
            "Current failures after {:?} ({}): {:?}",
            start_time.elapsed(),
            failed.len(),
            failed.values()
        );
        sleep(PUT_DELAY * 2).await;
    }

    println!();
    println!(
        "Test stopping after running for {:?}.",
        start_time.elapsed()
    );
    println!("{:?} churn events happened.", *churn_count.read().await);
    println!();

    let failed = failures.read().await;
    if failed.len() > 0 {
        bail!("{} failure/s in test: {:?}", failed.len(), failed.values());
    }

    // The churning of storing_chunk/querying_chunk are all random,
    // which will have a high chance that newly stored chunk got queried BEFORE
    // the original holders churned out.
    // i.e. the test may pass even without any replication
    // Hence, we carry out a final round of query all data to confirm storage.
    println!("Final querying confirmation of content");
    let mut content_queried_count = 0;

    // take one read lock to avoid holding the lock for the whole loop
    // prevent any late content uploads being added to the list
    let content = content.read().await;
    let uploaded_content_count = content.len();
    for net_addr in content.iter() {
        let result = final_retry_query_content(&client, net_addr).await;
        assert!(
            result.is_ok(),
            "Failed to query content at {net_addr:?} with error {result:?}"
        );

        content_queried_count += 1;
    }

    println!("{:?} pieces of content queried", content_queried_count);

    assert_eq!(
        content_queried_count, uploaded_content_count,
        "Not all content was queried"
    );

    // check if verification failed
    if verification_failed.load(Ordering::SeqCst) {
        bail!("Verificaion failed");
    }

    drop(log_appender_guard);

    println!("Test passed after running for {:?}.", start_time.elapsed());
    Ok(())
}

// Spawns a task which periodically creates Registers at random locations.
fn create_registers_task(client: Client, content: ContentList) {
    let _handle = tokio::spawn(async move {
        loop {
            let xorname = XorName(rand::random());
            let tag = rand::random();

            let addr = RegisterAddress { name: xorname, tag };
            println!("Creating Register at {addr:?} in {PUT_DELAY:?}");
            sleep(PUT_DELAY).await;

            let mut content_write_lock = content.write().await;
            match client.create_register(xorname, tag).await {
                Ok(_) => content_write_lock.push_back(NetworkAddress::RegisterAddress(addr)),
                Err(err) => println!("Discarding new Register ({addr:?}) due to error: {err:?}"),
            }
        }
    });
}

// Spawns a task which periodically stores Chunks at random locations.
fn store_chunks_task(client: Client, content: ContentList) {
    let _handle = tokio::spawn(async move {
        let file_api = Files::new(client);
        let mut rng = OsRng;
        loop {
            let random_bytes: Vec<u8> = ::std::iter::repeat(())
                .map(|()| rng.gen::<u8>())
                .take(CHUNKS_SIZE)
                .collect();
            let bytes = Bytes::copy_from_slice(&random_bytes);

            let addr = ChunkAddress::new(
                file_api
                    .calculate_address(bytes.clone())
                    .expect("Failed to calculate new Chunk address"),
            );
            println!("Storing Chunk at {addr:?} in {PUT_DELAY:?}");
            sleep(PUT_DELAY).await;

            let mut content_write_lock = content.write().await;
            match file_api
                .upload_with_proof(bytes, &BTreeMap::default())
                .await
            {
                Ok(_) => content_write_lock.push_back(NetworkAddress::ChunkAddress(addr)),
                Err(err) => println!("Discarding new Chunk ({addr:?}) due to error: {err:?}"),
            }
        }
    });
}

// Spawns a task which periodically queries a content by randomly choosing it from the list
// of content created by another task.
fn query_content_task(client: Client, content: ContentList, content_erred: ContentErredList) {
    let _handle = tokio::spawn(async move {
        loop {
            let len = content.read().await.len();
            if len == 0 {
                println!("No content created/stored just yet, let's try in {GET_DELAY:?} ...");
                sleep(GET_DELAY).await;
                continue;
            }

            // let's choose a random content to query, picking it from the list of created
            let index = rand::thread_rng().gen_range(0..len);
            let net_addr = content.read().await[index].clone();
            trace!("Querying content (bucket index: {index}) at {net_addr:?} in {GET_DELAY:?}");
            sleep(GET_DELAY).await;

            match query_content(&client, &net_addr).await {
                Ok(_) => {
                    let _ = content_erred.write().await.remove(&net_addr);
                }
                Err(last_err) => {
                    println!(
                        "Failed to query content (index: {index}) at {net_addr:?}: {last_err:?}"
                    );
                    // mark it to try 'MAX_NUM_OF_QUERY_ATTEMPTS' times.
                    let _ = content_erred
                        .write()
                        .await
                        .entry(net_addr.clone())
                        .and_modify(|curr| curr.attempts += 1)
                        .or_insert(ContentError {
                            net_addr,
                            attempts: 1,
                            last_err,
                        });
                }
            }
        }
    });
}

// Spawns a task which periodically picks up a node, and restarts it to cause churn in the network.
// Also verifies the location of the data on the network post churn
fn churn_nodes_task(
    churn_count: Arc<RwLock<usize>>,
    content: ContentList,
    test_duration: Duration,
    verification_failed: Arc<AtomicBool>,
) {
    let start = Instant::now();
    let _handle = tokio::spawn(async move {
        let mut node_index = 1;
        let mut addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12000);
        loop {
            // break out if we've run the duration of churn
            if start.elapsed() > test_duration {
                println!("Test duration reached, stopping churn nodes task");
                break;
            }

            addr.set_port(12000 + node_index);

            println!("Restarting node through its RPC service at {addr}");

            if let Err(err) = node_restart(addr).await {
                println!("Failed to restart node with RPC endpoint {addr}: {err}");
                continue;
            }

            *churn_count.write().await += 1;

            node_index += 1;
            if node_index > NODE_COUNT as u16 {
                node_index = 1;
            }

            sleep(CHURN_INTERVAL).await;
            // read lock to make sure we don't put new data while verification is being performed
            let _read_lock = content.read().await;
            // verify data location
            let stored_records = match get_peers_and_records().await {
                Ok(stored_records) => stored_records,
                Err(err) => {
                    println!("Failed to get peers and their records addresses {err:?}");
                    verification_failed.store(true, Ordering::SeqCst);
                    continue;
                }
            };
            if let Err(err) = verify_location(&stored_records, content.clone()).await {
                println!("Failed to verify location {err:?}");
                verification_failed.store(true, Ordering::SeqCst);
                continue;
            }
        }
    });
}

// Checks (periodically) for any content that an error was reported either at the moment of its creation or
// in a later query attempt.
fn retry_query_content_task(
    client: Client,
    content_erred: ContentErredList,
    failures: ContentErredList,
) {
    let _handle = tokio::spawn(async move {
        let delay = 10 * GET_DELAY;
        loop {
            sleep(delay).await;

            // let's try to query from the bucket of those that erred upon creation/query
            let erred = content_erred.write().await.pop_first();

            if let Some((net_addr, mut content_error)) = erred {
                let attempts = content_error.attempts + 1;

                println!("Querying erred content at {net_addr:?}, attempt: #{attempts} ...");
                if let Err(last_err) = query_content(&client, &net_addr).await {
                    println!("Erred content is still not retrievable at {net_addr:?} after {attempts} attempts: {last_err:?}");
                    // We only keep it to retry 'MAX_NUM_OF_QUERY_ATTEMPTS' times,
                    // otherwise report it effectivelly as failure.
                    content_error.attempts = attempts;
                    content_error.last_err = last_err;

                    if attempts == MAX_NUM_OF_QUERY_ATTEMPTS {
                        let _ = failures.write().await.insert(net_addr, content_error);
                    } else {
                        let _ = content_erred.write().await.insert(net_addr, content_error);
                    }
                }
            }
        }
    });
}

async fn final_retry_query_content(client: &Client, net_addr: &NetworkAddress) -> Result<()> {
    let mut attempts = 1;
    loop {
        println!("Querying content at {net_addr:?}, attempt: #{attempts} ...");
        if let Err(last_err) = query_content(client, net_addr).await {
            if attempts == MAX_NUM_OF_QUERY_ATTEMPTS {
                bail!("Final check: Content is still not retrievable at {net_addr:?} after {attempts} attempts: {last_err:?}");
            } else {
                attempts += 1;
                let delay = 10 * GET_DELAY;
                sleep(delay).await;
                continue;
            }
        } else {
            // content retrieved fine
            return Ok(());
        }
    }
}

async fn get_peers_and_records() -> Result<HashMap<PeerId, HashSet<RecordKey>>> {
    let mut result = HashMap::new();
    for node_index in 1..NODE_COUNT + 1 {
        println!("getting addresses for {node_index:?}");
        let mut addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12000);
        addr.set_port(12000 + node_index as u16);
        let endpoint = format!("https://{addr}");
        let mut rpc_client = SafeNodeClient::connect(endpoint).await?;

        // get the record addresses
        let response = rpc_client
            .record_addresses(Request::new(RecordAddressesRequest {}))
            .await?;
        #[allow(clippy::mutable_key_type)]
        let keys = response
            .get_ref()
            .addresses
            .iter()
            .cloned()
            .map(RecordKey::from)
            .collect();

        // get the peer_id
        let response = rpc_client
            .node_info(Request::new(NodeInfoRequest {}))
            .await?;
        let peer_id = PeerId::from_bytes(&response.get_ref().peer_id)?;
        result.insert(peer_id, keys);
    }
    Ok(result)
}

// Verfies that the chunk is stored by the actual closest peers to the RecordKey
async fn verify_location(
    stored_records: &HashMap<PeerId, HashSet<RecordKey>>,
    content: ContentList,
) -> Result<()> {
    #[allow(clippy::mutable_key_type)]
    let all_peers = stored_records.keys().cloned().collect_vec();
    for addr in content.read().await.iter() {
        let key = RecordKey::new(&addr.as_bytes());
        println!("Verifying {addr:?}");
        let record_key = KBucketKey::from(key.to_vec());
        let expected_closest_peers =
            sort_peers_by_key(all_peers.to_vec(), &record_key, CLOSE_GROUP_SIZE)?
                .into_iter()
                .collect::<HashSet<_>>();
        for expected in &expected_closest_peers {
            if !stored_records
                .get(expected)
                .ok_or_else(|| {
                    eyre!("Expected peer {expected}, should be present in stored_records")
                })?
                .contains(&key)
            {
                return Err(eyre!("Record {addr:?} is not stored inside {expected:?}\nstored_records {stored_records:?}"));
            }
        }
    }
    Ok(())
}

async fn query_content(client: &Client, net_addr: &NetworkAddress) -> Result<(), Error> {
    match net_addr {
        NetworkAddress::RegisterAddress(addr) => {
            let _ = client.get_register(*addr.name(), addr.tag()).await?;
            Ok(())
        }
        NetworkAddress::ChunkAddress(addr) => {
            let file_api = Files::new(client.clone());
            let _ = file_api.read_bytes(*addr).await?;
            Ok(())
        }
        _other => Ok(()), // we don't create/store any other type of content in this test yet
    }
}
