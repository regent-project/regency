//! Live view of running jobs (D24), over the RabbitMQ stream `regency.live`.
//!
//! - Producers (runner, endpoint) publish batches of regent-sdk `LiveUpdate`s,
//!   each batch a regent-sdk `ReportLiveRequest` (the message proactive hosts
//!   already send to the endpoint, forwarded as-is). Best effort: a producer
//!   never waits for the stream, and drops what it cannot send.
//! - Every backend replica follows the stream ([`follow`]) and serves the live
//!   views from memory.
//! - The run's log (regent-sdk `RunLog`), cropped to the `run_log_max_bytes`
//!   setting, travels with the outcome ([`crate::messages::set_run_log`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use protobuf::{Parse, Serialize};
use rabbitmq_stream_client::types::{Message, OffsetSpecification};
use rabbitmq_stream_client::{NoDedup, Producer};
use regent_sdk::hosts::live::{LiveSink, LiveUpdate, RunLog};
use regent_sdk::proto::ReportLiveRequest;
use regent_sdk::secrets::SecretProvidersPool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::RabbitMqConfig;
use crate::control::{ControlError, environment};
use crate::couch::{Couch, Since};
use crate::model::{DB_SETTINGS, DEFAULT_RUN_LOG_MAX_BYTES, SETTINGS_DOC_ID, SettingsDoc};
use crate::topology::STREAM_LIVE;

/// Updates a run may queue before the forwarder (beyond: dropped by the SDK).
pub const SINK_CAPACITY: usize = 4096;
/// A batch leaves when it holds this many updates…
const BATCH_MAX: usize = 128;
/// …or when its oldest update waited this long.
const BATCH_DELAY: Duration = Duration::from_millis(200);
/// Batches waiting for the stream producer (beyond: dropped).
const LINK_QUEUE: usize = 1024;

/// Publishing side of `regency.live`: a background task owns the stream
/// producer and reconnects when needed; [`LiveLink::send`] never waits.
#[derive(Clone)]
pub struct LiveLink {
    tx: mpsc::Sender<Vec<u8>>,
    dropped: Arc<AtomicU64>,
}

impl LiveLink {
    pub fn spawn(
        config: RabbitMqConfig,
        secrets: SecretProvidersPool,
        cancel: CancellationToken,
    ) -> Self {
        let (tx, rx) = mpsc::channel(LINK_QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        tokio::spawn(produce(config, secrets, rx, cancel, dropped.clone()));
        Self { tx, dropped }
    }

    /// Queue one encoded `ReportLiveRequest`; dropped if the stream does not
    /// keep up (counted, see [`LiveLink::dropped`]).
    pub fn send(&self, batch: Vec<u8>) {
        if self.tx.try_send(batch).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Batches dropped so far (queue full, or stream unavailable).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

async fn produce(
    config: RabbitMqConfig,
    secrets: SecretProvidersPool,
    mut rx: mpsc::Receiver<Vec<u8>>,
    cancel: CancellationToken,
    dropped: Arc<AtomicU64>,
) {
    let mut producer: Option<Producer<NoDedup>> = None;
    let mut retry_at = tokio::time::Instant::now();
    loop {
        let batch = tokio::select! {
            _ = cancel.cancelled() => break,
            batch = rx.recv() => match batch { Some(batch) => batch, None => break },
        };
        if producer.as_ref().is_some_and(Producer::is_closed) {
            producer = None;
        }
        if producer.is_none() && tokio::time::Instant::now() >= retry_at {
            let created = tokio::time::timeout(Duration::from_secs(10), async {
                let environment = environment(&config, &secrets).await?;
                environment
                    .producer()
                    .build(STREAM_LIVE)
                    .await
                    .map_err(|e| ControlError::Stream(e.to_string()))
            })
            .await;
            match created {
                Ok(Ok(created)) => {
                    info!("publishing to regency.live");
                    producer = Some(created);
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "cannot publish to regency.live, live view off for now")
                }
                Err(_) => warn!("regency.live unreachable, live view off for now"),
            }
            if producer.is_none() {
                retry_at = tokio::time::Instant::now() + Duration::from_secs(15);
            }
        }
        let Some(current) = &producer else {
            dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let message = Message::builder().body(batch).build();
        if let Err(e) = current.send(message, |_| async {}).await {
            debug!(error = %e, "live batch not published");
            dropped.fetch_add(1, Ordering::Relaxed);
            producer = None;
        }
    }
    if let Some(producer) = producer {
        let _ = producer.close().await;
    }
}

/// Forward a run's live updates as batches to `link` (if any) and keep its
/// log up to `log_limit` bytes (0 = keep none). Ends when the run drops its
/// sink; returns the log.
pub async fn collect(
    mut updates: mpsc::Receiver<LiveUpdate>,
    link: Option<LiveLink>,
    log_limit: u64,
) -> RunLog {
    let mut log = RunLog::new(log_limit);
    let mut batch = ReportLiveRequest::new();
    let mut deadline: Option<tokio::time::Instant> = None;
    let flush = |batch: &mut ReportLiveRequest| {
        if batch.updates().is_empty() {
            return;
        }
        let full = std::mem::replace(batch, ReportLiveRequest::new());
        if let Some(link) = &link
            && let Ok(bytes) = full.serialize()
        {
            link.send(bytes);
        }
    };
    loop {
        let update = match deadline {
            None => updates.recv().await,
            Some(at) => tokio::select! {
                update = updates.recv() => update,
                _ = tokio::time::sleep_until(at) => {
                    flush(&mut batch);
                    deadline = None;
                    continue;
                }
            },
        };
        let Some(update) = update else { break };
        if link.is_some() {
            batch.updates_mut().push(update.to_proto());
            if batch.updates().len() >= BATCH_MAX {
                flush(&mut batch);
                deadline = None;
            } else if deadline.is_none() {
                deadline = Some(tokio::time::Instant::now() + BATCH_DELAY);
            }
        }
        if log_limit > 0 {
            log.push(update);
        }
    }
    flush(&mut batch);
    log
}

/// A run in live mode: the sink for the task and the collector of its
/// updates ([`collect`]).
pub fn start(
    link: Option<LiveLink>,
    log_limit: u64,
) -> (LiveSink, tokio::task::JoinHandle<RunLog>) {
    let (sink, updates) = LiveSink::channel(SINK_CAPACITY);
    (sink, tokio::spawn(collect(updates, link, log_limit)))
}

/// Apply regency's `run_log_max_bytes` (0 = keep none) to a `RunLog`
/// received from elsewhere (e.g. a proactive host): `None` when no log is to
/// be kept. A log within the limit is forwarded unchanged (unknown fields
/// included, D6); a larger one is cropped.
pub fn crop_run_log(
    message: &regent_sdk::proto::RunLog,
    limit: u64,
) -> Result<Option<Vec<u8>>, String> {
    if limit == 0 {
        return Ok(None);
    }
    if message.size_bytes() <= limit {
        return message.serialize().map(Some).map_err(|e| format!("{e:?}"));
    }
    let mut log = RunLog::from_proto(message).map_err(|e| e.to_string())?;
    log.crop(limit);
    log.to_proto_bytes().map(Some).map_err(|e| e.to_string())
}

/// Decode a batch of `regency.live`.
pub fn decode_batch(bytes: &[u8]) -> Result<ReportLiveRequest, String> {
    ReportLiveRequest::parse(bytes).map_err(|e| format!("{e:?}"))
}

/// Follow `regency.live` from `now - lookback` until `cancel`, calling
/// `on_batch` with each batch's bytes; resubscribes on errors.
/// `on_ready(true)` once subscribed.
pub async fn follow(
    config: RabbitMqConfig,
    secrets: SecretProvidersPool,
    lookback: Duration,
    cancel: CancellationToken,
    on_ready: impl Fn(bool),
    mut on_batch: impl FnMut(&[u8]),
) {
    let mut delay = Duration::from_millis(500);
    while !cancel.is_cancelled() {
        let subscribed = async {
            let environment = environment(&config, &secrets).await?;
            let start = chrono::Utc::now().timestamp_millis() - lookback.as_millis() as i64;
            environment
                .consumer()
                .offset(OffsetSpecification::Timestamp(start))
                .build(STREAM_LIVE)
                .await
                .map_err(|e| ControlError::Stream(e.to_string()))
        }
        .await;
        let mut consumer = match subscribed {
            Ok(consumer) => consumer,
            Err(e) => {
                on_ready(false);
                warn!(error = %e, retry_in_ms = delay.as_millis() as u64, "cannot follow regency.live");
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        info!(lookback_sec = lookback.as_secs(), "following regency.live");
        on_ready(true);
        delay = Duration::from_millis(500);
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => return,
                next = consumer.next() => next,
            };
            match next {
                Some(Ok(delivery)) => {
                    if let Some(data) = delivery.message().data() {
                        on_batch(data);
                    }
                }
                Some(Err(e)) => warn!(error = %e, "regency.live delivery error"),
                None => {
                    warn!("regency.live consumer ended, resubscribing");
                    on_ready(false);
                    break;
                }
            }
        }
    }
}

/// The `run_log_max_bytes` setting (D24), kept current from
/// `regency_settings`.
#[derive(Clone)]
pub struct RunLogLimit(Arc<AtomicU64>);

impl Default for RunLogLimit {
    fn default() -> Self {
        Self(Arc::new(AtomicU64::new(DEFAULT_RUN_LOG_MAX_BYTES)))
    }
}

impl RunLogLimit {
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, limit: u64) {
        self.0.store(limit, Ordering::Relaxed);
    }

    /// Follow the settings document until `cancel` (the default applies until
    /// it is read, and when it is unreadable).
    pub fn spawn_follower(&self, couch: Couch, cancel: CancellationToken) {
        let limit = self.clone();
        tokio::spawn(async move {
            crate::couch::follow(
                &couch,
                DB_SETTINGS,
                Since::Start,
                &cancel,
                |change| {
                    if change.id != SETTINGS_DOC_ID {
                        return;
                    }
                    let settings = if change.deleted {
                        SettingsDoc::default()
                    } else {
                        match change.doc.map(serde_json::from_value::<SettingsDoc>) {
                            Some(Ok(settings)) => settings,
                            _ => {
                                warn!("unreadable settings document, run log limit unchanged");
                                return;
                            }
                        }
                    };
                    let value = settings.run_log_max_bytes();
                    if value != limit.get() {
                        info!(run_log_max_bytes = value, "run log limit");
                    }
                    limit.set(value);
                },
                || {},
            )
            .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regent_sdk::hosts::live::{LiveEvent, OutputStream};
    use regent_sdk::task::Job;

    fn line(seq: u64, text: &str) -> LiveUpdate {
        LiveUpdate {
            run_id: "job-1".into(),
            host_id: "web-01".into(),
            job: Job::Assess,
            seq,
            at: chrono::Utc::now(),
            event: LiveEvent::Output {
                exec_id: 1,
                stream: OutputStream::Stdout,
                line: text.into(),
            },
        }
    }

    #[tokio::test]
    async fn collect_keeps_the_log_up_to_its_limit() {
        let (tx, rx) = mpsc::channel(16);
        let one = line(1, "0123456789").approx_size() as u64;
        let collector = tokio::spawn(collect(rx, None, 2 * one));
        for seq in 1..=3 {
            tx.send(line(seq, "0123456789")).await.unwrap();
        }
        drop(tx);
        let log = collector.await.unwrap();
        assert_eq!((log.updates().len(), log.omitted_updates()), (2, 1));
    }

    #[test]
    fn crop_run_log_respects_the_setting() {
        let mut log = RunLog::new(0);
        for seq in 1..=10 {
            log.push(line(seq, "0123456789"));
        }
        let message = log.to_proto();
        assert_eq!(crop_run_log(&message, 0).unwrap(), None, "0 = keep none");
        let one = line(1, "0123456789").approx_size() as u64;
        let cropped = crop_run_log(&message, 4 * one).unwrap().unwrap();
        let back = RunLog::from_proto_bytes(&cropped).unwrap();
        assert_eq!((back.updates().len(), back.omitted_updates()), (4, 6));
        assert_eq!(back.limit_bytes(), 4 * one);
        let mut small = RunLog::new(100 * one);
        small.push(line(1, "x"));
        let kept = crop_run_log(&small.to_proto(), 4 * one).unwrap().unwrap();
        assert_eq!(
            kept,
            small.to_proto_bytes().unwrap(),
            "within the limit: unchanged"
        );
    }
}
