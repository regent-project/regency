//! Central, editable service configuration and instance status (D21).
//!
//! Layers, lowest first: defaults < central document (`regency_config`) <
//! local file < `REGENCY_*` env. Bootstrap keys ([`BOOTSTRAP_KEYS`]) are local
//! only. A change of the effective configuration restarts the service
//! (checked first; exit status [`RESTART_EXIT_CODE`]). Every instance
//! publishes its status on `regency.status`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::amqp::PublisherLink;
use crate::config::{CommonConfig, ConfigError};
use crate::couch::{Couch, CouchError, Since, follow};
use crate::messages::SCHEMA_VERSION;
use crate::model::{DB_CONFIG, Stored};
use crate::telemetry::Health;
use crate::topology::EX_STATUS;

/// Services that read a central configuration.
pub const SERVICES: [&str; 6] = [
    "scheduler",
    "runner",
    "dbwriter",
    "backend",
    "endpoint",
    "sourcesync",
];

/// Exit status asking the deployment to restart the service with its new
/// configuration.
pub const RESTART_EXIT_CODE: i32 = 3;

/// Keys a central document may not set: what a service needs to reach
/// CouchDB, or its identity (`section.key`, or a whole section).
pub const BOOTSTRAP_KEYS: [&str; 9] = [
    "rabbitmq",
    "couchdb",
    "secrets",
    "health",
    "log",
    "central",
    "instance",
    "security.internal",
    "runner.pool",
];

/// Where an effective key comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Central,
    File,
    Env,
}

/// `[central]` section (local only).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CentralSection {
    /// Read `regency_config` and follow it (D21).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Random delay before restarting for a new configuration, so replicas
    /// do not all restart at once.
    #[serde(default = "default_jitter")]
    pub restart_jitter_sec: u64,
    /// Instance status period.
    #[serde(default = "default_status_interval")]
    pub status_interval_sec: u64,
}

fn yes() -> bool {
    true
}
fn default_jitter() -> u64 {
    30
}
fn default_status_interval() -> u64 {
    30
}

impl Default for CentralSection {
    fn default() -> Self {
        Self {
            enabled: true,
            restart_jitter_sec: default_jitter(),
            status_interval_sec: default_status_interval(),
        }
    }
}

/// A central configuration document: `service:{service}` or
/// `service:runner:{pool}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServiceConfigDoc {
    #[serde(rename = "type")]
    pub kind: String,
    pub service: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// Same sections and keys as the service's TOML file.
    #[serde(default)]
    pub settings: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub updated_by: String,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

pub const CONFIG_DOC_TYPE: &str = "service_config";

/// Id of a central document.
pub fn config_doc_id(service: &str, pool: Option<&str>) -> String {
    match pool {
        Some(pool) => format!("service:{service}:{pool}"),
        None => format!("service:{service}"),
    }
}

/// Documents applying to an instance, lowest precedence first.
pub fn doc_ids(service: &str, pool: Option<&str>) -> Vec<String> {
    let mut ids = vec![config_doc_id(service, None)];
    if let Some(pool) = pool {
        ids.push(config_doc_id(service, Some(pool)));
    }
    ids
}

/// Bootstrap keys found in central settings (refused by backend, ignored by
/// services).
pub fn bootstrap_keys_in(settings: &toml::Table) -> Vec<String> {
    let mut found = Vec::new();
    for key in BOOTSTRAP_KEYS {
        match key.split_once('.') {
            None if settings.contains_key(key) => found.push(key.to_string()),
            Some((section, name))
                if settings
                    .get(section)
                    .and_then(toml::Value::as_table)
                    .is_some_and(|t| t.contains_key(name)) =>
            {
                found.push(key.to_string())
            }
            _ => {}
        }
    }
    found
}

fn strip_bootstrap(settings: &mut toml::Table) {
    for key in BOOTSTRAP_KEYS {
        match key.split_once('.') {
            None => {
                settings.remove(key);
            }
            Some((section, name)) => {
                if let Some(table) = settings
                    .get_mut(section)
                    .and_then(toml::Value::as_table_mut)
                {
                    table.remove(name);
                }
            }
        }
    }
}

/// JSON settings → TOML table (`null` is not representable: refused).
pub fn json_to_table(
    settings: &serde_json::Map<String, serde_json::Value>,
) -> Result<toml::Table, String> {
    toml::Table::try_from(settings).map_err(|e| format!("not representable in TOML: {e}"))
}

/// Recursive merge: `upper` wins; tables merge, anything else is replaced.
pub fn merge(lower: &mut toml::Table, upper: &toml::Table) {
    for (key, value) in upper {
        match (lower.get_mut(key), value) {
            (Some(toml::Value::Table(low)), toml::Value::Table(up)) => merge(low, up),
            _ => {
                lower.insert(key.clone(), value.clone());
            }
        }
    }
}

fn leaves(table: &toml::Table, prefix: &str, out: &mut BTreeMap<String, toml::Value>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            toml::Value::Table(inner) => leaves(inner, &path, out),
            other => {
                out.insert(path, other.clone());
            }
        }
    }
}

/// The layered table and the origin of every key set by a layer (absent
/// keys take the type's defaults).
pub fn layer(
    central: &toml::Table,
    file: &toml::Table,
    vars: &[(String, String)],
) -> (toml::Table, BTreeMap<String, Origin>) {
    let mut central = central.clone();
    strip_bootstrap(&mut central);
    let mut merged = central.clone();
    merge(&mut merged, file);
    let mut effective = merged.clone();
    crate::config::apply_env_overrides(&mut effective, vars.iter().cloned());

    let mut origins = BTreeMap::new();
    let (mut c, mut f, mut m, mut e) = (
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    leaves(&central, "", &mut c);
    leaves(file, "", &mut f);
    leaves(&merged, "", &mut m);
    leaves(&effective, "", &mut e);
    for (path, value) in &e {
        let origin = if m.get(path) != Some(value) {
            Origin::Env
        } else if f.contains_key(path) {
            Origin::File
        } else if c.contains_key(path) {
            Origin::Central
        } else {
            continue;
        };
        origins.insert(path.clone(), origin);
    }
    (effective, origins)
}

/// Deserialize a layered table into a service's config type.
pub fn deserialize<T: DeserializeOwned>(table: &toml::Table) -> Result<T, String> {
    toml::Value::Table(table.clone())
        .try_into()
        .map_err(|e: toml::de::Error| e.to_string())
}

static RESTART: AtomicBool = AtomicBool::new(false);
static RESTART_SIGNAL: std::sync::LazyLock<CancellationToken> =
    std::sync::LazyLock::new(CancellationToken::new);

/// Cancelled when the process must stop to apply a new configuration
/// ([`crate::telemetry::shutdown_signal`] resolves on it like on SIGTERM).
pub fn restart_signal() -> CancellationToken {
    RESTART_SIGNAL.clone()
}

/// Whether the process stopped to apply a new configuration (exit with
/// [`RESTART_EXIT_CODE`]).
pub fn restart_requested() -> bool {
    RESTART.load(Ordering::SeqCst)
}

/// Exit status of a service whose `run` returned `Ok`.
pub fn exit_code() -> i32 {
    if restart_requested() {
        RESTART_EXIT_CODE
    } else {
        0
    }
}

/// Local part of the configuration, read before anything else.
pub struct Bootstrap {
    pub path: String,
    pub file: toml::Table,
    pub vars: Vec<(String, String)>,
    pub common: CommonConfig,
    pub central: CentralSection,
    /// Runner pool (runners only; `None` for other services).
    pub pool: Option<String>,
}

#[derive(Deserialize)]
struct BootstrapRaw {
    #[serde(flatten)]
    common: CommonConfig,
    #[serde(default)]
    central: CentralSection,
}

impl Bootstrap {
    /// Read the local file (missing = empty) and the environment.
    pub fn load(service: &str, path: &str) -> Result<Self, ConfigError> {
        let raw = crate::config::read_file(path)?;
        let file: toml::Table = raw
            .parse()
            .map_err(|e: toml::de::Error| ConfigError::Toml {
                path: path.to_string(),
                details: e.to_string(),
            })?;
        let vars: Vec<(String, String)> = std::env::vars().collect();
        let (local, _) = layer(&toml::Table::new(), &file, &vars);
        let boot: BootstrapRaw = deserialize(&local).map_err(|details| ConfigError::Toml {
            path: path.to_string(),
            details,
        })?;
        let pool = (service == "runner").then(|| {
            local
                .get("runner")
                .and_then(|r| r.get("pool"))
                .and_then(toml::Value::as_str)
                .unwrap_or("default")
                .to_string()
        });
        Ok(Self {
            path: path.to_string(),
            file,
            vars,
            common: boot.common,
            central: boot.central,
            pool,
        })
    }

    /// Fetch the central documents (waiting for CouchDB) and build the
    /// effective configuration.
    pub async fn resolve<T: DeserializeOwned + Serialize>(
        self,
        service: &'static str,
        version: &'static str,
        check: Check<T>,
        cancel: &CancellationToken,
    ) -> Result<Resolved<T>, String> {
        let instance = self.common.instance();
        let couch = if self.central.enabled {
            let secrets = self.common.secrets.pool().map_err(|e| e.to_string())?;
            Some(
                Couch::from_config(&self.common.couchdb, &secrets)
                    .await
                    .map_err(|e| format!("CouchDB client: {e}"))?,
            )
        } else {
            None
        };
        let documents = match &couch {
            Some(couch) => loop {
                match fetch(couch, service, self.pool.as_deref()).await {
                    Ok(documents) => break documents,
                    Err(e) => {
                        warn!(error = %e, "cannot read the central configuration, retrying");
                        tokio::select! {
                            _ = cancel.cancelled() => return Err("stopped".into()),
                            _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                        }
                    }
                }
            },
            None => Documents::default(),
        };
        let (effective, origins) = layer(&documents.table, &self.file, &self.vars);
        let config: T = deserialize(&effective)
            .map_err(|e| format!("invalid configuration ({} + central): {e}", self.path))?;
        if !documents.ignored.is_empty() {
            warn!(keys = ?documents.ignored, "central configuration sets local-only keys, ignored");
        }
        Ok(Resolved {
            config,
            service,
            version,
            instance,
            check,
            couch,
            bootstrap: self,
            effective,
            origins,
            revisions: documents.revisions,
        })
    }
}

#[derive(Default)]
struct Documents {
    table: toml::Table,
    revisions: BTreeMap<String, String>,
    ignored: Vec<String>,
    errors: Vec<String>,
}

async fn fetch(couch: &Couch, service: &str, pool: Option<&str>) -> Result<Documents, CouchError> {
    let mut documents = Documents::default();
    for id in doc_ids(service, pool) {
        let doc = match couch.get::<ServiceConfigDoc>(DB_CONFIG, &id).await {
            Ok(doc) => doc,
            Err(e) if e.is_not_found() => None,
            Err(e) => return Err(e),
        };
        let Some(doc) = doc else { continue };
        if let Some(rev) = &doc.rev {
            documents.revisions.insert(id.clone(), rev.clone());
        }
        match json_to_table(&doc.doc.settings) {
            Ok(table) => {
                documents.ignored.extend(bootstrap_keys_in(&table));
                merge(&mut documents.table, &table);
            }
            Err(e) => documents.errors.push(format!("{id}: {e}")),
        }
    }
    Ok(documents)
}

/// Validation of a candidate configuration by its service (D19 gate,
/// files...). Returns the violations.
pub type Check<T> = Arc<dyn Fn(&T) -> Vec<Result<(), String>> + Send + Sync>;

/// The effective configuration of a running service.
pub struct Resolved<T> {
    pub config: T,
    pub service: &'static str,
    pub version: &'static str,
    pub instance: String,
    check: Check<T>,
    pub couch: Option<Couch>,
    pub bootstrap: Bootstrap,
    pub effective: toml::Table,
    pub origins: BTreeMap<String, Origin>,
    pub revisions: BTreeMap<String, String>,
}

/// An instance's status on `regency.status` (JSON), stored by dbwriter as
/// `instance:{service}:{instance}` in `regency_state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatus {
    pub schema_version: u32,
    pub service: String,
    pub instance: String,
    /// The service's own version (D20).
    pub version: String,
    pub sdk_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    pub started_at: DateTime<Utc>,
    pub at: DateTime<Utc>,
    pub ready: bool,
    /// Readiness checks: name → "ok" or the reason.
    #[serde(default)]
    pub checks: BTreeMap<String, String>,
    pub security_internal: u8,
    pub security_external: u8,
    /// Effective configuration (defaults included; secrets are references).
    pub config: serde_json::Value,
    /// Origin of each key set by a layer; other keys are defaults.
    #[serde(default)]
    pub origins: BTreeMap<String, Origin>,
    /// `_rev` of each central document applied.
    #[serde(default)]
    pub central_revisions: BTreeMap<String, String>,
    /// Whether the instance follows the central configuration.
    pub central: bool,
    /// A central change that could not be applied (kept running as is).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
    /// A new configuration is about to be applied (restart).
    #[serde(default)]
    pub restart_pending: bool,
}

impl InstanceStatus {
    pub fn routing_key(&self) -> String {
        format!("instance.{}", self.service)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let status: Self = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if status.schema_version == 0 || status.schema_version > SCHEMA_VERSION {
            return Err(format!(
                "unsupported schema_version {}",
                status.schema_version
            ));
        }
        Ok(status)
    }
}

impl<T: DeserializeOwned + Serialize + Send + Sync + 'static> Resolved<T> {
    /// Start the status publisher and, if enabled, the configuration watcher.
    /// `check` validates a new configuration before restarting into it.
    pub fn spawn(&self, health: &Health) {
        let (version, instance, check) = (self.version, self.instance.clone(), self.check.clone());
        let instance = instance.as_str();
        // these tasks live as long as the process
        let cancel = &CancellationToken::new();
        let common = &self.bootstrap.common;
        let status = Arc::new(Mutex::new(InstanceStatus {
            schema_version: SCHEMA_VERSION,
            service: self.service.to_string(),
            instance: instance.to_string(),
            version: version.to_string(),
            sdk_version: regent_sdk::VERSION.to_string(),
            pool: self.bootstrap.pool.clone(),
            started_at: Utc::now(),
            at: Utc::now(),
            ready: false,
            checks: BTreeMap::new(),
            security_internal: common.security.internal as u8,
            security_external: self.external_level(),
            config: serde_json::to_value(&self.config).unwrap_or_default(),
            origins: self.origins.clone(),
            central_revisions: self.revisions.clone(),
            central: self.bootstrap.central.enabled,
            config_error: None,
            restart_pending: false,
        }));
        spawn_status(
            common,
            format!("{}/{instance}/status", self.service),
            Duration::from_secs(self.bootstrap.central.status_interval_sec.max(5)),
            status.clone(),
            health.clone(),
            cancel.clone(),
        );
        if let Some(couch) = &self.couch {
            let watcher = Watcher {
                couch: couch.clone(),
                service: self.service,
                pool: self.bootstrap.pool.clone(),
                file: self.bootstrap.file.clone(),
                vars: self.bootstrap.vars.clone(),
                applied: self.effective.clone(),
                jitter: Duration::from_secs(self.bootstrap.central.restart_jitter_sec),
                status,
                check,
                cancel: cancel.clone(),
                _config: std::marker::PhantomData::<T>,
            };
            tokio::spawn(watcher.run());
        }
    }

    fn external_level(&self) -> u8 {
        self.effective
            .get("security")
            .and_then(|s| s.get("external"))
            .and_then(|v| match v {
                toml::Value::Integer(n) => Some(n.to_string()),
                toml::Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .and_then(|text| crate::security::Level::parse(&text))
            .unwrap_or(crate::security::Level::MAX) as u8
    }
}

fn spawn_status(
    common: &CommonConfig,
    name: String,
    interval: Duration,
    status: Arc<Mutex<InstanceStatus>>,
    health: Health,
    cancel: CancellationToken,
) {
    let Ok(secrets) = common.secrets.pool() else {
        return;
    };
    // own connection and health (its readiness is not the service's)
    let link = PublisherLink::spawn(
        common.rabbitmq.clone(),
        secrets,
        name,
        Health::new(),
        cancel.clone(),
    );
    tokio::spawn(async move {
        loop {
            let message = {
                let mut status = status.lock().expect("status lock");
                status.at = Utc::now();
                status.ready = health.is_ready();
                status.checks = health.checks();
                status.clone()
            };
            if let Some(publisher) = link.wait(Duration::from_secs(5)).await {
                match message.to_bytes() {
                    Ok(bytes) => {
                        if let Err(e) = publisher
                            .publish_json(EX_STATUS, &message.routing_key(), &bytes)
                            .await
                        {
                            debug!(error = %e, "instance status not published");
                        }
                    }
                    Err(e) => error!(error = %e, "cannot encode the instance status"),
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(interval) => {}
            }
        }
    });
}

struct Watcher<T> {
    couch: Couch,
    service: &'static str,
    pool: Option<String>,
    file: toml::Table,
    vars: Vec<(String, String)>,
    applied: toml::Table,
    jitter: Duration,
    status: Arc<Mutex<InstanceStatus>>,
    check: Check<T>,
    cancel: CancellationToken,
    _config: std::marker::PhantomData<T>,
}

impl<T: DeserializeOwned + Send + Sync + 'static> Watcher<T> {
    async fn run(self) {
        let ids = doc_ids(self.service, self.pool.as_deref());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let (couch, cancel, ids) = (self.couch.clone(), self.cancel.clone(), ids.clone());
            tokio::spawn(async move {
                follow(
                    &couch,
                    DB_CONFIG,
                    Since::Now,
                    &cancel,
                    |change| {
                        if ids.contains(&change.id) {
                            let _ = tx.send(());
                        }
                    },
                    || {},
                )
                .await;
            });
        }
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                received = rx.recv() => if received.is_none() { return },
            }
            while rx.try_recv().is_ok() {}
            let documents = match fetch(&self.couch, self.service, self.pool.as_deref()).await {
                Ok(documents) => documents,
                Err(e) => {
                    warn!(error = %e, "cannot read the changed central configuration");
                    continue;
                }
            };
            let (effective, _) = layer(&documents.table, &self.file, &self.vars);
            if effective == self.applied {
                debug!("central configuration changed, effective configuration unchanged");
                self.status.lock().expect("status lock").config_error = None;
                continue;
            }
            let verdict = if documents.errors.is_empty() {
                deserialize::<T>(&effective).and_then(|config| {
                    let failures: Vec<String> = (self.check)(&config)
                        .into_iter()
                        .filter_map(Result::err)
                        .collect();
                    if failures.is_empty() {
                        Ok(())
                    } else {
                        Err(failures.join("; "))
                    }
                })
            } else {
                Err(documents.errors.join("; "))
            };
            match verdict {
                Err(e) => {
                    error!(error = %e, "new central configuration refused, keeping the current one");
                    self.status.lock().expect("status lock").config_error = Some(e);
                }
                Ok(()) => {
                    let delay = crate::amqp::jitter(self.jitter);
                    info!(
                        delay_ms = delay.as_millis() as u64,
                        "new central configuration accepted, restarting to apply it"
                    );
                    {
                        let mut status = self.status.lock().expect("status lock");
                        status.config_error = None;
                        status.restart_pending = true;
                    }
                    tokio::select! {
                        _ = self.cancel.cancelled() => return,
                        _ = tokio::time::sleep(delay) => {}
                    }
                    RESTART.store(true, Ordering::SeqCst);
                    RESTART_SIGNAL.cancel();
                    return;
                }
            }
        }
    }
}

/// Startup of a service (D15, D19, D21): local configuration, logging, the
/// internal-level gate (before any connection), the central configuration,
/// then the full gate. Exits with status 2 on any failure.
pub async fn startup<T: DeserializeOwned + Serialize + Send + Sync + 'static>(
    service: &'static str,
    version: &'static str,
    path: &str,
    uses_rabbitmq: bool,
    uses_couchdb: bool,
    common: fn(&T) -> &CommonConfig,
    external: fn(&T) -> Vec<Result<(), String>>,
) -> Resolved<T> {
    let boot = match Bootstrap::load(service, path) {
        Ok(boot) => boot,
        Err(e) => {
            eprintln!("regency-{service}: {e}");
            std::process::exit(2);
        }
    };
    crate::telemetry::init_logging(&boot.common.log, service, version, &boot.common.instance());
    let uses_couchdb = uses_couchdb || boot.central.enabled;
    if let Err(e) = boot.common.check_internal(uses_rabbitmq, uses_couchdb) {
        crate::security::enforce_at_startup(&boot.common.security, [Err(e)]);
    }
    let check: Check<T> = Arc::new(move |config: &T| {
        let mut checks = vec![common(config).check_internal(uses_rabbitmq, uses_couchdb)];
        checks.extend(external(config));
        checks
    });
    let resolved = match boot
        .resolve::<T>(service, version, check.clone(), &CancellationToken::new())
        .await
    {
        Ok(resolved) => resolved,
        Err(e) => {
            error!(error = %e, "refusing to start");
            eprintln!("regency-{service}: {e}");
            std::process::exit(2);
        }
    };
    crate::security::enforce_at_startup(
        &common(&resolved.config).security,
        check(&resolved.config),
    );
    resolved
}

/// Validation by backend of a central document for `service` (D21): refuses
/// bootstrap keys, and checks the document merged with each live instance's
/// local layers (or alone when no instance runs; then missing required keys
/// are tolerated, the instance checks again before restarting).
pub fn validate(
    service: &str,
    settings: &toml::Table,
    instances: &[InstanceStatus],
) -> Result<(), String> {
    let bootstrap = bootstrap_keys_in(settings);
    if !bootstrap.is_empty() {
        return Err(format!(
            "local-only keys cannot be set centrally: {}",
            bootstrap.join(", ")
        ));
    }
    let check = |table: &toml::Table| -> Result<(), String> {
        match service {
            "scheduler" => checks_of::<crate::services::scheduler::Config>(
                table,
                crate::services::scheduler::external_checks,
            ),
            "runner" => checks_of::<crate::services::runner::Config>(
                table,
                crate::services::runner::external_checks,
            ),
            "dbwriter" => checks_of::<crate::services::dbwriter::Config>(
                table,
                crate::services::dbwriter::external_checks,
            ),
            "backend" => checks_of::<crate::services::backend::Config>(
                table,
                crate::services::backend::external_checks,
            ),
            "endpoint" => checks_of::<crate::services::endpoint::Config>(
                table,
                crate::services::endpoint::external_checks,
            ),
            "sourcesync" => checks_of::<crate::services::sourcesync::Config>(
                table,
                crate::services::sourcesync::external_checks,
            ),
            other => Err(format!("unknown service {other}")),
        }
    };
    let live: Vec<&InstanceStatus> = instances.iter().filter(|i| i.service == service).collect();
    if live.is_empty() {
        return match check(settings) {
            Err(e) if e.starts_with("missing field") => Ok(()),
            other => other,
        };
    }
    for instance in live {
        let local = local_layers(instance);
        let mut candidate = settings.clone();
        merge(&mut candidate, &local);
        check(&candidate).map_err(|e| format!("instance {}: {e}", instance.instance))?;
    }
    Ok(())
}

fn checks_of<T: DeserializeOwned>(
    table: &toml::Table,
    checks: fn(&T) -> Vec<Result<(), String>>,
) -> Result<(), String> {
    let config: T = deserialize(table)?;
    let failures: Vec<String> = checks(&config)
        .into_iter()
        .filter_map(Result::err)
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// The keys an instance got from its file or environment.
pub fn local_layers(instance: &InstanceStatus) -> toml::Table {
    let mut table = toml::Table::new();
    let Ok(toml::Value::Table(effective)) = toml::Value::try_from(&instance.config) else {
        return table;
    };
    let mut values = BTreeMap::new();
    leaves(&effective, "", &mut values);
    for (path, origin) in &instance.origins {
        if *origin == Origin::Central {
            continue;
        }
        let Some(value) = values.get(path) else {
            continue;
        };
        let mut parts: Vec<&str> = path.split('.').collect();
        let last = parts.pop().expect("non-empty path");
        let mut cursor = &mut table;
        for part in parts {
            cursor = cursor
                .entry(part.to_string())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                .as_table_mut()
                .expect("table");
        }
        cursor.insert(last.to_string(), value.clone());
    }
    table
}

/// Instance docs in `regency_state` (D21).
pub const INSTANCE_DOC_TYPE: &str = "instance";

pub fn instance_doc_id(service: &str, instance: &str) -> String {
    format!("instance:{service}:{instance}")
}

/// `instance:{service}:{instance}` in `regency_state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceDoc {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub status: InstanceStatus,
}

impl PartialEq for InstanceDoc {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_value(self).ok() == serde_json::to_value(other).ok()
    }
}

/// An instance unseen for this long is shown as gone.
pub const INSTANCE_GONE_AFTER: chrono::TimeDelta = chrono::TimeDelta::minutes(2);
/// An instance unseen for this long is removed by dbwriter.
pub const INSTANCE_FORGOTTEN_AFTER: chrono::TimeDelta = chrono::TimeDelta::days(7);

/// Store of a central document, for dbwriter.
pub fn stored(doc: ServiceConfigDoc) -> Stored<ServiceConfigDoc> {
    Stored::new(config_doc_id(&doc.service, doc.pool.as_deref()), doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(text: &str) -> toml::Table {
        text.parse().unwrap()
    }

    #[test]
    fn layers_in_order_with_origins() {
        let central = t(
            "[runner]\nassess_concurrency = 4\nreach_prefetch = 2\npool = \"x\"\n[security]\nexternal = 1\ninternal = 0\n",
        );
        let file = t("[runner]\nreach_prefetch = 8\n");
        let vars = vec![("REGENCY_SECURITY_EXTERNAL".to_string(), "2".to_string())];
        let (effective, origins) = layer(&central, &file, &vars);
        let runner = effective["runner"].as_table().unwrap();
        assert_eq!(runner["assess_concurrency"].as_integer(), Some(4));
        assert_eq!(runner["reach_prefetch"].as_integer(), Some(8));
        assert!(!runner.contains_key("pool"), "bootstrap key from central");
        assert!(
            !effective["security"]
                .as_table()
                .unwrap()
                .contains_key("internal")
        );
        assert_eq!(effective["security"]["external"].as_integer(), Some(2));
        assert_eq!(origins["runner.assess_concurrency"], Origin::Central);
        assert_eq!(origins["runner.reach_prefetch"], Origin::File);
        assert_eq!(origins["security.external"], Origin::Env);
    }

    #[test]
    fn validation_refuses_bootstrap_keys_and_weak_settings() {
        let err = validate("runner", &t("[rabbitmq]\nurl = \"amqp://x\"\n"), &[]).unwrap_err();
        assert!(err.contains("rabbitmq"), "{err}");
        // backend alone: required session_key missing is tolerated
        validate("backend", &t("[backend]\nsession_ttl_sec = 600\n"), &[]).unwrap();
        // unknown key in a section is not
        assert!(validate("runner", &t("[runner]\nnope = 1\n"), &[]).is_err());
    }

    #[test]
    fn validation_merges_live_instances_local_layers() {
        let local = serde_json::json!({
            "backend": { "session_key": { "SecRef": "k" }, "tls_offloaded": false }
        });
        let instance = InstanceStatus {
            schema_version: 1,
            service: "backend".into(),
            instance: "b-1".into(),
            version: "0.1.0".into(),
            sdk_version: "x".into(),
            pool: None,
            started_at: Utc::now(),
            at: Utc::now(),
            ready: true,
            checks: BTreeMap::new(),
            security_internal: 1,
            security_external: 1,
            config: local,
            origins: BTreeMap::from([
                ("backend.session_key.SecRef".to_string(), Origin::File),
                ("backend.tls_offloaded".to_string(), Origin::File),
            ]),
            central_revisions: BTreeMap::new(),
            central: true,
            config_error: None,
            restart_pending: false,
        };
        validate(
            "backend",
            &t("[security]\nexternal = 1\n"),
            std::slice::from_ref(&instance),
        )
        .unwrap();
        let err = validate("backend", &t("[security]\nexternal = 2\n"), &[instance]).unwrap_err();
        assert!(err.contains("b-1") && err.contains("TLS"), "{err}");
    }
}
