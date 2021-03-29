//! Data types representing metrics dumped from a running Noria instance

pub use metrics::Key;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;

/// Documents the set of metrics that are currently being recorded within
/// a ReadySet instance.
pub mod recorded {
    /// Counter: The number of lookup misses that occured during replay
    /// requests. Recorded at the domain on every lookup miss during a
    /// replay request.
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The index of the domain the replay miss is recorded in |
    /// | shard | The shard the replay miss is recorded in |
    /// | miss_in | The LocalNodeIndex of the data flow node where the miss occured |
    /// | needed_for | The client tag of the request that the replay is required for. |
    pub const DOMAIN_REPLAY_MISSES: &str = "domain.replay_misses";

    /// Counter: The time in microseconds that a domain spends
    /// handling and forwarding a Message or Input packet. Recorded at
    /// the domain following handling each Message and Input packet.
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The index of the domain handling the packet. |
    /// | shard | The shard handling the packet. |
    /// | from_node | The src node of the packet. |
    /// | to_node |The dst node of the packet. |
    pub const DOMAIN_FORWARD_TIME: &str = "domain.forward_time_us";

    /// Counter: The time in microseconds that a domain spends
    /// handling a ReplayPiece packet. Recorded at the domain following
    /// ReplayPiece packet handling.
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The index of the domain the replay miss is recorded in. |
    /// | shard | The shard the replay miss is recorded in. |
    /// | tag | The client tag of the request that the replay is required for. |
    pub const DOMAIN_REPLAY_TIME: &str = "domain.handle_replay_time";

    /// Counter: The time in microseconds spent handling a reader replay
    /// request. Recorded at the domain following RequestReaderReplay
    /// packet handling.
    ///
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The index of the domain the reader replay request is recorded in. |
    /// | shard | The shard the reader replay request is recorded in. |
    /// | node | The LocalNodeIndex of the reader node handling the packet. |
    pub const DOMAIN_READER_REPLAY_REQUEST_TIME: &str = "domain.reader_replay_request_time_us";

    /// Counter: The time in microseconds that a domain spends
    /// handling a RequestPartialReplay packet. Recorded at the domain
    /// following RequestPartialReplay packet handling.
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The index of the domain the replay request is recorded in. |
    /// | shard |The shard the replay request is recorded in. |
    /// | tag | The client tag of the request that the replay is required for. |
    pub const DOMAIN_SEED_REPLAY_TIME: &str = "domain.seed_replay_time_us";

    /// Counter: The time in microseconds that a domain spawning a state
    /// chunker at a node during the processing of a StartReplay packet.
    /// Recorded at the domain when the state chunker thread is finished
    /// executing.
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The index of the domain the start replay request is recorded in. |
    /// | shard | The shard the replay request is recorded in. |
    /// | from_node | The first node on the replay path. |
    pub const DOMAIN_CHUNKED_REPLAY_TIME: &str = "domain.chunked_replay_time_us";

    /// Counter: The time in microseconds that a domain spends
    /// handling a StartReplay packet. Recorded at the domain
    /// following StartReplay packet handling.
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The index of the domain the replay request is recorded in. |
    /// | shard | The shard the replay request is recorded in. |
    /// | tag | The client tag of the request that the replay is required for. |
    pub const DOMAIN_CHUNKED_REPLAY_START_TIME: &str = "domain.chunked_replay_start_time_us";

    /// Counter: The time in microseconds that a domain spends
    /// handling a Finish packet for a replay. Recorded at the domain
    /// following Finish packet handling.
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The index of the domain the replay request is recorded in. |
    /// | shard | The shard the replay request is recorded in. |
    /// | tag | The client tag of the request that the Finish packet is required for. |
    pub const DOMAIN_FINISH_REPLAY_TIME: &str = "domain.finish_replay_time_us";

    /// Counter: The time in microseconds that the domain spends handling
    /// a buffered replay request. Recorded at the domain following packet
    /// handling.
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The index of the domain the replay request is recorded in. |
    /// | shard | The shard the replay request is recorded in. |
    /// | requesting_shard | The shard that is requesting to be seeded. |
    /// | tag | The client tag of the request that the Finish packet is required for. |
    pub const DOMAIN_SEED_ALL_TIME: &str = "domain.seed_all_time_us";

    /// Counter: The time in microseconds that the controller spent committing
    /// a migration to the soup graph. Recorded at the controller at the end of
    /// the `commit` call.
    pub const CONTROLLER_MIGRATION_TIME: &str = "controller.migration_time_us";

    /// Counter: The number of evicitons performed at a worker. Incremented each
    /// time `do_eviction` is called at the worker.
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The domain that the eviction is performed in. |
    pub const EVICTION_WORKER_EVICTIONS_REQUESTED: &str = "eviction_worker.evictions_requested";

    /// Gauge: The amount of bytes the eviction worker is using for the current
    /// state sizes.
    pub const EVICTION_WORKER_PARTIAL_MEMORY_BYTES_USED: &str =
        "eviction_worker.partial_memory_used_bytes";

    /// Gauge: The amount of bytes required to store a dataflow node's state.
    ///
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The index of the domain. |
    /// | shard | The shard identifier of the domain. |
    /// | node | The LocalNodeIndex of the dataflow node. |
    pub const DOMAIN_NODE_STATE_SIZE_BYTES: &str = "domain.node_state_size_bytes";

    /// Gauge: The sum of the amount of bytes used to store the dataflow node's
    /// partial state within a domain.
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The index of the domain. |
    /// | shard | The shard identifier of the domain. |
    pub const DOMAIN_PARTIAL_STATE_SIZE_BYTES: &str = "domain.partial_state_size_bytes";

    /// Gauge: The sum of the amount of bytes used to store a node's reader state
    /// within a domain.
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The index of the domain. |
    /// | shard | The shard identifier of the domain. |
    pub const DOMAIN_READER_STATE_SIZE_BYTES: &str = "domain.reader_state_size_bytes";

    /// Gauge: The sum of a domain's total node state and reader state bytes.
    ///
    /// | Tag | Description |
    /// | --- | ----------- |
    /// | domain | The index of the domain. |
    /// | shard | The shard identifier of the domain. |
    pub const DOMAIN_TOTAL_NODE_STATE_SIZE_BYTES: &str = "domain.total_node_state_size_bytes";
}

/// A dumped metric's kind.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum DumpedMetricKind {
    /// Counters that can be incremented or decremented
    Counter,

    /// Gauges whose values can be explicitly set
    Gauge,
}

/// A dumped metric's value.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DumpedMetric {
    /// Labels associated with this metric value.
    pub labels: HashMap<String, String>,
    /// The actual value.
    pub value: f64,
    /// The kind of this metric.
    pub kind: DumpedMetricKind,
}

/// A dump of metrics that implements `Serialize`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MetricsDump {
    /// The actual metrics.
    pub metrics: HashMap<String, Vec<DumpedMetric>>,
}

fn convert_key(k: Key) -> (String, HashMap<String, String>) {
    let key_data = k.into_owned();
    let (name_parts, labels) = key_data.into_parts();
    let name = name_parts.to_string();
    let labels = labels
        .into_iter()
        .map(|l| {
            let (k, v) = l.into_parts();
            (k.into_owned(), v.into_owned())
        })
        .collect();
    (name, labels)
}

impl MetricsDump {
    /// Build a [`MetricsDump`] from a map containing values for counters, and another map
    /// containing values for gauges
    pub fn from_metrics(counters: HashMap<Key, u64>, gauges: HashMap<Key, f64>) -> Self {
        let mut ret = HashMap::new();
        for (key, val) in counters.into_iter() {
            let (name, labels) = convert_key(key);
            let ent = ret.entry(name).or_insert_with(Vec::new);
            ent.push(DumpedMetric {
                labels,
                // It's going to be serialized to JSON anyway, so who cares
                value: val as f64,
                kind: DumpedMetricKind::Counter,
            });
        }
        for (key, val) in gauges.into_iter() {
            let (name, labels) = convert_key(key);
            let ent = ret.entry(name).or_insert_with(Vec::new);
            ent.push(DumpedMetric {
                labels,
                value: val,
                kind: DumpedMetricKind::Gauge,
            });
        }
        Self { metrics: ret }
    }

    /// Return the sum of all the reported values for the given metric, if present
    pub fn total<K>(&self, metric: &K) -> Option<f64>
    where
        String: Borrow<K>,
        K: Hash + Eq + ?Sized,
    {
        Some(self.metrics.get(metric)?.iter().map(|m| m.value).sum())
    }

    /// Return an iterator over all the metric keys in this [`MetricsDump`]
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.metrics.keys()
    }
}
