//! RabbitMQ topology (D4, D7, D10, D11). Every service declares, idempotently
//! at startup, what it depends on.
//!
//! Dead-lettering: task queues dead-letter to `regency.dead` with routing key
//! `task` (queue `regency.dead`, read by dbwriter: `EXPIRED` / `FAILED`, D7);
//! other queues dead-letter with key `parked` to the queue
//! `regency.dead.parked`, kept for inspection (a poison outcome or command is
//! never silently lost).

use lapin::options::{ExchangeDeclareOptions, QueueBindOptions, QueueDeclareOptions};
use lapin::types::{FieldTable, ShortString};
use lapin::{Channel, ExchangeKind};

use crate::amqp::{long, string, table};

pub const EX_TASKS: &str = "regency.tasks";
pub const EX_OUTCOMES: &str = "regency.outcomes";
pub const EX_LIFECYCLE: &str = "regency.lifecycle";
pub const EX_COMMANDS: &str = "regency.commands";
pub const EX_DEAD: &str = "regency.dead";
/// Instance status (D21).
pub const EX_STATUS: &str = "regency.status";

pub const Q_OUTCOMES_DBWRITER: &str = "regency.outcomes.dbwriter";
pub const Q_LIFECYCLE_DBWRITER: &str = "regency.lifecycle.dbwriter";
pub const Q_COMMANDS_SCHEDULER: &str = "regency.commands.scheduler";
pub const Q_COMMANDS_DBWRITER: &str = "regency.commands.dbwriter";
pub const Q_STATUS_DBWRITER: &str = "regency.status.dbwriter";
pub const Q_DEAD: &str = "regency.dead";
pub const Q_DEAD_PARKED: &str = "regency.dead.parked";
pub const STREAM_CONTROL: &str = "regency.control";

pub const DEAD_KEY_TASK: &str = "task";
pub const DEAD_KEY_PARKED: &str = "parked";

/// Command kinds handled by dbwriter (D10).
pub const DBWRITER_COMMAND_KEYS: [&str; 7] = [
    "order.*",
    "settings.*",
    "host.*",
    "catalog.*",
    "user.*",
    "policy.*",
    "config.*",
];

/// Tunables of the topology.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyConfig {
    /// Reach partitions per pool (D4).
    #[serde(default = "default_partitions")]
    pub reach_partitions: u32,
    /// Quorum `delivery-limit` before dead-lettering (D4).
    #[serde(default = "default_delivery_limit")]
    pub delivery_limit: i64,
    /// Task queues' `consumer_timeout`, ms: above the longest realistic run.
    #[serde(default = "default_consumer_timeout")]
    pub task_consumer_timeout_ms: i64,
    /// `regency.control` retention, seconds (D7: >= runners' look-back).
    #[serde(default = "default_control_max_age")]
    pub control_max_age_sec: u64,
}

fn default_partitions() -> u32 {
    256
}
fn default_delivery_limit() -> i64 {
    5
}
fn default_consumer_timeout() -> i64 {
    4 * 3600 * 1000
}
fn default_control_max_age() -> u64 {
    2 * 86_400
}

impl Default for TopologyConfig {
    fn default() -> Self {
        Self {
            reach_partitions: default_partitions(),
            delivery_limit: default_delivery_limit(),
            task_consumer_timeout_ms: default_consumer_timeout(),
            control_max_age_sec: default_control_max_age(),
        }
    }
}

/// Assess queue of a pool.
pub fn assess_queue(pool: &str) -> String {
    format!("{EX_TASKS}.{pool}.assess")
}

pub fn assess_key(pool: &str) -> String {
    format!("{pool}.assess")
}

/// Reach partition queue `regency.tasks.<pool>.<P>.<N>`.
pub fn reach_queue(pool: &str, partitions: u32, partition: u32) -> String {
    format!("{EX_TASKS}.{pool}.{partitions}.{partition}")
}

pub fn reach_key(pool: &str, partitions: u32, partition: u32) -> String {
    format!("{pool}.{partitions}.{partition}")
}

fn s(value: &str) -> ShortString {
    ShortString::from(value)
}

async fn exchange(channel: &Channel, name: &str, kind: ExchangeKind) -> lapin::Result<()> {
    channel
        .exchange_declare(
            s(name),
            kind,
            ExchangeDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
}

/// Declare all exchanges (cheap; every service calls it).
pub async fn declare_exchanges(channel: &Channel) -> lapin::Result<()> {
    for name in [
        EX_TASKS,
        EX_OUTCOMES,
        EX_LIFECYCLE,
        EX_COMMANDS,
        EX_DEAD,
        EX_STATUS,
    ] {
        exchange(channel, name, ExchangeKind::Topic).await?;
    }
    Ok(())
}

async fn quorum(channel: &Channel, name: &str, mut arguments: FieldTable) -> lapin::Result<()> {
    arguments.insert(s("x-queue-type"), string("quorum"));
    channel
        .queue_declare(
            s(name),
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            arguments,
        )
        .await
        .map(|_| ())
}

async fn bind(channel: &Channel, queue: &str, exchange: &str, key: &str) -> lapin::Result<()> {
    channel
        .queue_bind(
            s(queue),
            s(exchange),
            s(key),
            QueueBindOptions::default(),
            FieldTable::default(),
        )
        .await
}

fn parked(config: &TopologyConfig) -> FieldTable {
    table(&[
        ("x-dead-letter-exchange", string(EX_DEAD)),
        ("x-dead-letter-routing-key", string(DEAD_KEY_PARKED)),
        ("x-delivery-limit", long(config.delivery_limit)),
    ])
}

/// A consumer's queue on an exchange, with parked dead-lettering.
async fn consumer_queue(
    channel: &Channel,
    config: &TopologyConfig,
    queue: &str,
    exchange: &str,
    keys: &[&str],
) -> lapin::Result<()> {
    quorum(channel, queue, parked(config)).await?;
    for key in keys {
        bind(channel, queue, exchange, key).await?;
    }
    Ok(())
}

/// Dead-letter queues.
pub async fn declare_dead(channel: &Channel) -> lapin::Result<()> {
    declare_exchanges(channel).await?;
    quorum(channel, Q_DEAD, FieldTable::default()).await?;
    bind(channel, Q_DEAD, EX_DEAD, DEAD_KEY_TASK).await?;
    quorum(channel, Q_DEAD_PARKED, FieldTable::default()).await?;
    bind(channel, Q_DEAD_PARKED, EX_DEAD, DEAD_KEY_PARKED).await
}

fn task_arguments(config: &TopologyConfig) -> FieldTable {
    table(&[
        ("x-dead-letter-exchange", string(EX_DEAD)),
        ("x-dead-letter-routing-key", string(DEAD_KEY_TASK)),
        ("x-delivery-limit", long(config.delivery_limit)),
        // dead-letter expired tasks (D4: recorded as EXPIRED)
        ("x-dead-letter-strategy", string("at-least-once")),
        ("x-overflow", string("reject-publish")),
        ("x-consumer-timeout", long(config.task_consumer_timeout_ms)),
    ])
}

/// Task queues of a pool: shared assess queue + every Reach partition
/// (single-active-consumer).
pub async fn declare_pool(
    channel: &Channel,
    config: &TopologyConfig,
    pool: &str,
) -> lapin::Result<()> {
    declare_dead(channel).await?;
    quorum(channel, &assess_queue(pool), task_arguments(config)).await?;
    bind(channel, &assess_queue(pool), EX_TASKS, &assess_key(pool)).await?;
    for partition in 0..config.reach_partitions {
        let mut arguments = task_arguments(config);
        arguments.insert(
            s("x-single-active-consumer"),
            lapin::types::AMQPValue::Boolean(true),
        );
        let queue = reach_queue(pool, config.reach_partitions, partition);
        quorum(channel, &queue, arguments).await?;
        bind(
            channel,
            &queue,
            EX_TASKS,
            &reach_key(pool, config.reach_partitions, partition),
        )
        .await?;
    }
    Ok(())
}

pub async fn declare_outcomes_dbwriter(
    channel: &Channel,
    config: &TopologyConfig,
) -> lapin::Result<()> {
    declare_dead(channel).await?;
    consumer_queue(channel, config, Q_OUTCOMES_DBWRITER, EX_OUTCOMES, &["#"]).await
}

pub async fn declare_lifecycle_dbwriter(
    channel: &Channel,
    config: &TopologyConfig,
) -> lapin::Result<()> {
    declare_dead(channel).await?;
    consumer_queue(channel, config, Q_LIFECYCLE_DBWRITER, EX_LIFECYCLE, &["#"]).await
}

pub async fn declare_commands_dbwriter(
    channel: &Channel,
    config: &TopologyConfig,
) -> lapin::Result<()> {
    declare_dead(channel).await?;
    consumer_queue(
        channel,
        config,
        Q_COMMANDS_DBWRITER,
        EX_COMMANDS,
        &DBWRITER_COMMAND_KEYS,
    )
    .await
}

/// Instance statuses (D21): a status older than the next one is useless, so
/// the queue keeps few messages (newest wins in dbwriter anyway).
pub async fn declare_status_dbwriter(
    channel: &Channel,
    config: &TopologyConfig,
) -> lapin::Result<()> {
    declare_dead(channel).await?;
    let mut arguments = parked(config);
    arguments.insert(s("x-max-length"), long(10_000));
    arguments.insert(s("x-overflow"), string("drop-head"));
    quorum(channel, Q_STATUS_DBWRITER, arguments).await?;
    bind(channel, Q_STATUS_DBWRITER, EX_STATUS, "#").await
}

pub async fn declare_commands_scheduler(
    channel: &Channel,
    config: &TopologyConfig,
) -> lapin::Result<()> {
    declare_dead(channel).await?;
    consumer_queue(channel, config, Q_COMMANDS_SCHEDULER, EX_COMMANDS, &["run"]).await
}

/// Per-process `source.refresh` queue of a sourcesync replica (D11):
/// exclusive, auto-delete. Returns its server-generated name.
pub async fn declare_refresh_queue(channel: &Channel) -> lapin::Result<String> {
    declare_exchanges(channel).await?;
    let queue = channel
        .queue_declare(
            s(""),
            QueueDeclareOptions {
                exclusive: true,
                auto_delete: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await?;
    let name = queue.name().to_string();
    bind(channel, &name, EX_COMMANDS, "source.refresh").await?;
    Ok(name)
}

/// The replicated stream `regency.control` (D7).
pub async fn declare_control_stream(
    channel: &Channel,
    config: &TopologyConfig,
) -> lapin::Result<()> {
    channel
        .queue_declare(
            s(STREAM_CONTROL),
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            table(&[
                ("x-queue-type", string("stream")),
                (
                    "x-max-age",
                    string(&format!("{}s", config.control_max_age_sec)),
                ),
            ]),
        )
        .await
        .map(|_| ())
}
