//! Service configuration conventions (D15).
//!
//! - TOML file, by default `/etc/regency/<service>.toml`;
//! - every key overridable by an environment variable
//!   `REGENCY_<SECTION>_<KEY>` (top-level keys: `REGENCY_<KEY>`), e.g.
//!   `REGENCY_RABBITMQ_URL` sets `[rabbitmq] url`. Section names never contain
//!   `_`, so the first `_` after the section separates it from the key. Values
//!   are read as TOML scalars when they parse as such (`42`, `true`), else as
//!   strings;
//! - secrets only as [`SecretReference`]s, resolved through a
//!   [`SecretProvidersPool`] built from the `[secrets]` section.

use std::collections::HashMap;
use std::path::Path;

use regent_sdk::secrets::{SecretProvidersPool, SecretReference};
use regent_sdk::{SecretProvider, SecretProvidersPoolBuilder};
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// Configuration loading error.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid TOML in {path}: {details}")]
    Toml { path: String, details: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("secret {reference}: {details}")]
    Secret { reference: String, details: String },
}

/// Default path of a service's configuration file.
/// `/etc/regency/<service>.toml`; on Windows
/// `%ProgramData%\regency\<service>.toml` (D22).
pub fn default_path(service: &str) -> String {
    #[cfg(windows)]
    {
        let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string());
        format!(r"{base}\regency\{service}.toml")
    }
    #[cfg(not(windows))]
    {
        format!("/etc/regency/{service}.toml")
    }
}

/// Load `path` (a missing file counts as empty, so env vars alone are enough,
/// e.g. in containers), apply `REGENCY_*` overrides from the process
/// environment, then deserialize.
pub fn load<T: DeserializeOwned>(path: &str) -> Result<T, ConfigError> {
    let raw = read_file(path)?;
    let vars: Vec<(String, String)> = std::env::vars().collect();
    load_from_str(&raw, path, vars)
}

/// Read a configuration file; a missing file counts as empty.
pub fn read_file(path: &str) -> Result<String, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !Path::new(path).exists() => {
            Ok(String::new())
        }
        Err(source) => Err(ConfigError::Read {
            path: path.to_string(),
            source,
        }),
    }
}

/// [`load`] on explicit inputs (testable).
pub fn load_from_str<T: DeserializeOwned>(
    raw: &str,
    origin: &str,
    vars: impl IntoIterator<Item = (String, String)>,
) -> Result<T, ConfigError> {
    let mut table: toml::Table = raw
        .parse()
        .map_err(|e: toml::de::Error| ConfigError::Toml {
            path: origin.to_string(),
            details: e.to_string(),
        })?;
    apply_env_overrides(&mut table, vars);
    toml::Value::Table(table)
        .try_into()
        .map_err(|e: toml::de::Error| ConfigError::Toml {
            path: origin.to_string(),
            details: e.to_string(),
        })
}

pub(crate) fn apply_env_overrides(
    table: &mut toml::Table,
    vars: impl IntoIterator<Item = (String, String)>,
) {
    for (name, value) in vars {
        let Some(rest) = name.strip_prefix("REGENCY_") else {
            continue;
        };
        let rest = rest.to_ascii_lowercase();
        if rest.is_empty() {
            continue;
        }
        let value = parse_scalar(&value);
        match rest.split_once('_') {
            // `REGENCY_RABBITMQ_URL` -> [rabbitmq] url, when `rabbitmq` is a
            // section (present in the file, or the key is unknown at top
            // level); a top-level key present in the file wins
            // (`REGENCY_RABBITMQ_MANAGEMENT_URL` -> rabbitmq_management_url)
            Some((section, key))
                if !table.get(&rest).is_some_and(|v| !v.is_table())
                    && (table.get(section).is_some_and(toml::Value::is_table)
                        || !table.contains_key(&rest)) =>
            {
                let entry = table
                    .entry(section.to_string())
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                if let toml::Value::Table(section) = entry {
                    section.insert(key.to_string(), value);
                }
            }
            _ => {
                table.insert(rest, value);
            }
        }
    }
}

fn parse_scalar(raw: &str) -> toml::Value {
    if let Ok(value) = raw.parse::<i64>() {
        return toml::Value::Integer(value);
    }
    if let Ok(value) = raw.parse::<bool>() {
        return toml::Value::Boolean(value);
    }
    if let Ok(value) = raw.parse::<f64>() {
        return toml::Value::Float(value);
    }
    toml::Value::String(raw.to_string())
}

/// `[rabbitmq]` section. Credentials never appear in the URL.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RabbitMqConfig {
    /// `amqp://host:5672/vhost` or `amqps://…` (without credentials).
    #[serde(default = "default_amqp_url")]
    pub url: String,
    /// RabbitMQ stream protocol endpoint (for `regency.control`, D7).
    #[serde(default = "default_stream_host")]
    pub stream_host: String,
    #[serde(default = "default_stream_port")]
    pub stream_port: u16,
    #[serde(default = "default_vhost")]
    pub vhost: String,
    #[serde(default)]
    pub username: Option<String>,
    /// Password, as a secret reference.
    #[serde(default)]
    pub password: Option<SecretReference>,
    /// AMQP heartbeat, seconds (D3: short, 5–10 s).
    #[serde(default = "default_heartbeat")]
    pub heartbeat_sec: u16,
    /// TLS trust anchor (PEM file): verifies the server. Required from
    /// security level 2 (D19).
    #[serde(default)]
    pub ca_file: Option<String>,
    /// Client certificate (PEM file) for mutual TLS (level 3).
    #[serde(default)]
    pub cert_file: Option<String>,
    /// Client private key (PEM), as a secret reference (level 3).
    #[serde(default)]
    pub key: Option<SecretReference>,
}

fn default_amqp_url() -> String {
    "amqp://localhost:5672".to_string()
}
fn default_stream_host() -> String {
    "localhost".to_string()
}
fn default_stream_port() -> u16 {
    5552
}
fn default_vhost() -> String {
    "/".to_string()
}
fn default_heartbeat() -> u16 {
    10
}

impl Default for RabbitMqConfig {
    fn default() -> Self {
        Self {
            url: default_amqp_url(),
            stream_host: default_stream_host(),
            stream_port: default_stream_port(),
            vhost: default_vhost(),
            username: None,
            password: None,
            heartbeat_sec: default_heartbeat(),
            ca_file: None,
            cert_file: None,
            key: None,
        }
    }
}

/// `[couchdb]` section.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct CouchDbConfig {
    #[serde(default = "default_couch_url")]
    pub url: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<SecretReference>,
    /// TLS trust anchor (PEM file): verifies the server. Required from
    /// security level 2 (D19).
    #[serde(default)]
    pub ca_file: Option<String>,
    /// Client certificate (PEM file) for mutual TLS (level 3).
    #[serde(default)]
    pub cert_file: Option<String>,
    /// Client private key (PEM), as a secret reference (level 3).
    #[serde(default)]
    pub key: Option<SecretReference>,
}

fn default_couch_url() -> String {
    "http://localhost:5984".to_string()
}

impl Default for CouchDbConfig {
    fn default() -> Self {
        Self {
            url: default_couch_url(),
            username: None,
            password: None,
            ca_file: None,
            cert_file: None,
            key: None,
        }
    }
}

/// `[health]` section: the separate port for `/healthz`, `/readyz`, `/metrics`.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthConfig {
    #[serde(default = "default_health_listen")]
    pub listen: String,
}

fn default_health_listen() -> String {
    "0.0.0.0:9090".to_string()
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            listen: default_health_listen(),
        }
    }
}

/// `[log]` section.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// `json` (default) or `text`.
    #[serde(default = "default_log_format")]
    pub format: String,
    /// `tracing` env-filter directive, e.g. `info,lapin=warn`. `RUST_LOG`
    /// overrides it.
    #[serde(default = "default_log_level")]
    pub level: String,
    /// Append the logs to this file instead of stdout (e.g. Windows services,
    /// which have no console). Rotation is left to the platform's tooling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

fn default_log_format() -> String {
    "json".to_string()
}
fn default_log_level() -> String {
    "info,lapin=warn".to_string()
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            format: default_log_format(),
            level: default_log_level(),
            file: None,
        }
    }
}

/// One secret provider of the `[secrets]` section.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretProviderConfig {
    /// Name used as `Provider` in secret references.
    pub name: String,
    /// `files` (the reference is a file path) or `env` (an env var name).
    pub kind: String,
}

/// `[secrets]` section: the providers of this process (D9: each runner pool
/// may hold different credentials).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    #[serde(default = "default_providers")]
    pub providers: Vec<SecretProviderConfig>,
    /// Provider used by references without `Provider`.
    #[serde(default = "default_provider_name")]
    pub default: String,
}

fn default_providers() -> Vec<SecretProviderConfig> {
    vec![
        SecretProviderConfig {
            name: "files".to_string(),
            kind: "files".to_string(),
        },
        SecretProviderConfig {
            name: "env".to_string(),
            kind: "env".to_string(),
        },
    ]
}
fn default_provider_name() -> String {
    "files".to_string()
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            providers: default_providers(),
            default: default_provider_name(),
        }
    }
}

impl SecretsConfig {
    /// Build the SDK pool from these providers.
    pub fn pool(&self) -> Result<SecretProvidersPool, ConfigError> {
        let mut providers: HashMap<&str, SecretProvider> = HashMap::new();
        for provider in &self.providers {
            let built = match provider.kind.as_str() {
                "files" => SecretProvider::files(&provider.name),
                "env" => SecretProvider::env_var(&provider.name),
                other => {
                    return Err(ConfigError::Invalid(format!(
                        "secret provider {}: unknown kind {other} (expected files or env)",
                        provider.name
                    )));
                }
            };
            providers.insert(&provider.name, built);
        }
        let default = providers
            .get(self.default.as_str())
            .cloned()
            .ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "default secret provider {} is not declared",
                    self.default
                ))
            })?;
        let mut builder = SecretProvidersPoolBuilder::new();
        for (name, provider) in &providers {
            if *name != self.default {
                builder = builder.add_provider(provider);
            }
        }
        builder
            .with_default_provider(&default)
            .build()
            .map_err(|e| ConfigError::Invalid(format!("secret providers: {e}")))
    }
}

/// Resolve a secret reference to its raw value (trailing newline trimmed).
pub async fn resolve_secret(
    pool: &SecretProvidersPool,
    reference: &SecretReference,
) -> Result<String, ConfigError> {
    pool.get_secret_raw(reference)
        .await
        .map(|secret| secret.inner().trim_end_matches(['\n', '\r']).to_string())
        .map_err(|e| ConfigError::Secret {
            reference: reference.sec_ref().to_string(),
            details: e.to_string(),
        })
}

/// Sections every service has.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct CommonConfig {
    #[serde(default)]
    pub rabbitmq: RabbitMqConfig,
    #[serde(default)]
    pub couchdb: CouchDbConfig,
    #[serde(default)]
    pub health: HealthConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    /// Instance name (logs, consumer tags, `publisher`); default: hostname.
    #[serde(default)]
    pub instance: Option<String>,
    /// Security levels (D19); both default to 3 (hardened).
    #[serde(default)]
    pub security: crate::security::SecurityConfig,
}

impl CommonConfig {
    pub fn instance(&self) -> String {
        self.instance.clone().unwrap_or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| format!("pid-{}", std::process::id()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct Sample {
        #[serde(default)]
        rabbitmq: RabbitMqConfig,
        #[serde(default)]
        shards: Option<i64>,
        #[serde(default)]
        scheduler: Option<toml::Table>,
    }

    #[test]
    fn env_overrides_sections_and_top_level() {
        let raw = "shards = 4\n[rabbitmq]\nurl = \"amqp://a\"\n";
        let vars = [
            ("REGENCY_RABBITMQ_URL".to_string(), "amqp://b".to_string()),
            (
                "REGENCY_RABBITMQ_HEARTBEAT_SEC".to_string(),
                "5".to_string(),
            ),
            ("REGENCY_SHARDS".to_string(), "8".to_string()),
            (
                "REGENCY_SCHEDULER_LOCK_RETRY_SEC".to_string(),
                "3".to_string(),
            ),
            ("OTHER".to_string(), "x".to_string()),
        ];
        let sample: Sample = load_from_str(raw, "test", vars).unwrap();
        assert_eq!(sample.rabbitmq.url, "amqp://b");
        assert_eq!(sample.rabbitmq.heartbeat_sec, 5);
        assert_eq!(sample.shards, Some(8));
        assert_eq!(
            sample
                .scheduler
                .unwrap()
                .get("lock_retry_sec")
                .and_then(|v| v.as_integer()),
            Some(3)
        );
    }

    #[test]
    fn an_existing_top_level_key_wins_over_a_section() {
        #[derive(Debug, Deserialize)]
        struct Ctl {
            rabbitmq_management_url: String,
            #[serde(default)]
            rabbitmq: RabbitMqConfig,
        }
        let raw = "rabbitmq_management_url = \"http://a\"\n[rabbitmq]\nurl = \"amqp://a\"\n";
        let vars = [(
            "REGENCY_RABBITMQ_MANAGEMENT_URL".to_string(),
            "http://b".to_string(),
        )];
        let ctl: Ctl = load_from_str(raw, "test", vars).unwrap();
        assert_eq!(ctl.rabbitmq_management_url, "http://b");
        assert_eq!(ctl.rabbitmq.url, "amqp://a");
    }

    #[test]
    fn default_secrets_pool_builds() {
        SecretsConfig::default().pool().unwrap();
    }
}

/// TLS material of one infrastructure link (`ca_file`, `cert_file`, `key`),
/// read at connection time.
#[derive(Clone, Default)]
pub struct TlsMaterial {
    /// Trust anchor(s), PEM.
    pub ca_pem: Option<Vec<u8>>,
    /// Client certificate chain, PEM.
    pub cert_pem: Option<Vec<u8>>,
    /// Client private key, PEM.
    pub key_pem: Option<String>,
}

impl TlsMaterial {
    pub async fn load(
        ca_file: Option<&str>,
        cert_file: Option<&str>,
        key: Option<&SecretReference>,
        secrets: &SecretProvidersPool,
    ) -> Result<Self, ConfigError> {
        let read = |path: &str| {
            std::fs::read(path).map_err(|source| ConfigError::Read {
                path: path.to_string(),
                source,
            })
        };
        let key_pem = match key {
            Some(reference) => Some(resolve_secret(secrets, reference).await?),
            None => None,
        };
        let material = Self {
            ca_pem: ca_file.map(read).transpose()?,
            cert_pem: cert_file.map(read).transpose()?,
            key_pem,
        };
        if material.cert_pem.is_some() != material.key_pem.is_some() {
            return Err(ConfigError::Invalid(
                "cert_file and key go together".to_string(),
            ));
        }
        Ok(material)
    }

    pub fn has_identity(&self) -> bool {
        self.cert_pem.is_some() && self.key_pem.is_some()
    }

    /// Write the private key to a private (0600) file, for clients that only
    /// accept paths (the RabbitMQ stream client).
    pub fn key_file(&self, name: &str) -> Result<Option<std::path::PathBuf>, ConfigError> {
        let Some(key) = &self.key_pem else {
            return Ok(None);
        };
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("regency-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|source| ConfigError::Read {
            path: dir.display().to_string(),
            source,
        })?;
        let path = dir.join(name);
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // owner only (on Windows, the per-user temporary directory)
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        file.write_all(key.as_bytes())
            .map_err(|source| ConfigError::Read {
                path: path.display().to_string(),
                source,
            })?;
        Ok(Some(path))
    }
}

impl RabbitMqConfig {
    pub fn uses_tls(&self) -> bool {
        self.url.starts_with("amqps://")
    }

    pub async fn tls(&self, secrets: &SecretProvidersPool) -> Result<TlsMaterial, ConfigError> {
        TlsMaterial::load(
            self.ca_file.as_deref(),
            self.cert_file.as_deref(),
            self.key.as_ref(),
            secrets,
        )
        .await
    }
}

impl CouchDbConfig {
    pub async fn tls(&self, secrets: &SecretProvidersPool) -> Result<TlsMaterial, ConfigError> {
        TlsMaterial::load(
            self.ca_file.as_deref(),
            self.cert_file.as_deref(),
            self.key.as_ref(),
            secrets,
        )
        .await
    }
}

impl CommonConfig {
    /// D19 startup check of the internal links this service uses.
    pub fn check_internal(&self, uses_rabbitmq: bool, uses_couchdb: bool) -> Result<(), String> {
        let level = self.security.internal;
        crate::security::check_internal(
            level,
            uses_rabbitmq.then_some(&self.rabbitmq),
            uses_couchdb.then_some(&self.couchdb),
        )
        .into_result("internal", level)
    }
}
