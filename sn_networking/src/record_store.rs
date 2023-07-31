// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.
use crate::event::NetworkEvent;
use libp2p::{
    identity::PeerId,
    kad::{
        record::{Key, ProviderRecord, Record},
        store::{Error, RecordStore, Result},
        KBucketDistance as Distance, KBucketKey,
    },
};
use rand::Rng;
use sn_dbc::Token;
use sn_protocol::{NetworkAddress, PrettyPrintRecordKey};
use std::{
    borrow::Cow,
    collections::{hash_set, HashSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
    vec,
};
use tokio::sync::mpsc;
use xor_name::XorName;

// Each node will have a replication interval between these bounds
// This should serve to stagger the intense replication activity across the network
pub const REPLICATION_INTERVAL_UPPER_BOUND: Duration = Duration::from_secs(540);
pub const REPLICATION_INTERVAL_LOWER_BOUND: Duration = Duration::from_secs(180);

/// Max number of records a node can store
const MAX_RECORDS_COUNT: usize = 2048;

/// How frequenly we prune the records in the store
/// eg, if 100 here, every 100 valid PUTs we'll prune the store
/// This should give us some margin about storing data for nodes that are no longer our responsibility
/// And may eventually feed into price discovery / storage costs and give us a buffer there too
const PRUNE_ONCE_EVERY_PUTS: usize = 100;

/// A `RecordStore` that stores records on disk.
pub struct DiskBackedRecordStore {
    /// The identity of the peer owning the store.
    local_key: KBucketKey<PeerId>,
    /// The configuration of the store.
    config: DiskBackedRecordStoreConfig,
    /// A set of keys, each corresponding to a data `Record` stored on disk.
    records: HashSet<Key>,
    /// Distance range specify the acceptable range of record entry.
    /// `None` means accept all.
    distance_range: Option<Distance>,
    /// Currently only used to notify the record received via network put to be validated.
    event_sender: Option<mpsc::Sender<NetworkEvent>>,
}

/// Configuration for a `DiskBackedRecordStore`.
#[derive(Debug, Clone)]
pub struct DiskBackedRecordStoreConfig {
    /// The directory where the records are stored.
    pub storage_dir: PathBuf,
    /// The maximum number of records.
    pub max_records: usize,
    /// The maximum size of record values, in bytes.
    pub max_value_bytes: usize,
    /// This node's replication interval
    /// Which should be between REPLICATION_INTERVAL_LOWER_BOUND and REPLICATION_INTERVAL_UPPER_BOUND
    pub replication_interval: Duration,
}

impl Default for DiskBackedRecordStoreConfig {
    fn default() -> Self {
        // get a random integer between REPLICATION_INTERVAL_LOWER_BOUND and REPLICATION_INTERVAL_UPPER_BOUND
        let replication_interval = rand::thread_rng()
            .gen_range(REPLICATION_INTERVAL_LOWER_BOUND..REPLICATION_INTERVAL_UPPER_BOUND);

        Self {
            storage_dir: std::env::temp_dir(),
            max_records: MAX_RECORDS_COUNT,
            max_value_bytes: 65 * 1024,
            replication_interval,
        }
    }
}

impl DiskBackedRecordStore {
    /// Creates a new `DiskBackedStore` with the given configuration.
    pub fn with_config(
        local_id: PeerId,
        config: DiskBackedRecordStoreConfig,
        event_sender: Option<mpsc::Sender<NetworkEvent>>,
    ) -> Self {
        DiskBackedRecordStore {
            local_key: KBucketKey::from(local_id),
            config,
            records: Default::default(),
            distance_range: None,
            event_sender,
        }
    }

    /// Returns `true` if the `Key` is present locally
    pub fn contains(&self, key: &Key) -> bool {
        self.records.contains(key)
    }

    /// Retains the records satisfying a predicate.
    pub fn retain<F>(&mut self, predicate: F)
    where
        F: Fn(&Key) -> bool,
    {
        let to_be_removed = self
            .records
            .iter()
            .filter(|k| !predicate(k))
            .cloned()
            .collect::<Vec<_>>();

        to_be_removed.iter().for_each(|key| self.remove(key));
    }

    // Converts a Key into a Hex string.
    fn key_to_hex(key: &Key) -> String {
        let key_bytes = key.as_ref();
        let mut hex_string = String::with_capacity(key_bytes.len() * 2);
        for byte in key_bytes {
            hex_string.push_str(&format!("{:02x}", byte));
        }
        hex_string
    }

    pub fn record_addresses(&self) -> HashSet<NetworkAddress> {
        self.records
            .iter()
            .map(|record_key| NetworkAddress::from_record_key(record_key.clone()))
            .collect()
    }

    #[allow(clippy::mutable_key_type)]
    pub fn record_addresses_ref(&self) -> &HashSet<Key> {
        &self.records
    }

    pub fn read_from_disk<'a>(key: &Key, storage_dir: &Path) -> Option<Cow<'a, Record>> {
        let filename = Self::key_to_hex(key);
        let file_path = storage_dir.join(&filename);

        match fs::read(file_path) {
            Ok(value) => {
                trace!("Retrieved record from disk! filename: {filename}");
                let record = Record {
                    key: key.clone(),
                    value,
                    publisher: None,
                    expires: None,
                };
                Some(Cow::Owned(record))
            }
            Err(err) => {
                error!("Error while reading file. filename: {filename}, error: {err:?}");
                None
            }
        }
    }

    pub fn put_verified(&mut self, r: Record) -> Result<()> {
        let content_hash = XorName::from_content(&r.value);
        trace!(
            "PUT a verified Record: {:?} (content_hash {content_hash:?})",
            r.key
        );
        if r.value.len() >= self.config.max_value_bytes {
            warn!(
                "Record not stored. Value too large: {} bytes",
                r.value.len()
            );
            return Err(Error::ValueTooLarge);
        }

        self.prune_storage_if_needed()?;

        let filename = Self::key_to_hex(&r.key);
        let file_path = self.config.storage_dir.join(&filename);
        let _ = self.records.insert(r.key);

        // TODO: How could we clean up records if we fail to insert?
        tokio::spawn(async move {
            match fs::write(file_path, r.value) {
                Ok(_) => {
                    trace!("Wrote record to disk! filename: {filename}");
                }
                Err(err) => {
                    error!("Error writing file. filename: {filename}, error: {err:?}");
                }
            }
        });

        Ok(())
    }

    /// Prune the records in the store to ensure that we free up space
    /// for new records.
    ///
    /// This can happen if we're full, or once in every X PUTs
    ///
    /// An error is returned if we prune and we're _still_ at max_record count
    fn prune_storage_if_needed(&mut self) -> Result<()> {
        let num_records = self.records.len();
        let mut should_prune = num_records >= self.config.max_records;
        if !should_prune {
            // check the modulo of record count over 10, if 0 we should prune
            should_prune = num_records % PRUNE_ONCE_EVERY_PUTS == 0;
        }

        if should_prune {
            self.prune_storage();
            let new_num_records: usize = self.records.len();
            trace!("A pruning reduced number of record in hold from {num_records} to {new_num_records}");
            if new_num_records >= self.config.max_records {
                warn!("Record not stored. Maximum number of records reached. Current num_records: {num_records}");
                return Err(Error::MaxRecords);
            }
        }

        Ok(())
    }

    /// Remove records that are no longer our responsibility
    fn prune_storage(&mut self) {
        if let Some(distance_bar) = self.distance_range {
            let our_kbucket_key = self.local_key.clone();
            let predicate = |key: &Key| {
                let kbucket_key = KBucketKey::from(key.to_vec());
                our_kbucket_key.distance(&kbucket_key) < distance_bar
            };
            self.retain(predicate);
        } else {
            warn!("Record storage didn't have the distance_range setup yet.");
        }
    }

    #[allow(dead_code)]
    /// Calculate the cost to store data for our current store state
    pub fn store_cost(&self) -> Token {
        let records_len = self.records().count();

        let mut cost = Token::from_nano(1);
        const NANOS_PER_SNT: u64 = 1_000_000_000;

        // if we imagine that at FULL capacity we want _all the money_ (ie, we never want to reach full capacity)
        // So we get the total amount of nanos in the network,
        // we divide that number by our MAX_RECORD_COUNT to know
        // what to charge for each additional record
        let step_per_record = ((2 ^ 32) * NANOS_PER_SNT) / MAX_RECORDS_COUNT as u64;
        let step_amount = Token::from_nano(step_per_record);

        // we want to spread the cost of storing data across the MAX_CAPACITY
        // doubling the cost for each record store
        for _ in 0..records_len {
            if let Some(new_cost) = cost.checked_add(step_amount) {
                cost = new_cost;
            }
        }

        trace!("Cost is now {cost:?}");
        cost
    }

    /// Setup the distance range.
    pub fn set_distance_range(&mut self, distance_range: Distance) {
        self.distance_range = Some(distance_range);
    }
}

impl RecordStore for DiskBackedRecordStore {
    type RecordsIter<'a> = RecordsIterator<'a>;
    type ProvidedIter<'a> = vec::IntoIter<Cow<'a, ProviderRecord>>;

    fn get(&self, k: &Key) -> Option<Cow<'_, Record>> {
        // When a client calls GET, the request is forwarded to the nodes until one node returns
        // with the record. Thus a node can be bombarded with GET reqs for random keys. These can be safely
        // ignored if we don't have the record locally.
        trace!("GET request for Record key: {k:?}");
        if !self.records.contains(k) {
            trace!("Record not found locally");
            return None;
        }

        Self::read_from_disk(k, &self.config.storage_dir)
    }

    fn put(&mut self, record: Record) -> Result<()> {
        if self.records.contains(&record.key) {
            trace!(
                "Unverified Record {:?} already exists.",
                PrettyPrintRecordKey::from(record.key.clone())
            );
            // Blindly sent to validation to allow double spend can be detected.
            // TODO: consider avoid throw duplicated chunk to validation.
        }
        trace!(
            "Unverified Record {:?} try to validate and store",
            PrettyPrintRecordKey::from(record.key.clone())
        );
        if let Some(event_sender) = self.event_sender.clone() {
            // push the event off thread so as to be non-blocking
            let _handle = tokio::spawn(async move {
                if let Err(error) = event_sender
                    .send(NetworkEvent::UnverifiedRecord(record))
                    .await
                {
                    error!("SwarmDriver failed to send event: {}", error);
                }
            });
        } else {
            error!("Record store doesn't have event_sender setup");
        }
        Ok(())
    }

    fn remove(&mut self, k: &Key) {
        let _ = self.records.remove(k);

        let filename = Self::key_to_hex(k);
        let file_path = self.config.storage_dir.join(&filename);

        let _handle = tokio::spawn(async move {
            match fs::remove_file(file_path) {
                Ok(_) => {
                    trace!("Removed record from disk! filename: {filename}");
                }
                Err(err) => {
                    error!("Error while removing file. filename: {filename}, error: {err:?}");
                }
            }
        });
    }

    // A backstop replication shall only trigger within pre-defined interval
    fn records(&self) -> Self::RecordsIter<'_> {
        RecordsIterator {
            keys: self.records.iter(),
            storage_dir: self.config.storage_dir.clone(),
        }
    }

    fn add_provider(&mut self, _record: ProviderRecord) -> Result<()> {
        // ProviderRecords are not used currently
        Ok(())
    }

    fn providers(&self, _key: &Key) -> Vec<ProviderRecord> {
        // ProviderRecords are not used currently
        vec![]
    }

    fn provided(&self) -> Self::ProvidedIter<'_> {
        // ProviderRecords are not used currently
        vec![].into_iter()
    }

    fn remove_provider(&mut self, _key: &Key, _provider: &PeerId) {
        // ProviderRecords are not used currently
    }
}

// Since 'Record's need to be read from disk for each individual 'Key', we need this iterator
// which does that operation at the very moment the consumer/user is iterating each item.
pub struct RecordsIterator<'a> {
    keys: hash_set::Iter<'a, Key>,
    storage_dir: PathBuf,
}

impl<'a> Iterator for RecordsIterator<'a> {
    type Item = Cow<'a, Record>;

    fn next(&mut self) -> Option<Self::Item> {
        for key in self.keys.by_ref() {
            let record = DiskBackedRecordStore::read_from_disk(key, &self.storage_dir);
            if record.is_some() {
                return record;
            }
        }

        None
    }
}

#[allow(trivial_casts)]
#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::{core::multihash::Multihash, kad::KBucketKey};
    use quickcheck::*;
    use tokio::runtime::Runtime;

    const MULITHASH_CODE: u64 = 0x12;

    #[derive(Clone, Debug)]
    struct ArbitraryKey(Key);
    #[derive(Clone, Debug)]
    struct ArbitraryPeerId(PeerId);
    #[derive(Clone, Debug)]
    struct ArbitraryKBucketKey(KBucketKey<PeerId>);
    #[derive(Clone, Debug)]
    struct ArbitraryRecord(Record);
    #[derive(Clone, Debug)]
    struct ArbitraryProviderRecord(ProviderRecord);

    impl Arbitrary for ArbitraryPeerId {
        fn arbitrary(g: &mut Gen) -> ArbitraryPeerId {
            let hash: [u8; 32] = core::array::from_fn(|_| u8::arbitrary(g));
            let peer_id = PeerId::from_multihash(
                Multihash::wrap(MULITHASH_CODE, &hash).expect("Failed to gen Multihash"),
            )
            .expect("Failed to create PeerId");
            ArbitraryPeerId(peer_id)
        }
    }

    impl Arbitrary for ArbitraryKBucketKey {
        fn arbitrary(_: &mut Gen) -> ArbitraryKBucketKey {
            ArbitraryKBucketKey(KBucketKey::from(PeerId::random()))
        }
    }

    impl Arbitrary for ArbitraryKey {
        fn arbitrary(g: &mut Gen) -> ArbitraryKey {
            let hash: [u8; 32] = core::array::from_fn(|_| u8::arbitrary(g));
            ArbitraryKey(Key::from(
                Multihash::<64>::wrap(MULITHASH_CODE, &hash).expect("Failed to gen MultiHash"),
            ))
        }
    }

    impl Arbitrary for ArbitraryRecord {
        fn arbitrary(g: &mut Gen) -> ArbitraryRecord {
            let record = Record {
                key: ArbitraryKey::arbitrary(g).0,
                value: Vec::arbitrary(g),
                publisher: None,
                expires: None,
            };
            ArbitraryRecord(record)
        }
    }

    impl Arbitrary for ArbitraryProviderRecord {
        fn arbitrary(g: &mut Gen) -> ArbitraryProviderRecord {
            let record = ProviderRecord {
                key: ArbitraryKey::arbitrary(g).0,
                provider: PeerId::random(),
                expires: None,
                addresses: vec![],
            };
            ArbitraryProviderRecord(record)
        }
    }

    #[test]
    fn put_get_remove_record() {
        fn prop(r: ArbitraryRecord) {
            let rt = if let Ok(rt) = Runtime::new() {
                rt
            } else {
                panic!("Cannot create runtime");
            };
            rt.block_on(testing_thread(r));
        }
        quickcheck(prop as fn(_))
    }

    async fn testing_thread(r: ArbitraryRecord) {
        let r = r.0;
        let (network_event_sender, mut network_event_receiver) = mpsc::channel(1);
        let mut store = DiskBackedRecordStore::with_config(
            PeerId::random(),
            Default::default(),
            Some(network_event_sender),
        );

        let store_cost_before = store.store_cost();
        // An initial unverified put should not write to disk
        assert!(store.put(r.clone()).is_ok());
        assert!(store.get(&r.key).is_none());
        // Store cost should not change if no PUT has been added
        assert!(store.store_cost() == store_cost_before);

        let returned_record = if let Some(event) = network_event_receiver.recv().await {
            if let NetworkEvent::UnverifiedRecord(record) = event {
                record
            } else {
                panic!("Unexpected network event {event:?}");
            }
        } else {
            panic!("Failed recevied the record for further verification");
        };

        let store_cost_before = store.store_cost();

        assert!(store.put_verified(returned_record).is_ok());

        // loop over store.get 10 times to make sure async disk write has had time to complete
        for _ in 0..10 {
            // try to check if it is equal to the actual record. This is needed because, the file
            // might not be fully written to the fs and would cause intermittent failures.
            // If there is actually a problem with the PUT, the assert statement below would catch it.
            if store
                .get(&r.key)
                .is_some_and(|record| Cow::Borrowed(&r) == record)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let cost_after_put = store.store_cost();
        // store cost should have increased
        assert!(cost_after_put > store_cost_before);

        assert_eq!(
            Some(Cow::Borrowed(&r)),
            store.get(&r.key),
            "record can be retrieved after put"
        );
        store.remove(&r.key);

        // now we've removed a record, the cost should decrease
        assert!(cost_after_put > store.store_cost());
        assert!(store.get(&r.key).is_none());
    }
}
