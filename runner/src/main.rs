//! regency runner (D4, D7): consumes `RegentTask`s of its pool, runs them
//! against passive hosts (SSH/SSHOQ, through regent-sdk), publishes outcomes.
//!
//! - Assess: shared queue `regency.tasks.<pool>.assess`, competing runners.
//! - Reach: partitions `regency.tasks.<pool>.<P>.<N>` owned through exclusive
//!   lock queues (`regencylib::shard`), at most `ceil(P / live)` each.
//! - Aborts: follows the stream `regency.control`.
//! - Secrets: its own `SecretProvidersPool` (D9).

mod config;
mod executor;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use lapin::{Channel, Connection};
use regencylib::amqp::{Handled, Publisher, closed, connect_retrying, consume, consume_on};
use regencylib::control::{self, Aborts};
use regencylib::shard::{self, ShardSpec, Watchdog};
use regencylib::telemetry::{Health, counter, gauge, shutdown_signal};
use regencylib::topology::{self, assess_queue, reach_queue};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::executor::{Executor, Metrics};

const SERVICE: &str = "runner";

/// Reach partitions consumed on one AMQP channel (see `reach_partitions`).
const PARTITIONS_PER_CHANNEL: u32 = 32;

#[derive(Parser)]
#[command(
    name = "regency-runner",
    version,
    about = "regency runner for passive hosts"
)]
struct Args {
    #[arg(short, long, default_value_t = regencylib::config::default_path(SERVICE))]
    config: String,
    /// Run under the Windows Service Control Manager (D22; set by the MSI's
    /// service registration).
    #[arg(long, hide = true)]
    windows_service: bool,
}

fn main() {
    let args = Args::parse();
    let windows_service = args.windows_service;
    regencylib::process::launch(
        "regency-runner",
        windows_service,
        Box::new(move || {
            tokio::runtime::Runtime::new()
                .expect("tokio runtime")
                .block_on(async_main(args))
        }),
    );
}

async fn async_main(args: Args) -> i32 {
    let central = regencylib::central::startup::<Config>(
        SERVICE,
        env!("CARGO_PKG_VERSION"),
        &args.config,
        true,
        false,
        |config| &config.common,
        regencylib::services::runner::external_checks,
    )
    .await;
    let (config, instance) = (central.config.clone(), central.instance.clone());
    if let Err(e) = run(config, instance, central).await {
        error!(error = %format!("{e:#}"), "runner stopped");
        return 1;
    }
    regencylib::central::exit_code()
}

async fn run(
    config: Config,
    instance: String,
    central: regencylib::central::Resolved<Config>,
) -> anyhow::Result<()> {
    let secrets = config.common.secrets.pool()?;
    let health = Health::new();
    health.spawn(config.common.health.clone());
    let mut central = central;
    if let Some(path) = &config.runner.ssh_key {
        advertise_ssh_key(&mut central, path, &config.runner.pool);
    }
    central.spawn(&health);
    health.register("rabbitmq");
    health.register("control");
    let pool = config.runner.pool.clone();
    info!(pool = %pool, reach_partitions = config.topology.reach_partitions, "runner starting");

    // shutdown: stop consuming, then stop the tasks left after the grace
    let consuming = CancellationToken::new();
    let drain = CancellationToken::new();
    {
        let (consuming, drain) = (consuming.clone(), drain.clone());
        let grace = Duration::from_secs(config.runner.shutdown_grace_sec);
        tokio::spawn(async move {
            shutdown_signal().await;
            consuming.cancel();
            tokio::time::sleep(grace).await;
            warn!("shutdown grace over, stopping running tasks");
            drain.cancel();
        });
    }

    let lookback = Duration::from_secs(config.runner.control_lookback_sec);
    let aborts = Aborts::new(lookback + Duration::from_secs(3600));
    {
        let health = health.clone();
        tokio::spawn(control::follow(
            config.common.rabbitmq.clone(),
            secrets.clone(),
            aborts.clone(),
            lookback,
            drain.clone(),
            move |ok| {
                if ok {
                    health.ready("control")
                } else {
                    health.not_ready("control", "not subscribed")
                }
            },
        ));
    }
    // D24: live views (stream) and the run log limit (settings)
    let live = config.runner.live.then(|| {
        regencylib::live::LiveLink::spawn(
            config.common.rabbitmq.clone(),
            secrets.clone(),
            drain.clone(),
        )
    });
    let run_log_limit = regencylib::live::RunLogLimit::default();
    run_log_limit.spawn_follower(
        regencylib::couch::Couch::from_config(&config.common.couchdb, &secrets).await?,
        drain.clone(),
    );
    let metrics = Arc::new(Metrics {
        tasks: counter(
            &health,
            "regency_runner_tasks_total",
            "Tasks handled, by job and result",
            &["job", "result"],
        ),
        running: gauge(&health, "regency_runner_running_tasks", "Tasks running now"),
    });
    let owned_gauge = gauge(
        &health,
        "regency_runner_reach_partitions",
        "Reach partitions owned",
    );

    let name = format!("{SERVICE}/{pool}/{instance}");
    while let Some(connection) =
        connect_retrying(&config.common.rabbitmq, &secrets, &name, &consuming).await
    {
        let connection = Arc::new(connection);
        let setup = async {
            let channel = connection.create_channel().await?;
            topology::declare_exchanges(&channel).await?;
            topology::declare_pool(&channel, &config.topology, &pool).await?;
            let _ = channel.close(200, "declared".into()).await;
            Publisher::new(&connection).await
        };
        let publisher = match setup.await {
            Ok(publisher) => publisher,
            Err(e) => {
                warn!(error = %e, "RabbitMQ setup failed");
                regencylib::amqp::close(&connection, 200, "retry").await;
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        health.ready("rabbitmq");
        // D4: abort in-flight tasks on any connection error
        let stop_tasks = drain.child_token();
        {
            let (connection, stop_tasks) = (connection.clone(), stop_tasks.clone());
            let publisher = publisher.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = closed(&connection) => {}
                    _ = publisher.closed() => {}
                }
                stop_tasks.cancel();
            });
        }
        let executor = Arc::new(Executor::new(
            publisher,
            secrets.clone(),
            aborts.clone(),
            stop_tasks.clone(),
            instance.clone(),
            pool.clone(),
            metrics.clone(),
            config.common.security.external,
            live.clone(),
            run_log_limit.clone(),
        ));
        let session = consuming.child_token();
        let (assess_name, assess_tag) = (assess_queue(&pool), format!("{name}/assess"));
        let assess = {
            let executor = executor.clone();
            consume(
                &connection,
                &assess_name,
                &assess_tag,
                config.runner.assess_concurrency,
                &session,
                move |delivery| {
                    let executor = executor.clone();
                    async move { executor.handle(delivery).await }
                },
            )
        };
        let reach = reach_partitions(
            &config,
            &pool,
            connection.clone(),
            executor.clone(),
            &session,
            &name,
            &owned_gauge,
        );
        tokio::select! {
            result = assess => if let Err(e) = result { warn!(error = %e, "assess consumer stopped") },
            _ = reach => {}
            _ = stop_tasks.cancelled() => {}
        }
        // stop intake everywhere; in-flight tasks finish (or are stopped)
        session.cancel();
        while metrics.running.get() > 0 && !stop_tasks.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        health.not_ready("rabbitmq", "disconnected");
        regencylib::amqp::close(&connection, 200, "bye").await;
        if consuming.is_cancelled() {
            break;
        }
        warn!("RabbitMQ connection lost, reconnecting");
    }
    drain.cancel();
    info!("runner stopped");
    Ok(())
}

/// Own Reach partitions (D3 machinery) and consume the owned ones.
async fn reach_partitions(
    config: &Config,
    pool: &str,
    connection: Arc<Connection>,
    executor: Arc<Executor>,
    session: &CancellationToken,
    name: &str,
    owned_gauge: &prometheus::IntGauge,
) {
    let partitions = config.topology.reach_partitions;
    let spec = ShardSpec {
        lock_prefix: format!("runner.lock.{pool}"),
        item: "part".to_string(),
        members_queue: format!("runner.members.{pool}"),
        count: partitions,
        retry: Duration::from_secs(config.runner.lock_retry_sec),
        watchdog_timeout: Duration::from_secs(60),
    };
    let watchdog = Watchdog::new();
    // shared by every owned partition: Reaches running at once
    let slots = Arc::new(tokio::sync::Semaphore::new(usize::from(
        config.runner.reach_concurrency.max(1),
    )));
    let (owned_tx, mut owned_rx) = watch::channel(BTreeSet::new());
    let ownership = shard::run(&spec, &connection, &watchdog, &owned_tx, session);
    let manage = async {
        let mut consumers: HashMap<u32, (CancellationToken, JoinHandle<()>)> = HashMap::new();
        // partitions share channels, PARTITIONS_PER_CHANNEL each: one channel
        // per partition would exceed the broker's channel limit per
        // connection (RabbitMQ 4.4: 256 by default, = the default P)
        let mut channels: HashMap<u32, Channel> = HashMap::new();
        loop {
            watchdog.tick();
            let owned = owned_rx.borrow_and_update().clone();
            owned_gauge.set(owned.len() as i64);
            // released partitions: stop intake, in-flight tasks finish;
            // stopped consumers of owned partitions: started again below
            consumers.retain(|partition, (token, task)| {
                if !owned.contains(partition) {
                    token.cancel();
                    info!(partition, "reach partition released");
                    return false;
                }
                !task.is_finished()
            });
            channels.retain(|_, channel| channel.status().connected());
            for partition in owned {
                if consumers.contains_key(&partition) {
                    continue;
                }
                let group = partition / PARTITIONS_PER_CHANNEL;
                let channel = match channels.get(&group) {
                    Some(channel) => channel.clone(),
                    None => match connection.create_channel().await {
                        Ok(channel) => {
                            channels.insert(group, channel.clone());
                            channel
                        }
                        Err(e) => {
                            warn!(partition, error = %e, "no channel for a reach partition, retrying");
                            continue;
                        }
                    },
                };
                let token = session.child_token();
                let executor = executor.clone();
                let queue = reach_queue(pool, partitions, partition);
                let tag = format!("{name}/part-{partition}");
                let prefetch = config.runner.reach_prefetch;
                let cancel = token.clone();
                let slots = slots.clone();
                let stop = token.clone();
                let task = tokio::spawn(async move {
                    let handler = move |delivery| {
                        let executor = executor.clone();
                        let slots = slots.clone();
                        let stop = stop.clone();
                        async move {
                            // at most reach_concurrency Reaches at once (FIFO);
                            // given back if the partition stops meanwhile
                            let _slot = tokio::select! {
                                slot = slots.acquire_owned() => match slot {
                                    Ok(slot) => slot,
                                    Err(_) => return Handled::Requeue,
                                },
                                _ = stop.cancelled() => return Handled::Requeue,
                            };
                            executor.handle(delivery).await
                        }
                    };
                    if let Err(e) =
                        consume_on(&channel, &queue, &tag, prefetch, &cancel, handler).await
                    {
                        warn!(partition, error = %e, "reach consumer stopped, restarting it");
                    }
                });
                consumers.insert(partition, (token, task));
            }
            tokio::select! {
                _ = session.cancelled() => break,
                changed = owned_rx.changed() => if changed.is_err() { break },
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
        for (token, _) in consumers.values() {
            token.cancel();
        }
    };
    tokio::select! {
        result = ownership => if let Err(e) = result { warn!(error = %e, "partition ownership lost") },
        _ = manage => {}
    }
}

/// D25: generate the runner's SSH key when missing and advertise its public
/// key. A key that cannot be written is logged: the runner still serves the
/// hosts whose definitions bring their own credentials.
fn advertise_ssh_key(central: &mut regencylib::central::Resolved<Config>, path: &str, pool: &str) {
    use regencylib::sshkey::{Ensured, ensure};
    match ensure(
        std::path::Path::new(path),
        &format!("regency-runner-{pool}"),
    ) {
        Ok(Ensured::Generated(line)) => {
            info!(path = %path, "SSH key generated for the runner");
            central.advertised.insert("ssh_public_key".into(), line);
        }
        Ok(Ensured::Existing(line)) => {
            central.advertised.insert("ssh_public_key".into(), line);
        }
        Ok(Ensured::NoPublicKey) => {
            warn!(path = %path, "SSH key without its .pub file: its public key is not advertised");
        }
        Err(e) => error!(path = %path, error = %e, "cannot create the runner's SSH key"),
    }
}
