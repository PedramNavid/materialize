// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::cmp;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use differential_dataflow::lattice::Lattice;
use differential_dataflow::{AsCollection, Collection, Hashable};
use futures::{StreamExt, TryFutureExt};
use interchange::json::JsonEncoder;
use itertools::Itertools;
use ore::collections::CollectionExt;
use ore::retry::Retry;
use rdkafka::client::ClientContext;
use rdkafka::config::ClientConfig;
use rdkafka::error::{KafkaError, KafkaResult, RDKafkaErrorCode};
use rdkafka::message::{Message, ToBytes};
use rdkafka::producer::Producer;
use rdkafka::producer::{BaseRecord, DeliveryResult, ProducerContext, ThreadedProducer};
use timely::dataflow::channels::pact::Exchange;
use timely::dataflow::channels::pact::Pipeline;
use timely::dataflow::operators::generic::builder_rc::OperatorBuilder;
use timely::dataflow::operators::generic::{InputHandle, OutputHandle};
use timely::dataflow::operators::{Capability, Map};
use timely::dataflow::{Scope, Stream};
use timely::progress::frontier::AntichainRef;
use timely::progress::Antichain;
use timely::scheduling::Activator;
use tracing::{debug, error, info};

use dataflow_types::sinks::{
    KafkaSinkConnector, KafkaSinkConsistencyConnector, PublishedSchemaInfo, SinkAsOf, SinkDesc,
};
use expr::GlobalId;
use interchange::avro::{self, AvroEncoder, AvroSchemaGenerator};
use interchange::encode::Encode;
use kafka_util::client::MzClientContext;
use ore::cast::CastFrom;
use ore::metrics::{CounterVecExt, DeleteOnDropCounter, DeleteOnDropGauge, GaugeVecExt};
use repr::{Datum, Diff, RelationDesc, Row, Timestamp};
use timely_util::async_op;
use timely_util::operators_async_ext::OperatorBuilderExt;
use tokio::task;

use super::{KafkaBaseMetrics, SinkBaseMetrics};
use crate::render::sinks::SinkRender;
use crate::source::timestamp::TimestampBindingRc;
use prometheus::core::{AtomicI64, AtomicU64};

impl<G> SinkRender<G> for KafkaSinkConnector
where
    G: Scope<Timestamp = Timestamp>,
{
    fn uses_keys(&self) -> bool {
        true
    }

    fn get_key_indices(&self) -> Option<&[usize]> {
        self.key_desc_and_indices
            .as_ref()
            .map(|(_desc, indices)| indices.as_slice())
    }

    fn get_relation_key_indices(&self) -> Option<&[usize]> {
        self.relation_key_indices.as_deref()
    }

    fn render_continuous_sink(
        &self,
        _compute_state: &mut crate::render::ComputeState,
        storage_state: &mut crate::render::StorageState,
        sink: &SinkDesc,
        sink_id: GlobalId,
        sinked_collection: Collection<G, (Option<Row>, Option<Row>), Diff>,
        metrics: &SinkBaseMetrics,
    ) -> Option<Box<dyn Any>>
    where
        G: Scope<Timestamp = Timestamp>,
    {
        // consistent/exactly-once Kafka sinks need the timestamp in the row
        let sinked_collection = if self.consistency.is_some() {
            sinked_collection
                .inner
                .map(|((k, v), t, diff)| {
                    let v = v.map(|mut v| {
                        let t = t.to_string();
                        v.push_list_with(|rp| {
                            rp.push(Datum::String(&t));
                        });
                        v
                    });
                    ((k, v), t, diff)
                })
                .as_collection()
        } else {
            sinked_collection
        };

        // Extract handles to the relevant source timestamp histories the sink
        // needs to hear from before it can write data out to Kafka.
        let mut source_ts_histories = Vec::new();

        for id in &self.transitive_source_dependencies {
            if let Some(history) = storage_state.ts_histories.get(id) {
                // As soon as we have one sink that depends on a given source,
                // that source needs to persist timestamp bindings.
                history.enable_persistence();

                let mut history_bindings = history.clone();
                // We don't want these to block compaction
                // ever.
                history_bindings.set_compaction_frontier(Antichain::new().borrow());
                source_ts_histories.push(history_bindings);
            }
        }

        // TODO: this is a brittle way to indicate the worker that will write to the sink
        // because it relies on us continuing to hash on the sink_id, with the same hash
        // function, and for the Exchange pact to continue to distribute by modulo number
        // of workers.
        let peers = sinked_collection.inner.scope().peers();
        let worker_index = sinked_collection.inner.scope().index();
        let active_write_worker = (usize::cast_from(sink_id.hashed()) % peers) == worker_index;

        // Only the active_write_worker will ever produce data so all other workers have
        // an empty frontier.  It's necessary to insert all of these into `storage_state.
        // sink_write_frontier` below so we properly clear out default frontiers of
        // non-active workers.
        let shared_frontier = Rc::new(RefCell::new(if active_write_worker {
            Antichain::from_elem(0)
        } else {
            Antichain::new()
        }));

        let token = kafka(
            sinked_collection,
            sink_id,
            self.clone(),
            self.key_desc_and_indices
                .clone()
                .map(|(desc, _indices)| desc),
            self.value_desc.clone(),
            sink.as_of.clone(),
            source_ts_histories,
            shared_frontier.clone(),
            &metrics.kafka,
        );

        storage_state
            .sink_write_frontiers
            .insert(sink_id, shared_frontier);

        Some(token)
    }
}

/// Per-Kafka sink metrics.
pub struct SinkMetrics {
    messages_sent_counter: DeleteOnDropCounter<'static, AtomicI64, Vec<String>>,
    message_send_errors_counter: DeleteOnDropCounter<'static, AtomicI64, Vec<String>>,
    message_delivery_errors_counter: DeleteOnDropCounter<'static, AtomicI64, Vec<String>>,
    rows_queued: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
    messages_in_flight: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
}

impl SinkMetrics {
    fn new(
        base: &KafkaBaseMetrics,
        topic_name: &str,
        sink_id: &str,
        worker_id: &str,
    ) -> SinkMetrics {
        let labels = vec![
            topic_name.to_string(),
            sink_id.to_string(),
            worker_id.to_string(),
        ];
        SinkMetrics {
            messages_sent_counter: base
                .messages_sent_counter
                .get_delete_on_drop_counter(labels.clone()),
            message_send_errors_counter: base
                .message_send_errors_counter
                .get_delete_on_drop_counter(labels.clone()),
            message_delivery_errors_counter: base
                .message_delivery_errors_counter
                .get_delete_on_drop_counter(labels.clone()),
            rows_queued: base.rows_queued.get_delete_on_drop_gauge(labels.clone()),
            messages_in_flight: base.messages_in_flight.get_delete_on_drop_gauge(labels),
        }
    }
}

pub struct SinkProducerContext {
    metrics: Arc<SinkMetrics>,
    shutdown_flag: Arc<AtomicBool>,
}

impl SinkProducerContext {
    pub fn new(metrics: Arc<SinkMetrics>, shutdown_flag: Arc<AtomicBool>) -> Self {
        SinkProducerContext {
            metrics,
            shutdown_flag,
        }
    }
}

impl ClientContext for SinkProducerContext {
    // The shape of the rdkafka *Context traits require us to forward to the `MzClientContext`
    // implementation.
    fn log(&self, level: rdkafka::config::RDKafkaLogLevel, fac: &str, log_message: &str) {
        MzClientContext.log(level, fac, log_message)
    }
    fn error(&self, error: rdkafka::error::KafkaError, reason: &str) {
        MzClientContext.error(error, reason)
    }
}
impl ProducerContext for SinkProducerContext {
    type DeliveryOpaque = ();

    fn delivery(&self, result: &DeliveryResult, _: Self::DeliveryOpaque) {
        match result {
            Ok(_) => (),
            Err((e, msg)) => {
                self.metrics.message_delivery_errors_counter.inc();
                error!(
                    "received error while writing to kafka sink topic {}: {}",
                    msg.topic(),
                    e
                );
                self.shutdown_flag.store(true, Ordering::SeqCst);
            }
        }
    }
}

struct KafkaSinkToken {
    shutdown_flag: Arc<AtomicBool>,
}

impl Drop for KafkaSinkToken {
    fn drop(&mut self) {
        debug!("dropping kafka sink");
        self.shutdown_flag.store(true, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct KafkaTxProducer {
    inner: Arc<ThreadedProducer<SinkProducerContext>>,
    timeout: Duration,
}

impl KafkaTxProducer {
    fn init_transactions(&self) -> impl Future<Output = KafkaResult<()>> {
        let self_producer = Arc::clone(&self.inner);
        let self_timeout = self.timeout;
        task::spawn_blocking(move || self_producer.init_transactions(self_timeout))
            .unwrap_or_else(|_| Err(KafkaError::Canceled))
    }

    fn begin_transaction(&self) -> impl Future<Output = KafkaResult<()>> {
        let self_producer = Arc::clone(&self.inner);
        task::spawn_blocking(move || self_producer.begin_transaction())
            .unwrap_or_else(|_| Err(KafkaError::Canceled))
    }

    fn commit_transaction(&self) -> impl Future<Output = KafkaResult<()>> {
        let self_producer = Arc::clone(&self.inner);
        let self_timeout = self.timeout;
        task::spawn_blocking(move || self_producer.commit_transaction(self_timeout))
            .unwrap_or_else(|_| Err(KafkaError::Canceled))
    }

    fn abort_transaction(&self) -> impl Future<Output = KafkaResult<()>> {
        let self_producer = Arc::clone(&self.inner);
        let self_timeout = self.timeout;
        task::spawn_blocking(move || self_producer.abort_transaction(self_timeout))
            .unwrap_or_else(|_| Err(KafkaError::Canceled))
    }

    fn flush(&self) -> impl Future<Output = KafkaResult<()>> {
        let self_producer = Arc::clone(&self.inner);
        let self_timeout = self.timeout;
        task::spawn_blocking(move || self_producer.flush(self_timeout))
            .unwrap_or_else(|_| Err(KafkaError::Canceled))
    }

    fn in_flight_count(&self) -> i32 {
        // non-blocking call
        self.inner.in_flight_count()
    }

    fn send<'a, K, P>(
        &self,
        record: BaseRecord<'a, K, P>,
    ) -> Result<(), (KafkaError, BaseRecord<'a, K, P>)>
    where
        K: ToBytes + ?Sized,
        P: ToBytes + ?Sized,
    {
        self.inner.send(record)
    }
}

struct KafkaSinkState {
    name: String,
    topic: String,
    topic_prefix: String,
    shutdown_flag: Arc<AtomicBool>,
    metrics: Arc<SinkMetrics>,
    producer: KafkaTxProducer,
    activator: timely::scheduling::Activator,
    transactional: bool,
    consistency: Option<KafkaSinkConsistencyConnector>,
    pending_rows: HashMap<Timestamp, Vec<EncodedRow>>,
    ready_rows: VecDeque<(Timestamp, Vec<EncodedRow>)>,
    send_state: SendState,

    /// Timestamp of the latest `END` record that was written out to Kafka.
    latest_progress_ts: Timestamp,

    /// Write frontier of this sink.
    ///
    /// The write frontier potentially blocks compaction of timestamp bindings
    /// in upstream sources. The latest written `END` record is used when
    /// restarting the sink to gate updates with a lower timestamp. We advance
    /// the write frontier in lockstep with writing out END records. This
    /// ensures that we don't write updates more than once, ensuring
    /// exactly-once guaruantees.
    write_frontier: Rc<RefCell<Antichain<Timestamp>>>,
}

impl KafkaSinkState {
    fn new(
        connector: KafkaSinkConnector,
        sink_name: String,
        sink_id: &GlobalId,
        worker_id: String,
        shutdown_flag: Arc<AtomicBool>,
        activator: Activator,
        latest_progress_ts: Timestamp,
        write_frontier: Rc<RefCell<Antichain<Timestamp>>>,
        metrics: &KafkaBaseMetrics,
    ) -> Self {
        let config = Self::create_producer_config(&connector);

        let metrics = Arc::new(SinkMetrics::new(
            metrics,
            &connector.topic,
            &sink_id.to_string(),
            &worker_id,
        ));

        let producer = KafkaTxProducer {
            inner: Arc::new(
                config
                    .create_with_context::<_, ThreadedProducer<_>>(SinkProducerContext::new(
                        Arc::clone(&metrics),
                        Arc::clone(&shutdown_flag),
                    ))
                    .expect("creating kafka producer for Kafka sink failed"),
            ),
            timeout: Duration::from_secs(5),
        };

        KafkaSinkState {
            name: sink_name,
            topic: connector.topic,
            topic_prefix: connector.topic_prefix,
            shutdown_flag,
            metrics,
            producer,
            activator,
            transactional: connector.exactly_once,
            consistency: connector.consistency,
            pending_rows: HashMap::new(),
            ready_rows: VecDeque::new(),
            send_state: SendState::Init,
            latest_progress_ts,
            write_frontier,
        }
    }

    fn create_producer_config(connector: &KafkaSinkConnector) -> ClientConfig {
        let mut config = ClientConfig::new();
        config.set("bootstrap.servers", &connector.addrs.to_string());

        // Ensure that messages are sinked in order and without duplicates. Note that
        // this only applies to a single instance of a producer - in the case of restarts,
        // all bets are off and full exactly once support is required.
        config.set("enable.idempotence", "true");

        // Increase limits for the Kafka producer's internal buffering of messages
        // Currently we don't have a great backpressure mechanism to tell indexes or
        // views to slow down, so the only thing we can do with a message that we
        // can't immediately send is to put it in a buffer and there's no point
        // having buffers within the dataflow layer and Kafka
        // If the sink starts falling behind and the buffers start consuming
        // too much memory the best thing to do is to drop the sink
        // Sets the buffer size to be 16 GB (note that this setting is in KB)
        config.set("queue.buffering.max.kbytes", &format!("{}", 16 << 20));

        // Set the max messages buffered by the producer at any time to 10MM which
        // is the maximum allowed value
        config.set("queue.buffering.max.messages", &format!("{}", 10_000_000));

        // Make the Kafka producer wait at least 10 ms before sending out MessageSets
        // TODO(rkhaitan): experiment with different settings for this value to see
        // if it makes a big difference
        config.set("queue.buffering.max.ms", &format!("{}", 10));

        for (k, v) in connector.config_options.iter() {
            // We explicitly reject `statistics.interval.ms` here so that we don't
            // flood the INFO log with statistics messages.
            // TODO: properly support statistics on Kafka sinks
            // We explicitly reject 'isolation.level' as it's a consumer property
            // and, while benign, will fill the log with WARN messages
            if k != "statistics.interval.ms" && k != "isolation.level" {
                config.set(k, v);
            }
        }

        if connector.exactly_once {
            // TODO(aljoscha): this only works for now, once there's an actual
            // Kafka producer on each worker they would step on each others toes
            let transactional_id = format!("mz-producer-{}", connector.topic);
            config.set("transactional.id", transactional_id);
        }

        config
    }

    // TODO: maybe bound the backoff
    async fn retry_on_txn_error<'a, F, Fut, T>(&self, f: F) -> KafkaResult<T>
    where
        F: Fn(KafkaTxProducer) -> Fut,
        Fut: Future<Output = KafkaResult<T>>,
    {
        let shutdown = Cell::new(false);
        let self_producer = self.producer.clone();
        let mut last_error = KafkaError::Canceled;
        let tries = Retry::default()
            .clamp_backoff(Duration::from_secs(60 * 10))
            // Yes this might be bad but we had an infinite loop before so it's no worse. Fix when
            // addressing error strategy holistically.
            .max_tries(usize::MAX)
            .into_retry_stream();
        tokio::pin!(tries);
        while tries.next().await.is_some() {
            match f(self_producer.clone()).await {
                Ok(result) => return Ok(result),
                Err(KafkaError::Transaction(e)) => {
                    // clone is a cheap: Option<Arc<..>> internally
                    last_error = KafkaError::Transaction(e.clone());
                    if e.txn_requires_abort() {
                        info!("Error requiring abort in kafka sink: {:?}", e);
                        let self_self_producer = self_producer.clone();
                        let should_shutdown = Retry::default()
                            .clamp_backoff(Duration::from_secs(60 * 10))
                            // Yes this might be bad but we had an infinite loop before so it's no
                            // worse.  Fix when addressing error strategy holistically.
                            .max_tries(usize::MAX)
                            .retry_async(|_| async {
                                match self_self_producer.abort_transaction().await {
                                    Ok(_) => Ok(false),
                                    Err(KafkaError::Transaction(e)) if e.is_retriable() => {
                                        Err(KafkaError::Transaction(e))
                                    }
                                    Err(_) => Ok(true),
                                }
                            })
                            .await
                            .unwrap_or(true);
                        shutdown.set(should_shutdown);
                    } else if e.is_retriable() {
                        info!("Retriable error in kafka sink: {:?}", e);
                        continue;
                    } else {
                        shutdown.set(true);
                    }
                    break;
                }
                Err(e) => {
                    last_error = e;
                    shutdown.set(true);
                    break;
                }
            }
        }

        // Consider a retriable error that's hit our max backoff to be fatal.
        if shutdown.get() {
            self.shutdown_flag.store(true, Ordering::SeqCst);
            // Indicate that the sink is closed to everyone else who
            // might be tracking its write frontier.
            info!("shutting down kafka sink: {}", &self.name);
        }
        Err(last_error)
    }

    async fn begin_transaction(&self) -> KafkaResult<()> {
        self.producer.begin_transaction().await
    }

    async fn commit_transaction(&self) -> KafkaResult<()> {
        self.producer.commit_transaction().await
    }

    async fn send<'a, K, P>(&self, mut record: BaseRecord<'a, K, P>) -> KafkaResult<()>
    where
        K: ToBytes + ?Sized,
        P: ToBytes + ?Sized,
    {
        let mut last_error = KafkaError::Canceled;
        // Because of the lifetime bound on `record`, we can't just use `Retry::retry` so use the stream
        let tries = Retry::default()
            .clamp_backoff(Duration::from_secs(60 * 10))
            // Yes this might be bad but we had an infinite loop before so it's no worse. Fix when
            // addressing error strategy holistically.
            .max_tries(usize::MAX)
            .into_retry_stream();
        tokio::pin!(tries);
        while tries.next().await.is_some() {
            match self.producer.send(record) {
                Ok(_) => {
                    self.metrics.messages_sent_counter.inc();
                    return Ok(());
                }
                Err((e, rec)) => {
                    record = rec;
                    last_error = e;
                    self.metrics.message_send_errors_counter.inc();

                    if let KafkaError::MessageProduction(RDKafkaErrorCode::QueueFull) = last_error {
                        debug!(
                            "unable to produce message in {}: rdkafka queue full. Retrying.",
                            self.name
                        );
                        continue;
                    } else {
                        // We've received an error that is not transient
                        error!(
                            "unable to produce message in {}: {}. Shutting down sink.",
                            self.name, last_error
                        );
                        self.shutdown_flag.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
        Err(last_error)
    }

    async fn send_consistency_record(
        &self,
        transaction_id: &str,
        status: &str,
        message_count: Option<i64>,
    ) -> KafkaResult<()> {
        let consistency = self
            .consistency
            .as_ref()
            .expect("no consistency information");

        let encoded = avro::encode_debezium_transaction_unchecked(
            consistency.schema_id,
            &self.topic_prefix,
            transaction_id,
            status,
            message_count,
        );

        let record = BaseRecord::to(&consistency.topic)
            .payload(&encoded)
            .key(&self.topic_prefix);

        self.send(record).await
    }

    /// Asserts that the write frontier has not yet advanced beyond `t`.
    fn assert_progress(&self, ts: &Timestamp) {
        assert!(self.write_frontier.borrow().less_equal(ts));
    }

    /// Updates the latest progress update timestamp to `latest_update_ts` if it
    /// is greater than the currently maintained timestamp.
    ///
    /// This does not emit a progress update and should be used when the sink
    /// emits a progress update that is not based on updates of the input
    /// frontier.
    ///
    /// See [`maybe_emit_progress`](Self::maybe_emit_progress).
    fn maybe_update_progress(&mut self, latest_update_ts: &Timestamp) {
        if *latest_update_ts > self.latest_progress_ts {
            self.latest_progress_ts = *latest_update_ts;
        }
    }

    /// Updates the latest progress update timestamp based on the given
    /// input frontier and pending rows.
    ///
    /// This will emit an `END` record to the consistency topic if the frontier
    /// advanced and advance the maintained write frontier, which will in turn
    /// unblock compaction of timestamp bindings in sources.
    ///
    /// *NOTE*: `END` records will only be emitted when
    /// `KafkaSinkConnector.consistency` points to a consistency topic. The
    /// write frontier will be advanced regardless.
    async fn maybe_emit_progress<'a>(
        &mut self,
        input_frontier: AntichainRef<'a, Timestamp>,
    ) -> Result<(), anyhow::Error> {
        // This only looks at the first entry of the antichain.
        // If we ever have multi-dimensional time, this is not correct
        // anymore. There might not even be progress in the first dimension.
        // We panic, so that future developers introducing multi-dimensional
        // time in Materialize will notice.
        let input_frontier = input_frontier
            .iter()
            .at_most_one()
            .expect("more than one element in the frontier")
            .cloned();

        let min_pending_ts = self.pending_rows.keys().min().cloned();

        let min_frontier = input_frontier.into_iter().chain(min_pending_ts).min();

        if let Some(min_frontier) = min_frontier {
            // a frontier of `t` means we still might receive updates with `t`.
            // The progress frontier we emit is a strict frontier, so subtract `1`.
            let min_frontier = min_frontier.saturating_sub(1);

            if min_frontier > self.latest_progress_ts {
                // record the write frontier in the consistency topic.
                if self.consistency.is_some() {
                    if self.transactional {
                        self.begin_transaction().await?
                    }

                    self.send_consistency_record(&min_frontier.to_string(), "END", None)
                        .await
                        .map_err(|_| anyhow::anyhow!("Error sending write frontier update."))?;

                    if self.transactional {
                        self.commit_transaction().await?
                    }
                }
                self.latest_progress_ts = min_frontier;
            }

            let mut write_frontier = self.write_frontier.borrow_mut();

            // make sure we don't regress
            assert!(write_frontier.less_equal(&min_frontier));
            write_frontier.clear();
            write_frontier.insert(min_frontier);
        } else {
            // If there's no longer an input frontier, we will no longer receive any data forever and, therefore, will
            // never output more data
            self.write_frontier.borrow_mut().clear();
        }

        Ok(())
    }
}

#[derive(Debug, Copy, Clone)]
enum SendState {
    // Initialize ourselves as a transactional producer with Kafka
    // Note that this only runs once across all workers - it should only execute
    // for the worker that will actually be publishing to kafka
    Init,
    Running,
}

#[derive(Debug)]
struct EncodedRow {
    key: Option<Vec<u8>>,
    value: Option<Vec<u8>>,
    count: usize,
}

// TODO@jldlaughlin: What guarantees does this sink support? #1728
fn kafka<G>(
    collection: Collection<G, (Option<Row>, Option<Row>)>,
    id: GlobalId,
    connector: KafkaSinkConnector,
    key_desc: Option<RelationDesc>,
    value_desc: RelationDesc,
    as_of: SinkAsOf,
    source_timestamp_histories: Vec<TimestampBindingRc>,
    write_frontier: Rc<RefCell<Antichain<Timestamp>>>,
    metrics: &KafkaBaseMetrics,
) -> Box<dyn Any>
where
    G: Scope<Timestamp = Timestamp>,
{
    let name = format!("kafka-{}", id);

    let stream = &collection.inner;

    let encoded_stream = match connector.published_schema_info {
        Some(PublishedSchemaInfo {
            key_schema_id,
            value_schema_id,
        }) => {
            let schema_generator = AvroSchemaGenerator::new(
                None,
                None,
                key_desc,
                value_desc,
                connector.consistency.is_some(),
            );
            let encoder = AvroEncoder::new(schema_generator, key_schema_id, value_schema_id);
            encode_stream(
                stream,
                as_of.clone(),
                connector
                    .consistency
                    .clone()
                    .and_then(|consistency| consistency.gate_ts),
                encoder,
                connector.fuel,
                name.clone(),
            )
        }
        None => {
            let encoder = JsonEncoder::new(key_desc, value_desc, connector.consistency.is_some());
            encode_stream(
                stream,
                as_of.clone(),
                connector
                    .consistency
                    .clone()
                    .and_then(|consistency| consistency.gate_ts),
                encoder,
                connector.fuel,
                name.clone(),
            )
        }
    };

    produce_to_kafka(
        encoded_stream,
        id,
        name,
        connector,
        as_of,
        source_timestamp_histories,
        write_frontier,
        metrics,
    )
}

/// Produces/sends a stream of encoded rows (as `Vec<u8>`) to Kafka.
///
/// This operator exchanges all updates to a single worker by hashing on the given sink `id`.
///
/// Updates are only sent to Kafka once the input frontier has passed their `time`. Updates are
/// sent in ascending timestamp order. The order of updates at the same timstamp will not be changed.
/// However, it is important to keep in mind that this operator exchanges updates so if the input
/// stream is sharded updates will likely arrive at this operator in some non-deterministic order.
///
/// Updates that are not beyond the given [`SinkAsOf`] and/or the `gate_ts` in
/// [`KafkaSinkConnector`] will be discarded without producing them.
pub fn produce_to_kafka<G>(
    stream: Stream<G, ((Option<Vec<u8>>, Option<Vec<u8>>), Timestamp, Diff)>,
    id: GlobalId,
    name: String,
    connector: KafkaSinkConnector,
    as_of: SinkAsOf,
    source_timestamp_histories: Vec<TimestampBindingRc>,
    write_frontier: Rc<RefCell<Antichain<Timestamp>>>,
    metrics: &KafkaBaseMetrics,
) -> Box<dyn Any>
where
    G: Scope<Timestamp = Timestamp>,
{
    let mut builder = OperatorBuilder::new(name.clone(), stream.scope());
    let activator = stream
        .scope()
        .activator_for(&builder.operator_info().address[..]);

    let shutdown_flag = Arc::new(AtomicBool::new(false));

    let latest_progress_ts = match &connector.consistency {
        Some(consistency) => match consistency.gate_ts {
            Some(gate_ts) => gate_ts,
            None => Timestamp::MIN,
        },
        None => Timestamp::MIN,
    };

    let mut s = KafkaSinkState::new(
        connector,
        name,
        &id,
        stream.scope().index().to_string(),
        Arc::clone(&shutdown_flag),
        activator,
        latest_progress_ts,
        write_frontier,
        metrics,
    );

    // Keep track of whether this operator/worker ever received updates. We
    // use this to control who should send continuous END progress records
    // to Kafka below.
    let mut is_active_worker = false;

    let mut vector = Vec::new();

    // keep the latest progress updates, if any, in order to update
    // our internal state after the send loop
    let mut progress_update = None;

    // We want exactly one worker to send all the data to the sink topic.
    let hashed_id = id.hashed();
    let mut input = builder.new_input(&stream, Exchange::new(move |_| hashed_id));

    builder.build_async(
        stream.scope(),
        async_op!(|_initial_capabilities, frontiers| {
            if s.shutdown_flag.load(Ordering::SeqCst) {
                debug!("shutting down sink: {}", &s.name);
                // One last attempt to push anything pending to kafka before closing.
                let _ = s.producer.flush().await;

                // Indicate that the sink is closed to everyone else who
                // might be tracking its write frontier.
                s.write_frontier.borrow_mut().clear();
                return false;
            }
            // Panic if there's not exactly once element in the frontier like we expect.
            let frontier = frontiers.clone().into_element();

            // Figure out the durablity frontier for all sources we depend on
            let durability_frontier = source_timestamp_histories
                .iter()
                .fold(Antichain::new(), |accum, history| {
                    accum.meet(&history.durability_frontier())
                });

            // Queue all pending rows waiting to be sent to kafka
            input.for_each(|_, rows| {
                is_active_worker = true;
                rows.swap(&mut vector);
                for ((key, value), time, diff) in vector.drain(..) {
                    let should_emit = if as_of.strict {
                        as_of.frontier.less_than(&time)
                    } else {
                        as_of.frontier.less_equal(&time)
                    };

                    let previously_published = match &s.consistency {
                        Some(consistency) => match consistency.gate_ts {
                            Some(gate_ts) => time <= gate_ts,
                            None => false,
                        },
                        None => false,
                    };

                    if !should_emit || previously_published {
                        // Skip stale data for already published timestamps
                        continue;
                    }

                    assert!(diff >= 0, "can't sink negative multiplicities");
                    if diff == 0 {
                        // Explicitly refuse to send no-op records
                        continue;
                    };
                    let diff = diff as usize;

                    let rows = s.pending_rows.entry(time).or_default();
                    rows.push(EncodedRow {
                        key,
                        value,
                        count: diff,
                    });
                    s.metrics.rows_queued.inc();
                }
            });

            // Move any newly closed timestamps from pending to ready
            let mut closed_ts: Vec<u64> = s
                .pending_rows
                .iter()
                .filter(|(ts, _)| !frontier.less_equal(*ts) && !durability_frontier.less_equal(*ts))
                .map(|(&ts, _)| ts)
                .collect();
            closed_ts.sort_unstable();
            closed_ts.into_iter().for_each(|ts| {
                let rows = s.pending_rows.remove(&ts).unwrap();
                s.ready_rows.push_back((ts, rows));
            });

            // Can't use `?` when the return type is a `bool` so use a custom try operator
            macro_rules! bail_err {
                ($expr:expr) => {
                    match $expr {
                        Ok(val) => val,
                        Err(_) => {
                            s.activator.activate();
                            return true;
                        }
                    }
                };
            }

            if is_active_worker
                && matches!(s.send_state, SendState::Init)
                // Removing this check would cause us to start writing out END consistency rows as
                // soon as the sink starts up.  There isn't anything inherently wrong with this but
                // we need to update testdrive to be able to check a reasonable condition that this
                // is correct.  That is, td currently checks we start with a BEGIN and then END.
                // We'd need to be able to do a search for a BEGIN to properly test were we to
                // remove this check.  This would fix #9514.
                && s.ready_rows.front().is_some()
            {
                if s.transactional {
                    bail_err!(s.retry_on_txn_error(|p| p.init_transactions()).await);
                }
                s.send_state = SendState::Running;
            }

            while let Some((ts, rows)) = s.ready_rows.front() {
                assert!(is_active_worker);

                if s.transactional {
                    bail_err!(s.retry_on_txn_error(|p| p.begin_transaction()).await);
                }
                if s.consistency.is_some() {
                    bail_err!(
                        s.send_consistency_record(&ts.to_string(), "BEGIN", None)
                            .await
                    );
                }

                let mut repeat_counter = 0;
                let mut total_sent = 0;
                for encoded_row in rows {
                    let record = BaseRecord::to(&s.topic);
                    let record = match encoded_row.value.as_ref() {
                        Some(r) => record.payload(r),
                        None => record,
                    };
                    let record = match encoded_row.key.as_ref() {
                        Some(r) => record.key(r),
                        None => record,
                    };

                    // Only fatal errors are returned from send
                    bail_err!(s.send(record).await);

                    // advance to the next repetition of this row, or the next row if all
                    // reptitions are exhausted
                    total_sent += 1;
                    repeat_counter += 1;
                    if repeat_counter == encoded_row.count {
                        repeat_counter = 0;
                        s.metrics.rows_queued.dec();
                    }
                }

                if s.consistency.is_some() {
                    bail_err!(
                        s.send_consistency_record(&ts.to_string(), "END", Some(total_sent))
                            .await
                    );
                }
                if s.transactional {
                    bail_err!(s.retry_on_txn_error(|p| p.commit_transaction()).await);
                };

                bail_err!(s.retry_on_txn_error(|p| p.flush()).await);

                // sanity check for the continuous updating
                // of the write frontier below
                s.assert_progress(ts);
                progress_update.replace(ts.clone());

                s.ready_rows.pop_front();
            }

            // update our state based on any END records we might have sent
            if let Some(ts) = progress_update.take() {
                s.maybe_update_progress(&ts);
            }

            // If we don't have ready rows, our write frontier equals the minimum
            // of the input frontier and any stashed timestamps.
            // While we still have ready rows that we're emitting, hold the write
            // frontier at the previous time.
            //
            // Only ever emit progress records if this operator/worker received
            // updates. Only on worker receives all the updates and we don't want
            // the other workers to also emit END records.
            if is_active_worker {
                if let Err(e) = s.maybe_emit_progress(frontier.borrow()).await {
                    // This can happen when the producer has not been
                    // initialized yet. This also means, that we only start
                    // emitting continuous updates once some real data
                    // has been emitted.
                    debug!("Error writing out progress update: {}", e);
                }
            }

            if !s.pending_rows.is_empty() {
                // We have some more rows that we need to wait for frontiers to advance before we
                // can write them out. Let's make sure to reschedule with a small delay to give the
                // system time to advance.
                s.activator.activate_after(Duration::from_millis(100));
                return true;
            }

            // N.B. Given the `flush` call above, I don't think we should ever end up in this
            // situation but let's keep the metrics / logging here so we can verify this in real
            // world use cases.
            let in_flight = s.producer.in_flight_count();
            s.metrics.messages_in_flight.set(in_flight as u64);
            if in_flight > 0 {
                // We still have messages that need to be flushed out to Kafka
                // Let's make sure to keep the sink operator around until
                // we flush them out
                s.activator.activate_after(Duration::from_secs(5));
                return true;
            }

            false
        }),
    );

    Box::new(KafkaSinkToken { shutdown_flag })
}

/// Encodes a stream of `(Option<Row>, Option<Row>)` updates using the specified encoder.
///
/// This operator will only encode `fuel` number of updates per invocation. If necessary, it will
/// stash updates and use an [`timely::scheduling::Activator`] to re-schedule future invocations.
///
/// Input [`Row`] updates must me compatible with the given implementor of [`Encode`].
///
/// Updates that are not beyond the given [`SinkAsOf`] and/or the `gate_ts` will be discarded
/// without encoding them.
///
/// Input updates do not have to be partitioned and/or sorted. This operator will not exchange
/// data. Updates with lower timestamps will be processed before updates with higher timestamps
/// if they arrive in order. However, this is not a guarantee, as this operator does not wait
/// for the frontier to signal completeness. It is an optimization for downstream operators
/// that behave suboptimal when receiving updates that are too far in the future with respect
/// to the current frontier. The order of updates that arrive at the same timestamp will not be
/// changed.
fn encode_stream<G>(
    input_stream: &Stream<G, ((Option<Row>, Option<Row>), Timestamp, Diff)>,
    as_of: SinkAsOf,
    gate_ts: Option<Timestamp>,
    encoder: impl Encode + 'static,
    fuel: usize,
    name_prefix: String,
) -> Stream<G, ((Option<Vec<u8>>, Option<Vec<u8>>), Timestamp, Diff)>
where
    G: Scope<Timestamp = Timestamp>,
{
    let name = format!("{}-{}_encode", name_prefix, encoder.get_format_name());

    let mut builder = OperatorBuilder::new(name, input_stream.scope());
    let mut input = builder.new_input(&input_stream, Pipeline);
    let (mut output, output_stream) = builder.new_output();
    builder.set_notify(false);

    let activator = input_stream
        .scope()
        .activator_for(&builder.operator_info().address[..]);

    let mut stash: HashMap<Capability<Timestamp>, Vec<_>> = HashMap::new();
    let mut vector = Vec::new();
    let mut encode_logic = move |input: &mut InputHandle<
        Timestamp,
        ((Option<Row>, Option<Row>), Timestamp, Diff),
        _,
    >,
                                 output: &mut OutputHandle<
        _,
        ((Option<Vec<u8>>, Option<Vec<u8>>), Timestamp, Diff),
        _,
    >| {
        let mut fuel_remaining = fuel;
        // stash away all the input we get, we want to be a nice citizen
        input.for_each(|cap, data| {
            data.swap(&mut vector);
            let stashed = stash.entry(cap.retain()).or_default();
            for update in vector.drain(..) {
                let time = update.1;

                let should_emit = if as_of.strict {
                    as_of.frontier.less_than(&time)
                } else {
                    as_of.frontier.less_equal(&time)
                };

                let ts_gated = match gate_ts {
                    Some(gate_ts) => time <= gate_ts,
                    None => false,
                };

                if !should_emit || ts_gated {
                    // Skip stale data for already published timestamps
                    continue;
                }
                stashed.push(update);
            }
        });

        // work off some of our data and then yield, can't be hogging
        // the worker for minutes at a time

        while fuel_remaining > 0 && !stash.is_empty() {
            let lowest_ts = stash
                .keys()
                .min_by(|x, y| x.time().cmp(y.time()))
                .expect("known to exist")
                .clone();
            let records = stash.get_mut(&lowest_ts).expect("known to exist");

            let mut session = output.session(&lowest_ts);
            let num_records_to_drain = cmp::min(records.len(), fuel_remaining);
            records
                .drain(..num_records_to_drain)
                .for_each(|((key, value), time, diff)| {
                    let key = key.map(|key| encoder.encode_key_unchecked(key));
                    let value = value.map(|value| encoder.encode_value_unchecked(value));
                    session.give(((key, value), time, diff));
                });

            fuel_remaining -= num_records_to_drain;

            if records.is_empty() {
                // drop our capability for this time
                stash.remove(&lowest_ts);
            }
        }

        if !stash.is_empty() {
            activator.activate();
            return true;
        }
        // signal that we're complete now
        false
    };

    builder.build_reschedule(|_capabilities| {
        move |_frontiers| {
            let mut output_handle = output.activate();
            encode_logic(&mut input, &mut output_handle)
        }
    });

    output_stream
}
