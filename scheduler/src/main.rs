//! regency scheduler (D3, D10, D16): fires recurring orders of the shards it
//! leads, and expands `run` commands, into `RegentTask`s on `regency.tasks`.

mod commands;
mod config;
mod dispatch;
mod orders;
mod state;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use regencylib::amqp::{Publisher, connect_retrying, consume};
use regencylib::couch::Couch;
use regencylib::shard::{self, ShardSpec, Watchdog};
use regencylib::telemetry::{Health, counter, gauge, shutdown_signal};
use regencylib::topology::{self, Q_COMMANDS_SCHEDULER};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::dispatch::Dispatcher;
use crate::orders::Skips;
use crate::state::State;

const SERVICE: &str = "scheduler";

#[derive(Parser)]
#[command(name = "regency-scheduler", version, about = "regency scheduler")]
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
        "regency-scheduler",
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
        true,
        |config| &config.common,
        regencylib::services::scheduler::external_checks,
    )
    .await;
    let (config, instance) = (central.config.clone(), central.instance.clone());
    if let Err(e) = run(config, instance, central).await {
        error!(error = %format!("{e:#}"), "scheduler stopped");
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
    central.spawn(&health);
    health.register("rabbitmq");
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            cancel.cancel();
        });
    }
    let couch = Couch::from_config(&config.common.couchdb, &secrets).await?;
    let state = State::new(config.scheduler.bootstrap_admin.clone());
    state.spawn_followers(&couch, &health, &cancel);
    let published = counter(
        &health,
        "regency_scheduler_tasks_total",
        "Tasks built, by result",
        &["result"],
    );
    let skips = Arc::new(Skips(counter(
        &health,
        "regency_scheduler_skips_total",
        "Hosts or slots skipped, by reason",
        &["reason"],
    )));
    let owned_gauge = gauge(
        &health,
        "regency_scheduler_shards",
        "Order shards led by this scheduler",
    );
    let spec = ShardSpec {
        lock_prefix: "scheduler.lock".to_string(),
        item: "shard".to_string(),
        members_queue: "scheduler.members".to_string(),
        count: config.scheduler.shards,
        retry: Duration::from_secs(config.scheduler.lock_retry_sec),
        watchdog_timeout: Duration::from_secs(config.scheduler.watchdog_timeout_sec),
    };

    let name = format!("{SERVICE}/{instance}");
    while let Some(connection) =
        connect_retrying(&config.common.rabbitmq, &secrets, &name, &cancel).await
    {
        let setup = async {
            let channel = connection.create_channel().await?;
            topology::declare_exchanges(&channel).await?;
            topology::declare_commands_scheduler(&channel, &config.topology).await?;
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
        let connection = Arc::new(connection);
        let dispatcher = Arc::new(Dispatcher::new(
            publisher,
            connection.clone(),
            config.topology.clone(),
            published.clone(),
        ));
        let session = cancel.child_token();
        let watchdog = Watchdog::new();
        let (owned_tx, owned_rx) = watch::channel(BTreeSet::new());

        let ownership = shard::run(&spec, &connection, &watchdog, &owned_tx, &session);
        let firing = orders::fire_loop(
            state.clone(),
            dispatcher.clone(),
            owned_rx.clone(),
            config.scheduler.shards,
            watchdog.clone(),
            session.clone(),
            skips.clone(),
        );
        let report = async {
            let mut rx = owned_rx.clone();
            loop {
                let count = rx.borrow_and_update().len();
                owned_gauge.set(count as i64);
                info!(shards = count, "led shards changed");
                if rx.changed().await.is_err() {
                    return;
                }
            }
        };
        let tag = format!("{name}/commands");
        let commands = consume(
            &connection,
            Q_COMMANDS_SCHEDULER,
            &tag,
            config.scheduler.command_prefetch,
            &session,
            |delivery| {
                let (state, dispatcher) = (state.clone(), dispatcher.clone());
                async move { commands::handle(&state, &dispatcher, delivery).await }
            },
        );
        // D3: stop firing at once on any connection error (ownership ends
        // first, emptying the owned set)
        tokio::select! {
            result = ownership => if let Err(e) = result { warn!(error = %e, "shard ownership ended") },
            _ = firing => {}
            result = commands => if let Err(e) = result { warn!(error = %e, "command consumer ended") },
            _ = report => {}
            _ = dispatcher.publisher.closed() => {}
        }
        session.cancel();
        owned_tx.send_replace(BTreeSet::new());
        owned_gauge.set(0);
        health.not_ready("rabbitmq", "disconnected");
        regencylib::amqp::close(&connection, 200, "bye").await;
        if cancel.is_cancelled() {
            break;
        }
        warn!("RabbitMQ connection lost, reconnecting");
    }
    info!("scheduler stopped");
    Ok(())
}
