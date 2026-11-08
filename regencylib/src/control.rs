//! The abort stream `regency.control` (D7), over the RabbitMQ stream
//! protocol: backend appends [`AbortRequest`]s, runners follow it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use rabbitmq_stream_client::types::{Message, OffsetSpecification};
use rabbitmq_stream_client::{Environment, NoDedup, Producer};
use regent_sdk::secrets::SecretProvidersPool;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::{RabbitMqConfig, resolve_secret};
use crate::messages::AbortRequest;
use crate::topology::STREAM_CONTROL;

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("RabbitMQ stream: {0}")]
    Stream(String),
    #[error("configuration: {0}")]
    Config(String),
}

/// Stream-protocol environment from the `[rabbitmq]` section.
pub async fn environment(
    config: &RabbitMqConfig,
    secrets: &SecretProvidersPool,
) -> Result<Environment, ControlError> {
    let mut builder = Environment::builder()
        .host(&config.stream_host)
        .port(config.stream_port)
        .virtual_host(&config.vhost);
    if let Some(username) = &config.username {
        builder = builder.username(username);
    }
    // TLS follows the AMQP URL (D19): without a CA the stream client does
    // not verify the broker, which the level checks forbid from level 2
    if config.uses_tls() {
        let material = config
            .tls(secrets)
            .await
            .map_err(|e| ControlError::Config(e.to_string()))?;
        let mut tls = rabbitmq_stream_client::TlsConfiguration::builder().enable(true);
        if let Some(ca_file) = &config.ca_file {
            tls = tls.add_root_certificates(ca_file);
        }
        if let (Some(cert_file), Some(key_file)) = (
            &config.cert_file,
            material
                .key_file("stream-client.key")
                .map_err(|e| ControlError::Config(e.to_string()))?,
        ) {
            tls = tls.add_client_certificates_keys(cert_file, key_file);
        }
        builder = builder.tls(
            tls.build()
                .map_err(|e| ControlError::Config(e.to_string()))?,
        );
    }
    let password;
    if let Some(reference) = &config.password {
        password = resolve_secret(secrets, reference)
            .await
            .map_err(|e| ControlError::Config(e.to_string()))?;
        builder = builder.password(&password);
    }
    builder
        .build()
        .await
        .map_err(|e| ControlError::Stream(e.to_string()))
}

/// Appends abort requests (backend).
pub struct AbortPublisher {
    producer: Producer<NoDedup>,
}

impl AbortPublisher {
    pub async fn new(environment: &Environment) -> Result<Self, ControlError> {
        let producer = environment
            .producer()
            .build(STREAM_CONTROL)
            .await
            .map_err(|e| ControlError::Stream(e.to_string()))?;
        Ok(Self { producer })
    }

    pub async fn publish(&self, request: &AbortRequest) -> Result<(), ControlError> {
        let body = request
            .to_bytes()
            .map_err(|e| ControlError::Stream(e.to_string()))?;
        let status = self
            .producer
            .send_with_confirm(Message::builder().body(body).build())
            .await
            .map_err(|e| ControlError::Stream(e.to_string()))?;
        if status.confirmed() {
            Ok(())
        } else {
            Err(ControlError::Stream(
                "abort request not confirmed".to_string(),
            ))
        }
    }
}

/// What a runner knows about aborts: an expiring set of aborted job ids,
/// plus the cancellation tokens of its running tasks.
#[derive(Clone)]
pub struct Aborts {
    inner: Arc<Mutex<AbortsInner>>,
    retention: Duration,
    ids: Arc<AtomicU64>,
}

struct AbortsInner {
    aborted: HashMap<String, Instant>,
    running: HashMap<String, Vec<(u64, CancellationToken)>>,
}

impl Aborts {
    /// `retention`: how long an abort is remembered (the look-back window).
    pub fn new(retention: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(AbortsInner {
                aborted: HashMap::new(),
                running: HashMap::new(),
            })),
            retention,
            ids: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Record an abort; cancels the task if it runs here. Returns whether it
    /// was running.
    pub fn abort(&self, job_id: &str) -> bool {
        let mut inner = self.inner.lock().expect("aborts lock");
        let now = Instant::now();
        let retention = self.retention;
        inner
            .aborted
            .retain(|_, at| now.duration_since(*at) < retention);
        inner.aborted.insert(job_id.to_string(), now);
        match inner.running.get(job_id) {
            Some(tokens) => {
                for (_, token) in tokens {
                    token.cancel();
                }
                true
            }
            None => false,
        }
    }

    pub fn is_aborted(&self, job_id: &str) -> bool {
        self.inner
            .lock()
            .expect("aborts lock")
            .aborted
            .contains_key(job_id)
    }

    /// Register a running task; its token is cancelled by a later abort.
    /// `None` if the job is already aborted.
    pub fn start(&self, job_id: &str, token: CancellationToken) -> Option<RunningGuard> {
        let mut inner = self.inner.lock().expect("aborts lock");
        if inner.aborted.contains_key(job_id) {
            return None;
        }
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        inner
            .running
            .entry(job_id.to_string())
            .or_default()
            .push((id, token));
        Some(RunningGuard {
            aborts: self.clone(),
            job_id: job_id.to_string(),
            id,
        })
    }
}

/// Unregisters a running task when dropped.
pub struct RunningGuard {
    aborts: Aborts,
    job_id: String,
    id: u64,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        let mut inner = self.aborts.inner.lock().expect("aborts lock");
        if let Some(tokens) = inner.running.get_mut(&self.job_id) {
            tokens.retain(|(id, _)| *id != self.id);
            if tokens.is_empty() {
                inner.running.remove(&self.job_id);
            }
        }
    }
}

/// Follow `regency.control` from `now - lookback`, feeding `aborts`, until
/// `cancel`; resubscribes on errors. `on_ready(true)` once subscribed.
pub async fn follow(
    config: RabbitMqConfig,
    secrets: SecretProvidersPool,
    aborts: Aborts,
    lookback: Duration,
    cancel: CancellationToken,
    on_ready: impl Fn(bool),
) {
    let mut delay = Duration::from_millis(500);
    while !cancel.is_cancelled() {
        let subscribed = async {
            let environment = environment(&config, &secrets).await?;
            let start = chrono::Utc::now().timestamp_millis() - lookback.as_millis() as i64;
            environment
                .consumer()
                .offset(OffsetSpecification::Timestamp(start))
                .build(STREAM_CONTROL)
                .await
                .map_err(|e| ControlError::Stream(e.to_string()))
        }
        .await;
        let mut consumer = match subscribed {
            Ok(consumer) => consumer,
            Err(e) => {
                on_ready(false);
                warn!(error = %e, retry_in_ms = delay.as_millis() as u64, "cannot follow regency.control");
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        info!(
            lookback_sec = lookback.as_secs(),
            "following regency.control"
        );
        on_ready(true);
        delay = Duration::from_millis(500);
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => return,
                next = consumer.next() => next,
            };
            match next {
                Some(Ok(delivery)) => {
                    let Some(data) = delivery.message().data() else {
                        continue;
                    };
                    match AbortRequest::from_bytes(data) {
                        Ok(request) => {
                            let running = aborts.abort(&request.job_id);
                            info!(job_id = %request.job_id, principal = %request.requested_by, running, "abort requested");
                        }
                        Err(e) => warn!(error = %e, "unreadable abort request skipped"),
                    }
                }
                Some(Err(e)) => warn!(error = %e, "regency.control delivery error"),
                None => {
                    warn!("regency.control consumer ended, resubscribing");
                    on_ready(false);
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_cancels_running_and_blocks_queued() {
        let aborts = Aborts::new(Duration::from_secs(60));
        let token = CancellationToken::new();
        let guard = aborts.start("k1", token.clone()).unwrap();
        assert!(aborts.abort("k1"));
        assert!(token.is_cancelled());
        drop(guard);
        assert!(aborts.start("k1", CancellationToken::new()).is_none());
        assert!(!aborts.abort("k2"));
        assert!(aborts.is_aborted("k2"));
    }
}
