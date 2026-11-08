//! regency sourcesync (D11): stateless replicas sharing Git sources through
//! shard ownership (`sourcesync.lock.<count>.shard-N`). A shard leader polls
//! each of its Git sources at its interval (or on `source.refresh`), diffs the
//! repository against the catalog and publishes `catalog.upsert` /
//! `catalog.delete` commands (dbwriter writes them).

mod git;
mod parse;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use clap::Parser;
use regencylib::amqp::{Handled, Publisher, connect_retrying, consume};
use regencylib::config::resolve_secret;
use regencylib::couch::{Couch, Since, follow};
use regencylib::hashing::bucket;
use regencylib::messages::{Command, CommandBody};
use regencylib::model::{CatalogBody, CatalogItem, DB_CATALOG, SourceDef, SourceKind};
use regencylib::services::sourcesync::Config;
use regencylib::shard::{self, ShardSpec, Watchdog};
use regencylib::telemetry::{Health, counter, shutdown_signal};
use regencylib::topology::{self, EX_COMMANDS};
use regent_sdk::secrets::SecretProvidersPool;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span, warn};

const SERVICE: &str = "sourcesync";
/// Issuer of sourcesync's commands (dbwriter's D11 ownership rule).
const ISSUER: &str = "service:sourcesync";

#[derive(Parser)]
#[command(
    name = "regency-sourcesync",
    version,
    about = "regency Git source synchronization"
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
        "regency-sourcesync",
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
        regencylib::services::sourcesync::external_checks,
    )
    .await;
    let (config, instance) = (central.config.clone(), central.instance.clone());
    if let Err(e) = run(config, instance, central).await {
        error!(error = %format!("{e:#}"), "sourcesync stopped");
        return 1;
    }
    regencylib::central::exit_code()
}

/// Catalog copy (`regency_catalog` `_changes`).
#[derive(Default)]
struct Catalog(RwLock<HashMap<String, CatalogItem>>);

impl Catalog {
    fn git_sources(&self) -> Vec<SourceDef> {
        self.0
            .read()
            .expect("catalog")
            .values()
            .filter_map(|i| match &i.body {
                CatalogBody::Source(s) if s.kind == SourceKind::Git => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    fn items_of(&self, source_id: &str) -> HashMap<String, CatalogItem> {
        self.0
            .read()
            .expect("catalog")
            .iter()
            .filter(|(_, i)| {
                i.source_id() == source_id && !matches!(i.body, CatalogBody::Source(_))
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

struct Syncer {
    catalog: Arc<Catalog>,
    /// D19 external level: minimum security of Git sources.
    git_level: regencylib::security::Level,
    secrets: SecretProvidersPool,
    workdir: PathBuf,
    default_branch: String,
    syncs: prometheus::IntCounterVec,
}

impl Syncer {
    async fn sync(&self, publisher: &Publisher, source: &SourceDef) -> Result<(), String> {
        let repo = source.repo.clone().ok_or("git source without repo")?;
        regencylib::security::check_git_url(self.git_level, &repo)?;
        let branch = source
            .branch
            .clone()
            .unwrap_or_else(|| self.default_branch.clone());
        let path = source.path.clone().unwrap_or_default();
        let token = match &source.credentials {
            Some(reference) => Some(
                resolve_secret(&self.secrets, reference)
                    .await
                    .map_err(|e| e.to_string())?,
            ),
            None => None,
        };
        let dir = self.workdir.join(&source.source_id);
        let started = Instant::now();
        let (sha, files) =
            tokio::task::spawn_blocking(move || git::fetch(&repo, &branch, &path, &dir, token))
                .await
                .map_err(|e| e.to_string())??;
        let (items, errors) = parse::parse(&source.source_id, &sha, &files);
        for problem in &errors {
            warn!(sha = %sha, problem, "file skipped");
        }
        let existing = self.catalog.items_of(&source.source_id);
        let mut upserts = 0;
        let mut wanted = HashSet::new();
        for item in items {
            let doc_id = item.doc_id();
            wanted.insert(doc_id.clone());
            let changed = existing.get(&doc_id).is_none_or(|old| {
                serde_json::to_value(&old.body).ok() != serde_json::to_value(&item.body).ok()
            });
            if !changed {
                continue;
            }
            let command = Command::new(
                format!("sync:{}:{sha}:{doc_id}", source.source_id),
                ISSUER,
                CommandBody::CatalogUpsert(item),
            );
            publisher
                .publish_json(
                    EX_COMMANDS,
                    command.routing_key(),
                    &command.to_bytes().map_err(|e| e.to_string())?,
                )
                .await
                .map_err(|e| e.to_string())?;
            upserts += 1;
        }
        // a broken file never deletes what it used to define
        let mut deletes = 0;
        if errors.is_empty() {
            for doc_id in existing.keys().filter(|id| !wanted.contains(*id)) {
                let command = Command::new(
                    format!("sync:{}:{sha}:{doc_id}:delete", source.source_id),
                    ISSUER,
                    CommandBody::CatalogDelete {
                        doc_id: doc_id.clone(),
                        source_id: source.source_id.clone(),
                    },
                );
                publisher
                    .publish_json(
                        EX_COMMANDS,
                        command.routing_key(),
                        &command.to_bytes().map_err(|e| e.to_string())?,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                deletes += 1;
            }
        }
        self.syncs
            .with_label_values(&[if errors.is_empty() { "ok" } else { "partial" }])
            .inc();
        info!(
            sha = %sha,
            files = files.len(),
            upserts,
            deletes,
            file_errors = errors.len(),
            duration_ms = started.elapsed().as_millis() as u64,
            "source synchronized"
        );
        Ok(())
    }
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
    health.register("catalog");
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            cancel.cancel();
        });
    }
    let couch = Couch::from_config(&config.common.couchdb, &secrets).await?;
    let catalog = Arc::new(Catalog::default());
    {
        let (catalog, health, cancel) = (catalog.clone(), health.clone(), cancel.clone());
        tokio::spawn(async move {
            follow(
                &couch,
                DB_CATALOG,
                Since::Start,
                &cancel,
                |change| {
                    let mut items = catalog.0.write().expect("catalog");
                    items.remove(&change.id);
                    if !change.deleted
                        && !change.id.starts_with("_design/")
                        && let Some(item) = change
                            .doc
                            .and_then(|d| serde_json::from_value::<CatalogItem>(d).ok())
                    {
                        items.insert(change.id, item);
                    }
                },
                || health.ready("catalog"),
            )
            .await;
        });
    }
    let syncer = Arc::new(Syncer {
        catalog: catalog.clone(),
        git_level: config.common.security.external,
        secrets: secrets.clone(),
        workdir: PathBuf::from(&config.sourcesync.workdir),
        default_branch: config.sourcesync.default_branch.clone(),
        syncs: counter(
            &health,
            "regency_sourcesync_syncs_total",
            "Source synchronizations, by result",
            &["result"],
        ),
    });
    let refresh: Arc<Mutex<HashSet<String>>> = Arc::default();
    let spec = ShardSpec {
        lock_prefix: "sourcesync.lock".to_string(),
        item: "shard".to_string(),
        members_queue: "sourcesync.members".to_string(),
        count: config.sourcesync.shards,
        retry: Duration::from_secs(config.sourcesync.lock_retry_sec),
        watchdog_timeout: Duration::from_secs(600),
    };

    let name = format!("{SERVICE}/{instance}");
    while let Some(connection) =
        connect_retrying(&config.common.rabbitmq, &secrets, &name, &cancel).await
    {
        let setup = async {
            let channel = connection.create_channel().await?;
            let queue = topology::declare_refresh_queue(&channel).await?;
            Ok::<_, regencylib::amqp::AmqpError>((Publisher::new(&connection).await?, queue))
        };
        let (publisher, refresh_queue) = match setup.await {
            Ok(ok) => ok,
            Err(e) => {
                warn!(error = %e, "RabbitMQ setup failed");
                regencylib::amqp::close(&connection, 200, "retry").await;
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        health.ready("rabbitmq");
        let session = cancel.child_token();
        let watchdog = Watchdog::new();
        let (owned_tx, owned_rx) = watch::channel(BTreeSet::new());
        let ownership = shard::run(&spec, &connection, &watchdog, &owned_tx, &session);
        let refreshes = {
            let refresh = refresh.clone();
            let tag = format!("{name}/refresh");
            let (connection, refresh_queue, session) = (&connection, &refresh_queue, &session);
            async move {
                consume(connection, refresh_queue, &tag, 16, session, |delivery| {
                    let refresh = refresh.clone();
                    async move {
                        if let Ok(Command {
                            body: CommandBody::SourceRefresh { source_id },
                            ..
                        }) = Command::from_bytes(&delivery.data)
                        {
                            info!(source_id = %source_id, "refresh requested");
                            refresh.lock().expect("refresh").insert(source_id);
                        }
                        Handled::Ack
                    }
                })
                .await
            }
        };
        let polling = async {
            let mut next: HashMap<String, Instant> = HashMap::new();
            loop {
                watchdog.tick();
                let owned = owned_rx.borrow().clone();
                for source in catalog.git_sources() {
                    if !owned.contains(&bucket(&source.source_id, spec.count)) {
                        next.remove(&source.source_id);
                        continue;
                    }
                    let forced = refresh.lock().expect("refresh").remove(&source.source_id);
                    if !forced
                        && next
                            .get(&source.source_id)
                            .is_some_and(|at| Instant::now() < *at)
                    {
                        continue;
                    }
                    let span = info_span!("sync", source_id = %source.source_id, repo = source.repo.as_deref().unwrap_or(""));
                    if let Err(e) = syncer.sync(&publisher, &source).instrument(span).await {
                        warn!(source_id = %source.source_id, error = %e, "synchronization failed");
                        syncer.syncs.with_label_values(&["failed"]).inc();
                    }
                    watchdog.tick();
                    next.insert(
                        source.source_id.clone(),
                        Instant::now() + Duration::from_secs(source.poll_interval_sec.max(10)),
                    );
                }
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
        };
        tokio::select! {
            result = ownership => if let Err(e) = result { warn!(error = %e, "shard ownership ended") },
            result = refreshes => if let Err(e) = result { warn!(error = %e, "refresh consumer ended") },
            _ = polling => {}
            _ = publisher.closed() => {}
        }
        session.cancel();
        owned_tx.send_replace(BTreeSet::new());
        health.not_ready("rabbitmq", "disconnected");
        regencylib::amqp::close(&connection, 200, "bye").await;
        if cancel.is_cancelled() {
            break;
        }
        warn!("RabbitMQ connection lost, reconnecting");
    }
    info!("sourcesync stopped");
    Ok(())
}
