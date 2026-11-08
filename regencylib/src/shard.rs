//! Shard ownership through RabbitMQ exclusive queues (D3), shared by the
//! scheduler (orders), runners (Reach partitions, D4) and sourcesync (D11).
//!
//! - lock: declaring the exclusive queue `<prefix>.<count>.<item>-<N>` succeeds
//!   for exactly one connection; others get `RESOURCE_LOCKED` (which closes
//!   their channel, so every attempt uses a disposable channel). The lock is
//!   released when the holder's connection closes, or by deleting the queue;
//! - membership: every member consumes from the shared `<members>` queue; its
//!   `consumer_count` is the number of live members;
//! - balance: a member holds at most `ceil(count / live)` locks and releases
//!   the extras;
//! - watchdog: if the owner's work loop stops ticking, the connection is closed
//!   on purpose, which releases every lock (heartbeats alone would keep a hung
//!   process's locks).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use lapin::Connection;
use lapin::options::{BasicConsumeOptions, QueueDeclareOptions, QueueDeleteOptions};
use lapin::types::{FieldTable, ShortString};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::amqp::{jitter, string, table};

/// What is sharded and how.
#[derive(Debug, Clone)]
pub struct ShardSpec {
    /// e.g. `scheduler.lock`, `runner.lock.default`.
    pub lock_prefix: String,
    /// `shard` or `part`.
    pub item: String,
    /// e.g. `scheduler.members`, `runner.members.default`.
    pub members_queue: String,
    pub count: u32,
    /// Base period of acquisition/balancing rounds (jittered).
    pub retry: Duration,
    /// Max time between two watchdog ticks before giving up every lock.
    pub watchdog_timeout: Duration,
}

impl ShardSpec {
    pub fn lock_queue(&self, shard: u32) -> String {
        format!(
            "{}.{}.{}-{}",
            self.lock_prefix, self.count, self.item, shard
        )
    }
}

/// Liveness signal from the owner's work loop.
#[derive(Clone)]
pub struct Watchdog(Arc<AtomicI64>);

impl Default for Watchdog {
    fn default() -> Self {
        Self::new()
    }
}

impl Watchdog {
    pub fn new() -> Self {
        Self(Arc::new(AtomicI64::new(now_ms())))
    }

    pub fn tick(&self) {
        self.0.store(now_ms(), Ordering::Relaxed);
    }

    fn stale(&self, timeout: Duration) -> bool {
        now_ms() - self.0.load(Ordering::Relaxed) > timeout.as_millis() as i64
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Target number of locks per member.
pub fn fair_share(count: u32, live: u32) -> usize {
    count.div_ceil(live.max(1)) as usize
}

/// How many lock attempts one round makes. Every refused attempt costs a
/// channel the broker closes (which lapin does not always free), and with a
/// share that does not divide evenly (256 shards, 10 members: nine hold 26,
/// one holds 22) a member can stay under its share forever while every
/// shard is held. So after a full sweep that acquired nothing, probe one
/// shard per round, until the membership changes or a probe succeeds.
fn probe_budget(count: u32, exhausted: bool) -> u32 {
    if exhausted { 1 } else { count }
}

/// Hold shards on `connection` until it fails, the watchdog fires or `cancel`.
/// The owned set is published on `owned` (emptied **before** returning, so
/// users stop at once, D3). The caller reconnects and calls it again.
pub async fn run(
    spec: &ShardSpec,
    connection: &Connection,
    watchdog: &Watchdog,
    owned: &watch::Sender<BTreeSet<u32>>,
    cancel: &CancellationToken,
) -> Result<(), lapin::Error> {
    let result = hold(spec, connection, watchdog, owned, cancel).await;
    owned.send_replace(BTreeSet::new());
    result
}

async fn hold(
    spec: &ShardSpec,
    connection: &Connection,
    watchdog: &Watchdog,
    owned: &watch::Sender<BTreeSet<u32>>,
    cancel: &CancellationToken,
) -> Result<(), lapin::Error> {
    // membership
    let members = connection.create_channel().await?;
    let declared = members
        .queue_declare(
            ShortString::from(spec.members_queue.as_str()),
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            table(&[("x-queue-type", string("quorum"))]),
        )
        .await?;
    if declared.consumer_count() == 0 {
        // Probably just created: consuming from a quorum queue before its
        // replica process runs fails with `noproc`, a hard error closing the
        // whole connection. Give it time to start.
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(1500) + jitter(Duration::from_millis(500))) => {}
        }
    }
    let _membership = members
        .basic_consume(
            ShortString::from(spec.members_queue.as_str()),
            ShortString::from(""),
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await?;

    let mut held: BTreeSet<u32> = BTreeSet::new();
    // (live members, last full sweep acquired nothing), see probe_budget
    let mut sweep: (u32, bool) = (0, false);
    watchdog.tick();
    loop {
        if watchdog.stale(spec.watchdog_timeout) {
            warn!(prefix = %spec.lock_prefix, "watchdog: work loop stalled, dropping the connection and every lock");
            crate::amqp::close(connection, 0, "watchdog").await;
            return Ok(());
        }
        if !connection.status().connected() {
            // the caller reconnects either way
            return Ok(());
        }

        let live = members
            .queue_declare(
                ShortString::from(spec.members_queue.as_str()),
                QueueDeclareOptions {
                    passive: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await?
            .consumer_count();
        let target = fair_share(spec.count, live);

        // release extras (highest numbers first: deterministic)
        while held.len() > target {
            let shard = *held.iter().next_back().expect("non-empty");
            held.remove(&shard);
            owned.send_replace(held.clone());
            let channel = connection.create_channel().await?;
            channel
                .queue_delete(
                    ShortString::from(spec.lock_queue(shard).as_str()),
                    QueueDeleteOptions::default(),
                )
                .await?;
            let _ = channel.close(200, ShortString::from("released")).await;
            info!(prefix = %spec.lock_prefix, shard, live, "released");
        }

        // acquire up to the fair share, from a random offset
        if held.len() < target && spec.count > 0 {
            let offset = rand::random_range(0..spec.count);
            if sweep.0 != live {
                sweep = (live, false);
            }
            let attempts = probe_budget(spec.count, sweep.1);
            let before = held.len();
            for i in 0..attempts {
                if held.len() >= target || cancel.is_cancelled() {
                    break;
                }
                let shard = (offset + i) % spec.count;
                if held.contains(&shard) {
                    continue;
                }
                if try_lock(connection, &spec.lock_queue(shard)).await? {
                    held.insert(shard);
                    debug!(prefix = %spec.lock_prefix, shard, "acquired");
                }
            }
            // a full sweep (or a probe) that got nothing: probe from now on;
            // anything acquired: sweep again next round
            sweep.1 = held.len() == before && !cancel.is_cancelled();
            owned.send_replace(held.clone());
        }

        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep(spec.retry + jitter(spec.retry / 2)) => {}
        }
    }
}

/// `Ok(true)`: we hold it now; `Ok(false)`: held elsewhere.
async fn try_lock(connection: &Connection, queue: &str) -> Result<bool, lapin::Error> {
    let channel = match connection.create_channel().await {
        Ok(channel) => channel,
        // lapin reuses channel ids: the RESOURCE_LOCKED of an earlier attempt
        // may surface here. Not a connection failure (the loop checks the
        // connection's status): try again on the next pass.
        Err(e) if e.is_amqp_soft_error() => {
            debug!(queue, error = %e, "lock channel not opened, retrying later");
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    let result = channel
        .queue_declare(
            ShortString::from(queue),
            QueueDeclareOptions {
                exclusive: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await;
    match result {
        Ok(_) => {
            let _ = channel.close(200, ShortString::from("locked")).await;
            Ok(true)
        }
        // 405 RESOURCE_LOCKED (the channel is closed by the broker)
        Err(e) if e.is_amqp_soft_error() => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probing_once_after_a_fruitless_sweep() {
        assert_eq!(probe_budget(256, false), 256);
        assert_eq!(probe_budget(256, true), 1);
    }

    #[test]
    fn shares_and_names() {
        assert_eq!(fair_share(256, 3), 86);
        assert_eq!(fair_share(256, 0), 256);
        let spec = ShardSpec {
            lock_prefix: "scheduler.lock".into(),
            item: "shard".into(),
            members_queue: "scheduler.members".into(),
            count: 256,
            retry: Duration::from_secs(5),
            watchdog_timeout: Duration::from_secs(30),
        };
        assert_eq!(spec.lock_queue(7), "scheduler.lock.256.shard-7");
    }

    #[test]
    fn watchdog_detects_stall() {
        let watchdog = Watchdog::new();
        assert!(!watchdog.stale(Duration::from_secs(5)));
        watchdog.0.store(now_ms() - 10_000, Ordering::Relaxed);
        assert!(watchdog.stale(Duration::from_secs(5)));
    }
}
