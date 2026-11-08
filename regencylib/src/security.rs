//! Security levels (D19).
//!
//! Every service takes two levels at startup (`[security]`, or
//! `REGENCY_SECURITY_INTERNAL` / `REGENCY_SECURITY_EXTERNAL`):
//!
//! - `internal`: its links to the infrastructure (RabbitMQ, its stream port,
//!   CouchDB);
//! - `external`: what it exposes to or reaches outside (users for backend,
//!   managed hosts for endpoint and runners, Git servers for sourcesync).
//!
//! | Level | Name | Meaning |
//! |---|---|---|
//! | 0 | open | nothing required: anonymous, clear text, nothing verified |
//! | 1 | authenticated | every principal proves who it is; clear text tolerated; first-use trust allowed |
//! | 2 | encrypted | + every network link encrypted and every server identity verified (explicit CA, pins, known hosts); no first-use trust, no insecure modes |
//! | 3 | hardened | + mutual authentication with keys/certificates instead of passwords where the protocol allows, TLS 1.3, short-lived credentials |
//!
//! Both default to 3 (secure by default): a service whose configuration is
//! weaker than its levels refuses to start and lists every violation.
//! Secrets are references at every level (D9).

use std::fmt;

use regent_sdk::hosts::handlers::ConnectionMethod;
use regent_sdk::hosts::handlers::ssh2::Ssh2AuthReference;
use regent_sdk::hosts::handlers::sshoq::{SshoqAuthReference, SshoqTlsConfig};
use regent_sdk::hosts::managed_host::ManagedHostBuilder;
use serde::{Deserialize, Deserializer};

use crate::config::{CouchDbConfig, RabbitMqConfig};

/// A security level, 0 (open) to 3 (hardened).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Open = 0,
    Authenticated = 1,
    Encrypted = 2,
    Hardened = 3,
}

impl Level {
    pub const MAX: Level = Level::Hardened;

    pub fn name(self) -> &'static str {
        match self {
            Level::Open => "open",
            Level::Authenticated => "authenticated",
            Level::Encrypted => "encrypted",
            Level::Hardened => "hardened",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "0" | "open" => Level::Open,
            "1" | "authenticated" => Level::Authenticated,
            "2" | "encrypted" => Level::Encrypted,
            "3" | "hardened" => Level::Hardened,
            _ => return None,
        })
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", *self as u8, self.name())
    }
}

/// Serialized as its number (`0`..`3`).
impl serde::Serialize for Level {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for Level {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(i64),
            Text(String),
        }
        let text = match Raw::deserialize(deserializer)? {
            Raw::Number(n) => n.to_string(),
            Raw::Text(t) => t,
        };
        Level::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "invalid security level `{text}` (0 open, 1 authenticated, 2 encrypted, 3 hardened)"
            ))
        })
    }
}

fn max_level() -> Level {
    Level::MAX
}

/// `[security]` section.
#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityConfig {
    #[serde(default = "max_level")]
    pub internal: Level,
    #[serde(default = "max_level")]
    pub external: Level,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            internal: Level::MAX,
            external: Level::MAX,
        }
    }
}

/// Requirements not met by a configuration (empty = fine).
#[derive(Debug, Default)]
pub struct Violations(pub Vec<String>);

impl Violations {
    pub fn require(&mut self, condition: bool, message: impl Into<String>) {
        if !condition {
            self.0.push(message.into());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn extend(&mut self, other: Violations) {
        self.0.extend(other.0);
    }

    /// `Err` listing every violation, for startup.
    pub fn into_result(self, scope: &str, level: Level) -> Result<(), String> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "configuration too weak for {scope} security level {level}: {}",
                self.0.join("; ")
            ))
        }
    }
}

fn scheme(url: &str) -> String {
    url.split_once("://")
        .map(|(s, _)| s.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Internal links (D19): RabbitMQ (AMQP and, if `uses_stream`, the stream
/// port) and CouchDB (if used).
pub fn check_internal(
    level: Level,
    rabbitmq: Option<&RabbitMqConfig>,
    couchdb: Option<&CouchDbConfig>,
) -> Violations {
    let mut v = Violations::default();
    if let Some(r) = rabbitmq {
        if level >= Level::Authenticated {
            v.require(
                r.username.is_some() && r.password.is_some(),
                "rabbitmq.username and rabbitmq.password are required",
            );
            v.require(
                r.username.as_deref() != Some("guest"),
                "the RabbitMQ `guest` account is not allowed",
            );
        }
        if level >= Level::Encrypted {
            v.require(
                scheme(&r.url) == "amqps",
                "rabbitmq.url must be amqps:// (TLS; the stream port then uses TLS too)",
            );
            v.require(
                r.ca_file.is_some(),
                "rabbitmq.ca_file is required (explicit trust anchor; without it the stream client would not verify the broker)",
            );
        }
        if level >= Level::Hardened {
            v.require(
                r.cert_file.is_some() && r.key.is_some(),
                "rabbitmq.cert_file and rabbitmq.key are required (mutual TLS)",
            );
        }
    }
    if let Some(c) = couchdb {
        if level >= Level::Authenticated {
            v.require(
                c.username.is_some() && c.password.is_some(),
                "couchdb.username and couchdb.password are required",
            );
        }
        if level >= Level::Encrypted {
            v.require(scheme(&c.url) == "https", "couchdb.url must be https://");
            v.require(
                c.ca_file.is_some(),
                "couchdb.ca_file is required (explicit trust anchor)",
            );
        }
        if level >= Level::Hardened {
            v.require(
                c.cert_file.is_some() && c.key.is_some(),
                "couchdb.cert_file and couchdb.key are required (mutual TLS)",
            );
        }
    }
    v
}

/// Can a runner at `level` connect to this host (D19, external scope of
/// runners)? `Err` gives the reason, recorded as the task's outcome.
///
/// - 0, 1: any connection method;
/// - 2: the host's identity must be verified: SSHOQ without `Insecure` TLS;
///   SSH needs a `HostKey` (known_hosts file or SHA-256 fingerprints);
/// - 3: + no password authentication (keys, tokens).
pub fn check_host(level: Level, builder: &ManagedHostBuilder) -> Result<(), String> {
    let refuse = |why: &str| {
        Err(format!(
            "security level {level} forbids this host connection: {why}"
        ))
    };
    match &builder.host_connection_method {
        // no network involved
        None | Some(ConnectionMethod::Localhost(_)) => Ok(()),
        Some(ConnectionMethod::Ssh2(auth)) => {
            if level >= Level::Encrypted && auth.host_key.is_none() {
                return refuse(
                    "the SSH host key is not verified (set HostKey: a known_hosts file or SHA-256 fingerprints)",
                );
            }
            if level >= Level::Hardened
                && matches!(auth.auth_method, Ssh2AuthReference::UsernamePassword(_))
            {
                return refuse("password authentication (use a key)");
            }
            Ok(())
        }
        Some(ConnectionMethod::Sshoq(auth)) => {
            if level >= Level::Encrypted
                && matches!(auth.tls_config, Some(SshoqTlsConfig::Insecure))
            {
                return refuse("SSHOQ TLS verification is disabled (Insecure)");
            }
            if level >= Level::Hardened
                && matches!(auth.auth_method, SshoqAuthReference::UsernamePassword(_))
            {
                return refuse("password authentication (use a key or a bearer token)");
            }
            Ok(())
        }
    }
}

/// Can sourcesync at `level` fetch this Git URL (D19)?
///
/// - 0, 1: any URL;
/// - 2, 3: `https://` (TLS, server verified) or a local path / `file://`.
pub fn check_git_url(level: Level, url: &str) -> Result<(), String> {
    let s = scheme(url);
    if level >= Level::Encrypted && !(s == "https" || s == "file" || s.is_empty()) {
        return Err(format!(
            "security level {level} requires https:// Git sources, not {url}"
        ));
    }
    Ok(())
}

/// Startup gate (D19): log the levels, and exit (status 2) listing every
/// violation if the configuration is weaker than them.
pub fn enforce_at_startup(
    config: &SecurityConfig,
    checks: impl IntoIterator<Item = Result<(), String>>,
) {
    let failures: Vec<String> = checks.into_iter().filter_map(Result::err).collect();
    if failures.is_empty() {
        tracing::info!(
            security_internal = config.internal as u8,
            security_external = config.external as u8,
            "security levels: internal {}, external {}",
            config.internal,
            config.external
        );
        if config.internal == Level::Open || config.external == Level::Open {
            tracing::warn!("security level 0 (open) in use: for trials and trusted networks only");
        }
        return;
    }
    for failure in &failures {
        tracing::error!(error = %failure, "refusing to start");
    }
    eprintln!("refusing to start:\n  {}", failures.join("\n  "));
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use regent_sdk::hosts::handlers::ssh2::{Ssh2Auth, Ssh2HostKey};
    use regent_sdk::hosts::handlers::sshoq::SshoqAuth;
    use regent_sdk::secrets::SecretReference;

    #[test]
    fn levels_parse_from_numbers_and_names() {
        #[derive(Deserialize)]
        struct S {
            security: SecurityConfig,
        }
        let s: S = toml::from_str("[security]\ninternal = 0\nexternal = \"encrypted\"\n").unwrap();
        assert_eq!(s.security.internal, Level::Open);
        assert_eq!(s.security.external, Level::Encrypted);
        assert!(toml::from_str::<S>("[security]\ninternal = 7\n").is_err());
        assert_eq!(
            SecurityConfig::default().internal,
            Level::Hardened,
            "secure by default"
        );
    }

    #[test]
    fn internal_requirements_grow_with_the_level() {
        let plain = RabbitMqConfig::default();
        assert!(check_internal(Level::Open, Some(&plain), None).is_empty());
        assert_eq!(
            check_internal(Level::Authenticated, Some(&plain), None)
                .0
                .len(),
            1
        );
        let reference = SecretReference::from("/run/secrets/x", None);
        let authenticated = RabbitMqConfig {
            username: Some("regency-runner".into()),
            password: Some(reference.clone()),
            ..RabbitMqConfig::default()
        };
        assert!(check_internal(Level::Authenticated, Some(&authenticated), None).is_empty());
        assert_eq!(
            check_internal(Level::Encrypted, Some(&authenticated), None)
                .0
                .len(),
            2
        );
        let tls = RabbitMqConfig {
            url: "amqps://rabbitmq:5671".into(),
            ca_file: Some("/etc/regency/ca.pem".into()),
            ..authenticated.clone()
        };
        assert!(check_internal(Level::Encrypted, Some(&tls), None).is_empty());
        assert_eq!(check_internal(Level::Hardened, Some(&tls), None).0.len(), 1);
        let couch = CouchDbConfig::default();
        let v = check_internal(Level::Encrypted, None, Some(&couch));
        assert_eq!(v.0.len(), 3, "{:?}", v.0);
        assert!(
            v.into_result("internal", Level::Encrypted)
                .unwrap_err()
                .contains("https")
        );
    }

    #[test]
    fn host_connections_by_level() {
        let password = SecretReference::from("creds", None);
        let ssh = ManagedHostBuilder::new(
            "h",
            "h:22",
            Some(ConnectionMethod::Ssh2(Ssh2Auth::username_password(
                password.clone(),
            ))),
        );
        assert!(check_host(Level::Authenticated, &ssh).is_ok());
        assert!(
            check_host(Level::Encrypted, &ssh)
                .unwrap_err()
                .contains("host key")
        );
        let pinned = |auth: Ssh2Auth| {
            let auth = auth.with_host_key(Ssh2HostKey::Fingerprints(vec!["SHA256:x".into()]));
            ManagedHostBuilder::new("h", "h:22", Some(ConnectionMethod::Ssh2(auth)))
        };
        let pinned_password = pinned(Ssh2Auth::username_password(password.clone()));
        assert!(check_host(Level::Encrypted, &pinned_password).is_ok());
        assert!(
            check_host(Level::Hardened, &pinned_password)
                .unwrap_err()
                .contains("password")
        );
        let pinned_key = pinned(Ssh2Auth::key("ops", password.clone()));
        assert!(check_host(Level::Hardened, &pinned_key).is_ok());

        let insecure = SshoqAuth::from(
            SshoqAuthReference::UsernamePassword(password.clone()),
            None,
            Some(SshoqTlsConfig::Insecure),
        );
        let insecure =
            ManagedHostBuilder::new("h", "h:443", Some(ConnectionMethod::Sshoq(insecure)));
        assert!(check_host(Level::Authenticated, &insecure).is_ok());
        assert!(
            check_host(Level::Encrypted, &insecure)
                .unwrap_err()
                .contains("Insecure")
        );

        let verified = SshoqAuth::from(
            SshoqAuthReference::UsernamePassword(password.clone()),
            None,
            None,
        );
        let verified =
            ManagedHostBuilder::new("h", "h:443", Some(ConnectionMethod::Sshoq(verified)));
        assert!(check_host(Level::Encrypted, &verified).is_ok());
        assert!(
            check_host(Level::Hardened, &verified)
                .unwrap_err()
                .contains("password")
        );

        let token = SshoqAuth::from(SshoqAuthReference::BearerToken(password), None, None);
        let token = ManagedHostBuilder::new("h", "h:443", Some(ConnectionMethod::Sshoq(token)));
        assert!(check_host(Level::Hardened, &token).is_ok());

        let local = ManagedHostBuilder::new("h", "localhost", None);
        assert!(check_host(Level::Hardened, &local).is_ok());
    }

    #[test]
    fn git_urls_by_level() {
        assert!(check_git_url(Level::Open, "http://git.local/repo").is_ok());
        assert!(check_git_url(Level::Encrypted, "http://git.local/repo").is_err());
        assert!(check_git_url(Level::Encrypted, "https://github.com/x/y").is_ok());
        assert!(check_git_url(Level::Hardened, "file:///srv/repo").is_ok());
    }
}
