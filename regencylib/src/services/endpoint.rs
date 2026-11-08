//! `/etc/regency/endpoint.toml`.

use crate::config::CommonConfig;
use regent_sdk::secrets::SecretReference;
use serde::{Deserialize, Serialize};

use crate::security::{Level, Violations};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(flatten)]
    pub common: CommonConfig,
    pub endpoint: EndpointSection,
    /// Built-in CA mode (D13); absent = disabled.
    #[serde(default)]
    pub ca: Option<CaSection>,
    #[serde(default)]
    pub pinning: PinningSection,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointSection {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Server certificate chain, PEM file.
    pub cert: String,
    /// Server private key (PEM), as a secret reference.
    pub key: SecretReference,
    /// External CA mode (D13): CA bundle (PEM) trusted for client certs.
    #[serde(default)]
    pub external_ca_bundle: Option<String>,
    #[serde(default = "default_handshake")]
    pub handshake_timeout_sec: u64,
    /// How long ReportOutcome waits for RabbitMQ before answering UNAVAILABLE.
    #[serde(default = "default_publish_wait")]
    pub publish_wait_sec: u64,
}

fn default_listen() -> String {
    "0.0.0.0:8443".to_string()
}
fn default_handshake() -> u64 {
    10
}
fn default_publish_wait() -> u64 {
    5
}

/// Built-in CA + enrollment (D13).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaSection {
    /// CA certificate, PEM file.
    pub cert: String,
    /// CA private key (PEM, PKCS#8), shared by every endpoint replica.
    pub key: SecretReference,
    /// HMAC key of enrollment tokens, shared with backend (which issues them).
    pub enrollment_key: SecretReference,
    /// Lifetime of issued client certificates (short, D13).
    #[serde(default = "default_lifetime")]
    pub cert_lifetime_days: u32,
}

fn default_lifetime() -> u32 {
    30
}

/// Self-signed certificates with pinning (D13).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PinningSection {
    #[serde(default)]
    pub enabled: bool,
    /// Pin the first fingerprint seen without admin approval.
    #[serde(default)]
    pub trust_on_first_use: bool,
}

/// D19 checks of the external level.
pub fn external_checks(config: &Config) -> Vec<Result<(), String>> {
    vec![check_external(config)]
}

fn check_external(config: &Config) -> Result<(), String> {
    let level = config.common.security.external;
    let mut v = Violations::default();
    let ca_mode = config.endpoint.external_ca_bundle.is_some() || config.ca.is_some();
    if level >= Level::Authenticated {
        v.require(
            ca_mode || config.pinning.enabled,
            "a client trust mode is required: endpoint.external_ca_bundle, [ca] or pinning.enabled",
        );
    }
    if level >= Level::Encrypted {
        v.require(
            !config.pinning.trust_on_first_use,
            "pinning.trust_on_first_use is forbidden (an admin approves fingerprints)",
        );
    }
    if level >= Level::Hardened {
        v.require(
            !config.pinning.enabled,
            "self-signed certificates (pinning) are forbidden: use an external or the built-in CA",
        );
        v.require(
            ca_mode,
            "an external CA bundle or the built-in CA is required",
        );
        v.require(
            config
                .ca
                .as_ref()
                .is_none_or(|ca| ca.cert_lifetime_days <= 30),
            "ca.cert_lifetime_days must be <= 30",
        );
    }
    v.into_result("external", level)
}
