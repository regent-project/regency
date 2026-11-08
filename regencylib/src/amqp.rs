//! RabbitMQ connection and confirmed publishing (D4).
//!
//! No automatic recovery: a service that loses its connection stops what
//! depends on it (D3/D4 safety rules) and reconnects explicitly.

use std::time::Duration;

use futures::StreamExt;
use lapin::options::{BasicPublishOptions, ConfirmSelectOptions};
use lapin::types::{AMQPValue, FieldTable, LongString, ShortString};
use lapin::{BasicProperties, Channel, Confirmation, Connection, ConnectionProperties};
use regent_sdk::secrets::SecretProvidersPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::{RabbitMqConfig, resolve_secret};

#[derive(Debug, thiserror::Error)]
pub enum AmqpError {
    #[error("RabbitMQ: {0}")]
    Lapin(#[from] lapin::Error),
    #[error("RabbitMQ configuration: {0}")]
    Config(String),
    #[error("message to {exchange}/{routing_key} was not routed to any queue")]
    Unroutable {
        exchange: String,
        routing_key: String,
    },
    #[error("message to {exchange}/{routing_key} was refused by the broker (nack)")]
    Nacked {
        exchange: String,
        routing_key: String,
    },
}

/// AMQP URI with credentials and heartbeat.
pub async fn uri(
    config: &RabbitMqConfig,
    secrets: &SecretProvidersPool,
) -> Result<String, AmqpError> {
    let mut url =
        url::Url::parse(&config.url).map_err(|e| AmqpError::Config(format!("url: {e}")))?;
    if let Some(username) = &config.username {
        url.set_username(username)
            .map_err(|_| AmqpError::Config("url cannot hold a username".into()))?;
    }
    if let Some(reference) = &config.password {
        let password = resolve_secret(secrets, reference)
            .await
            .map_err(|e| AmqpError::Config(e.to_string()))?;
        url.set_password(Some(&password))
            .map_err(|_| AmqpError::Config("url cannot hold a password".into()))?;
    }
    url.query_pairs_mut()
        .append_pair("heartbeat", &config.heartbeat_sec.to_string());
    Ok(url.to_string())
}

/// Connect, naming the connection `<service>/<instance>` (visible in the
/// management UI).
pub async fn connect(
    config: &RabbitMqConfig,
    secrets: &SecretProvidersPool,
    name: &str,
) -> Result<Connection, AmqpError> {
    let uri = uri(config, secrets).await?;
    let properties = ConnectionProperties::default().with_connection_name(LongString::from(name));
    // TLS (amqps://): explicit trust anchor and client identity (D19)
    let material = config
        .tls(secrets)
        .await
        .map_err(|e| AmqpError::Config(e.to_string()))?;
    let tls = lapin::tcp::OwnedTLSConfig {
        cert_chain: material
            .ca_pem
            .map(|pem| String::from_utf8_lossy(&pem).into_owned()),
        identity: match (material.cert_pem, material.key_pem) {
            (Some(pem), Some(key)) => Some(lapin::tcp::OwnedIdentity::PKCS8 {
                pem,
                key: key.into_bytes(),
            }),
            _ => None,
        },
    };
    let runtime = lapin::runtime::default_runtime()?;
    let connection = Connection::connect_with_config(&uri, properties, tls, runtime).await?;
    info!(name, tls = config.uses_tls(), "connected to RabbitMQ");
    Ok(connection)
}

/// Connect, retrying with capped exponential backoff until `cancel`.
pub async fn connect_retrying(
    config: &RabbitMqConfig,
    secrets: &SecretProvidersPool,
    name: &str,
    cancel: &CancellationToken,
) -> Option<Connection> {
    let mut delay = Duration::from_millis(500);
    loop {
        match connect(config, secrets, name).await {
            Ok(connection) => return Some(connection),
            Err(e) => {
                warn!(error = %e, retry_in_ms = delay.as_millis() as u64, "RabbitMQ connection failed")
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return None,
            _ = tokio::time::sleep(delay + jitter(delay / 4)) => {}
        }
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

/// Random duration in `[0, max)`.
pub fn jitter(max: Duration) -> Duration {
    let millis = max.as_millis() as u64;
    if millis == 0 {
        return Duration::ZERO;
    }
    Duration::from_millis(rand::random_range(0..millis))
}

/// Close `connection`, giving up after 5 s: on a broken connection lapin's
/// close may never resolve, and a service stuck there would neither
/// reconnect nor stop. Dropping the connection afterwards is enough.
pub async fn close(connection: &Connection, code: u16, reason: &str) {
    match tokio::time::timeout(
        Duration::from_secs(5),
        connection.close(code, ShortString::from(reason)),
    )
    .await
    {
        Ok(_) => {}
        Err(_) => warn!(reason, "RabbitMQ connection close timed out, dropping it"),
    }
}

/// Is this a channel-level error (an AMQP soft error, e.g. the 405
/// `RESOURCE_LOCKED` of a lock queue held elsewhere, D3)? lapin reports
/// those on the connection's events too, but the connection is still fine.
fn channel_level(error: &lapin::Error) -> bool {
    matches!(
        error.kind(),
        lapin::ErrorKind::ProtocolError(e)
            if matches!(e.kind(), lapin::protocol::AMQPErrorKind::Soft(_))
    )
}

/// Resolves when the connection reports an error or closes (channel-level
/// errors are not connection errors).
pub async fn closed(connection: &Connection) {
    let mut events = connection.events_listener();
    loop {
        if !connection.status().connected() {
            return;
        }
        tokio::select! {
            event = events.next() => match event {
                Some(lapin::Event::Error(e)) if channel_level(&e) => {
                    debug!(error = %e, "RabbitMQ channel closed by the broker");
                }
                Some(lapin::Event::Error(e)) => {
                    warn!(error = %e, "RabbitMQ connection error");
                    return;
                }
                None => return,
                Some(_) => {}
            },
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

/// A channel in confirm mode; every publish is `mandatory` and waits for the
/// broker's confirm (D4).
#[derive(Clone)]
pub struct Publisher {
    channel: Channel,
}

impl Publisher {
    pub async fn new(connection: &Connection) -> Result<Self, AmqpError> {
        let channel = connection.create_channel().await?;
        channel
            .confirm_select(ConfirmSelectOptions::default())
            .await?;
        Ok(Self { channel })
    }

    /// Resolves when the publisher's channel is closed (e.g. by the broker
    /// refusing a declaration on it). Channel-level errors do not end the
    /// connection ([`closed`]), so a service holding a publisher must watch
    /// this too and renew its connection, or it could never publish again.
    pub async fn closed(&self) {
        while self.channel.status().connected() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        warn!("RabbitMQ publisher channel closed, renewing the connection");
    }

    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// Publish and wait for the confirm. Persistent delivery mode.
    pub async fn publish(
        &self,
        exchange: &str,
        routing_key: &str,
        payload: &[u8],
        properties: BasicProperties,
    ) -> Result<(), AmqpError> {
        let properties = properties.with_delivery_mode(2);
        let confirm = self
            .channel
            .basic_publish(
                ShortString::from(exchange),
                ShortString::from(routing_key),
                BasicPublishOptions {
                    mandatory: true,
                    ..Default::default()
                },
                payload,
                properties,
            )
            .await?
            .await?;
        match confirm {
            Confirmation::Ack(None) | Confirmation::NotRequested => Ok(()),
            Confirmation::Ack(Some(_)) => Err(AmqpError::Unroutable {
                exchange: exchange.to_string(),
                routing_key: routing_key.to_string(),
            }),
            Confirmation::Nack(_) => Err(AmqpError::Nacked {
                exchange: exchange.to_string(),
                routing_key: routing_key.to_string(),
            }),
        }
    }

    /// Publish JSON with `content_type` set.
    pub async fn publish_json(
        &self,
        exchange: &str,
        routing_key: &str,
        payload: &[u8],
    ) -> Result<(), AmqpError> {
        self.publish(
            exchange,
            routing_key,
            payload,
            BasicProperties::default().with_content_type(ShortString::from("application/json")),
        )
        .await
    }
}

/// Field-table helpers.
pub fn table(entries: &[(&str, AMQPValue)]) -> FieldTable {
    let mut table = FieldTable::default();
    for (key, value) in entries {
        table.insert(ShortString::from(*key), value.clone());
    }
    table
}

pub fn string(value: &str) -> AMQPValue {
    AMQPValue::LongString(LongString::from(value))
}

pub fn long(value: i64) -> AMQPValue {
    AMQPValue::LongLongInt(value)
}

/// First `x-death` entry of a dead-lettered message: (queue, reason).
pub fn first_death(properties: &BasicProperties) -> Option<(String, String)> {
    let headers = properties.headers().as_ref()?;
    let deaths = match headers.inner().get(&ShortString::from("x-death"))? {
        AMQPValue::FieldArray(array) => array.as_slice().to_vec(),
        _ => return None,
    };
    let first = match deaths.first()? {
        AMQPValue::FieldTable(table) => table.clone(),
        _ => return None,
    };
    let text = |key: &str| match first.inner().get(&ShortString::from(key)) {
        Some(AMQPValue::LongString(s)) => Some(s.to_string()),
        Some(AMQPValue::ShortString(s)) => Some(s.to_string()),
        _ => None,
    };
    Some((text("queue")?, text("reason")?))
}

/// A publisher kept connected in the background: `None` while disconnected
/// (callers answer "unavailable" then). Declares the exchanges on every
/// (re)connection and reports readiness as `rabbitmq` on `health`.
#[derive(Clone)]
pub struct PublisherLink {
    current: tokio::sync::watch::Receiver<Option<Publisher>>,
}

impl PublisherLink {
    pub fn spawn(
        config: crate::config::RabbitMqConfig,
        secrets: SecretProvidersPool,
        name: String,
        health: crate::telemetry::Health,
        cancel: CancellationToken,
    ) -> Self {
        let (tx, rx) = tokio::sync::watch::channel(None);
        health.register("rabbitmq");
        tokio::spawn(async move {
            while let Some(connection) = connect_retrying(&config, &secrets, &name, &cancel).await {
                let setup = async {
                    let publisher = Publisher::new(&connection).await?;
                    crate::topology::declare_exchanges(publisher.channel()).await?;
                    Ok::<_, AmqpError>(publisher)
                };
                match setup.await {
                    Ok(publisher) => {
                        tx.send_replace(Some(publisher.clone()));
                        health.ready("rabbitmq");
                        tokio::select! {
                            _ = cancel.cancelled() => {}
                            _ = closed(&connection) => {}
                            _ = publisher.closed() => {}
                        }
                    }
                    Err(e) => warn!(error = %e, "RabbitMQ setup failed"),
                }
                tx.send_replace(None);
                health.not_ready("rabbitmq", "disconnected");
                close(&connection, 200, "bye").await;
                if cancel.is_cancelled() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        Self { current: rx }
    }

    /// The connected publisher, if any.
    pub fn get(&self) -> Option<Publisher> {
        self.current.borrow().clone()
    }

    /// Wait (at most `timeout`) for a connected publisher.
    pub async fn wait(&self, timeout: Duration) -> Option<Publisher> {
        let mut rx = self.current.clone();
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(publisher) = rx.borrow_and_update().clone() {
                    return Some(publisher);
                }
                if rx.changed().await.is_err() {
                    return None;
                }
            }
        })
        .await
        .ok()
        .flatten()
    }
}

/// What to do with a delivery once handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    Ack,
    /// Poison message: reject without requeue (dead-lettered).
    Reject,
    /// Give it back (e.g. shutting down mid-way); counts towards the quorum
    /// delivery limit.
    Requeue,
}

/// Consume `queue` with at most `concurrency` messages in flight (prefetch),
/// settling each according to `handler`. Returns when the consumer ends
/// (connection lost) or on `cancel` (in-flight messages are finished first).
pub async fn consume<F, Fut>(
    connection: &Connection,
    queue: &str,
    consumer_tag: &str,
    concurrency: u16,
    cancel: &CancellationToken,
    handler: F,
) -> Result<(), AmqpError>
where
    F: Fn(lapin::message::Delivery) -> Fut,
    Fut: std::future::Future<Output = Handled>,
{
    let channel = connection.create_channel().await?;
    let started = async {
        channel
            .basic_qos(concurrency, lapin::options::BasicQosOptions::default())
            .await?;
        channel
            .basic_consume(
                ShortString::from(queue),
                ShortString::from(consumer_tag),
                lapin::options::BasicConsumeOptions::default(),
                FieldTable::default(),
            )
            .await
    };
    let consumer = match started.await {
        Ok(consumer) => consumer,
        Err(e) => {
            // never leave the channel open (lapin's per-connection limit)
            let _ = channel.close(200, ShortString::from("failed")).await;
            return Err(e.into());
        }
    };
    info!(queue, concurrency, "consuming");
    let stream = consumer.take_until(cancel.cancelled());
    stream
        .for_each_concurrent(usize::from(concurrency), |delivery| async {
            let delivery = match delivery {
                Ok(delivery) => delivery,
                Err(e) => {
                    warn!(queue, error = %e, "consumer error");
                    return;
                }
            };
            let acker = delivery.acker.clone();
            let settled = match handler(delivery).await {
                Handled::Ack => acker.ack(lapin::options::BasicAckOptions::default()).await,
                Handled::Reject => {
                    acker
                        .reject(lapin::options::BasicRejectOptions { requeue: false })
                        .await
                }
                Handled::Requeue => {
                    acker
                        .nack(lapin::options::BasicNackOptions {
                            requeue: true,
                            ..Default::default()
                        })
                        .await
                }
            };
            if let Err(e) = settled {
                warn!(queue, error = %e, "cannot settle a delivery (it will be redelivered)");
            }
        })
        .await;
    let _ = channel.close(200, ShortString::from("done")).await;
    Ok(())
}

/// Retry `operation` with capped exponential backoff while it fails with a
/// transient error, until `cancel` (then `None`). Permanent errors (as
/// classified by `is_transient`) are returned at once.
pub async fn retry_transient<T, E, F, Fut>(
    what: &str,
    cancel: &CancellationToken,
    is_transient: impl Fn(&E) -> bool,
    mut operation: F,
) -> Option<Result<T, E>>
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut delay = Duration::from_millis(200);
    loop {
        match operation().await {
            Err(e) if is_transient(&e) => {
                warn!(operation = what, error = %e, retry_in_ms = delay.as_millis() as u64, "transient failure, retrying");
                tokio::select! {
                    _ = cancel.cancelled() => return None,
                    _ = tokio::time::sleep(delay + jitter(delay / 4)) => {}
                }
                delay = (delay * 2).min(Duration::from_secs(30));
            }
            other => return Some(other),
        }
    }
}

#[cfg(test)]
mod closed_tests {
    use super::channel_level;
    use lapin::protocol::{AMQPError, AMQPErrorKind, AMQPHardError, AMQPSoftError};

    fn protocol(kind: AMQPErrorKind) -> lapin::Error {
        lapin::ErrorKind::ProtocolError(AMQPError::new(kind, "test".into())).into()
    }

    #[test]
    fn lock_contention_is_not_a_connection_error() {
        assert!(channel_level(&protocol(AMQPErrorKind::Soft(
            AMQPSoftError::RESOURCELOCKED
        ))));
        assert!(!channel_level(&protocol(AMQPErrorKind::Hard(
            AMQPHardError::CONNECTIONFORCED
        ))));
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        assert!(!channel_level(
            &lapin::ErrorKind::IOError(std::sync::Arc::new(io)).into()
        ));
    }
}
