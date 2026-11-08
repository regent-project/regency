//! regency dbwriter: the only CouchDB writer (D8). Consumes
//! - `regency.outcomes.dbwriter` → `outcome:`, `job:`, `host:` docs;
//! - `regency.lifecycle.dbwriter` → `job:` docs (D7);
//! - `regency.commands.dbwriter` → orders, settings, hosts, catalog, users,
//!   policies (D10);
//! - `regency.dead` → `EXPIRED` / `FAILED` jobs (D7);
//!
//! and runs the retention cleanup (D8). Replicas compete on every queue;
//! every write is an idempotent create or a commutative merge, so no
//! coordination is needed.

mod commands;
mod config;
mod retention;
mod writer;

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use lapin::message::Delivery;
use regencylib::amqp::{Handled, consume, first_death, retry_transient};
use regencylib::couch::Couch;
use regencylib::messages::{Command, LifecycleEvent, TaskHeader, decode_outcome};
use regencylib::telemetry::{Health, counter, shutdown_signal};
use regencylib::topology::{
    self, Q_COMMANDS_DBWRITER, Q_DEAD, Q_LIFECYCLE_DBWRITER, Q_OUTCOMES_DBWRITER, Q_STATUS_DBWRITER,
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span, warn};

use crate::commands::CommandError;
use crate::config::Config;
use crate::writer::{Writer, is_transient};

const SERVICE: &str = "dbwriter";

#[derive(Parser)]
#[command(name = "regency-dbwriter", version, about = "regency's CouchDB writer")]
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
        "regency-dbwriter",
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
        regencylib::services::dbwriter::external_checks,
    )
    .await;
    let (config, instance) = (central.config.clone(), central.instance.clone());
    if let Err(e) = run(config, instance, central).await {
        error!(error = %format!("{e:#}"), "dbwriter stopped");
        return 1;
    }
    regencylib::central::exit_code()
}

struct Context {
    writer: Arc<Writer>,
    cancel: CancellationToken,
    failures: prometheus::IntCounterVec,
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
    health.register("couchdb");
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
    let ready = retry_transient(
        "couchdb startup",
        &cancel,
        |_| true,
        || async {
            couch.ping().await?;
            retention::ensure_settings(&couch).await
        },
    )
    .await;
    match ready {
        Some(Ok(())) => health.ready("couchdb"),
        Some(Err(e)) => return Err(e.into()),
        None => return Ok(()),
    }
    let writer = Arc::new(Writer::new(
        couch,
        counter(
            &health,
            "regency_dbwriter_writes_total",
            "Documents written, by kind",
            &["kind"],
        ),
    ));
    tokio::spawn(retention::run(
        writer.clone(),
        Duration::from_secs(config.dbwriter.first_cleanup_delay_sec),
        Duration::from_secs(config.dbwriter.cleanup_interval_hours * 3600),
        cancel.clone(),
    ));
    let context = Arc::new(Context {
        writer,
        cancel: cancel.clone(),
        failures: counter(
            &health,
            "regency_dbwriter_rejected_total",
            "Messages rejected, by queue and reason",
            &["queue", "reason"],
        ),
    });

    let name = format!("{SERVICE}/{instance}");
    while let Some(connection) =
        regencylib::amqp::connect_retrying(&config.common.rabbitmq, &secrets, &name, &cancel).await
    {
        let setup = async {
            let channel = connection.create_channel().await?;
            topology::declare_outcomes_dbwriter(&channel, &config.topology).await?;
            topology::declare_lifecycle_dbwriter(&channel, &config.topology).await?;
            topology::declare_commands_dbwriter(&channel, &config.topology).await?;
            topology::declare_status_dbwriter(&channel, &config.topology).await?;
            topology::declare_dead(&channel).await?;
            let _ = channel.close(200, "declared".into()).await;
            Ok::<_, lapin::Error>(())
        };
        if let Err(e) = setup.await {
            warn!(error = %e, "topology declaration failed");
            regencylib::amqp::close(&connection, 200, "retry").await;
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        health.ready("rabbitmq");
        let prefetch = config.dbwriter.prefetch;
        let tags =
            ["outcomes", "lifecycle", "commands", "dead", "status"].map(|q| format!("{name}/{q}"));
        let c = &context;
        let results = tokio::join!(
            consume(
                &connection,
                Q_OUTCOMES_DBWRITER,
                &tags[0],
                prefetch,
                &cancel,
                |d| on_outcome(c, d)
            ),
            consume(
                &connection,
                Q_LIFECYCLE_DBWRITER,
                &tags[1],
                prefetch,
                &cancel,
                |d| on_lifecycle(c, d)
            ),
            consume(
                &connection,
                Q_COMMANDS_DBWRITER,
                &tags[2],
                prefetch,
                &cancel,
                |d| on_command(c, d)
            ),
            consume(&connection, Q_DEAD, &tags[3], prefetch, &cancel, |d| {
                on_dead(c, d)
            }),
            consume(
                &connection,
                Q_STATUS_DBWRITER,
                &tags[4],
                prefetch,
                &cancel,
                |d| on_status(c, d)
            ),
        );
        for result in [results.0, results.1, results.2, results.3, results.4] {
            if let Err(e) = result {
                warn!(error = %e, "consumer stopped");
            }
        }
        health.not_ready("rabbitmq", "disconnected");
        regencylib::amqp::close(&connection, 200, "bye").await;
        if cancel.is_cancelled() {
            break;
        }
        warn!("RabbitMQ connection lost, reconnecting");
    }
    info!("dbwriter stopped");
    Ok(())
}

/// Write with retries on transient CouchDB errors; Requeue if shutting down.
async fn write<F, Fut>(context: &Context, what: &str, operation: F) -> Handled
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), regencylib::couch::CouchError>>,
{
    match retry_transient(what, &context.cancel, is_transient, operation).await {
        None => Handled::Requeue,
        Some(Ok(())) => Handled::Ack,
        Some(Err(e)) => {
            error!(error = %e, "permanent CouchDB error, message parked");
            context.failures.with_label_values(&[what, "couchdb"]).inc();
            Handled::Reject
        }
    }
}

fn poison(context: &Context, queue: &str, error: impl std::fmt::Display) -> Handled {
    error!(queue, error = %error, "undecodable message, parked");
    context
        .failures
        .with_label_values(&[queue, "undecodable"])
        .inc();
    Handled::Reject
}

async fn on_outcome(context: &Context, delivery: Delivery) -> Handled {
    let decoded = match decode_outcome(&delivery.data) {
        Ok(decoded) => decoded,
        Err(e) => return poison(context, Q_OUTCOMES_DBWRITER, e),
    };
    let span = info_span!(
        "outcome",
        job_id = decoded.result.idempotency_key(),
        host_id = decoded.result.host_id(),
        origin = %decoded.origin,
    );
    write(context, "outcome", || context.writer.outcome(&decoded))
        .instrument(span)
        .await
}

async fn on_lifecycle(context: &Context, delivery: Delivery) -> Handled {
    let event = match LifecycleEvent::from_bytes(&delivery.data) {
        Ok(event) => event,
        Err(e) => return poison(context, Q_LIFECYCLE_DBWRITER, e),
    };
    let span = info_span!("lifecycle", job_id = %event.job_id, host_id = %event.host_id, state = event.state.as_str());
    write(context, "lifecycle", || context.writer.lifecycle(&event))
        .instrument(span)
        .await
}

async fn on_dead(context: &Context, delivery: Delivery) -> Handled {
    // header only: a dead task may come from another regent-sdk (D20)
    let envelope = match TaskHeader::from_bytes(&delivery.data) {
        Ok(envelope) => envelope,
        Err(e) => return poison(context, Q_DEAD, e),
    };
    let (queue, reason) =
        first_death(&delivery.properties).unwrap_or_else(|| ("?".into(), "unknown".into()));
    let span = info_span!(
        "dead_task",
        job_id = %envelope.job_id,
        host_id = %envelope.host_id,
        queue = %queue,
        reason = %reason,
    );
    let at = chrono::Utc::now();
    async {
        warn!("task dead-lettered");
        write(context, "dead", || {
            context.writer.dead_task(&envelope, &reason, at)
        })
        .await
    }
    .instrument(span)
    .await
}

async fn on_status(context: &Context, delivery: Delivery) -> Handled {
    let status = match regencylib::central::InstanceStatus::from_bytes(&delivery.data) {
        Ok(status) => status,
        Err(e) => return poison(context, Q_STATUS_DBWRITER, e),
    };
    write(context, "instance", || {
        context.writer.instance_status(&status)
    })
    .await
}

async fn on_command(context: &Context, delivery: Delivery) -> Handled {
    let command = match Command::from_bytes(&delivery.data) {
        Ok(command) => command,
        Err(e) => return poison(context, Q_COMMANDS_DBWRITER, e),
    };
    let span = info_span!(
        "command",
        command_id = %command.command_id,
        kind = command.routing_key(),
        principal = %command.issued_by,
    );
    async {
        let outcome = retry_transient(
            "command",
            &context.cancel,
            |e: &CommandError| matches!(e, CommandError::Couch(c) if is_transient(c)),
            || commands::apply(&context.writer.couch, &command),
        )
        .await;
        match outcome {
            None => Handled::Requeue,
            Some(Ok(())) => Handled::Ack,
            Some(Err(CommandError::Refused(reason))) => {
                warn!(reason, "command refused");
                context
                    .failures
                    .with_label_values(&[command.routing_key(), "refused"])
                    .inc();
                Handled::Ack
            }
            Some(Err(e)) => {
                error!(error = %e, "command failed, parked");
                context
                    .failures
                    .with_label_values(&[command.routing_key(), "couchdb"])
                    .inc();
                Handled::Reject
            }
        }
    }
    .instrument(span)
    .await
}
