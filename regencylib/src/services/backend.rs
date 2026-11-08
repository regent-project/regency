//! `/etc/regency/backend.toml`.

use crate::config::CommonConfig;
use regent_sdk::secrets::SecretReference;
use serde::{Deserialize, Serialize};

use crate::security::{Level, Violations};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(flatten)]
    pub common: CommonConfig,
    pub backend: BackendSection,
    /// OIDC providers (D12).
    #[serde(default)]
    pub oidc: Vec<OidcProvider>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackendSection {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// TLS (PEM files); without them, plain HTTP/2 (behind a TLS ingress).
    #[serde(default)]
    pub tls_cert: Option<String>,
    #[serde(default)]
    pub tls_key: Option<SecretReference>,
    /// TLS is terminated by a proxy in front of backend (satisfies the TLS
    /// requirement of security level 2+, D19).
    #[serde(default)]
    pub tls_offloaded: bool,
    /// Signing key of session tokens, shared by every replica (>= 32 bytes).
    pub session_key: SecretReference,
    #[serde(default = "default_session_ttl")]
    pub session_ttl_sec: i64,
    /// Bootstrap admin (D12): always authorized, created at first start.
    #[serde(default = "default_admin")]
    pub bootstrap_admin: String,
    /// Its initial password; without it, no bootstrap admin is created.
    #[serde(default)]
    pub bootstrap_password: Option<SecretReference>,
    /// HMAC key of enrollment tokens, shared with the endpoint (D13).
    #[serde(default)]
    pub enrollment_key: Option<SecretReference>,
    /// Origins allowed to call gRPC-Web from a browser (`*` = any).
    #[serde(default = "default_origins")]
    pub cors_origins: Vec<String>,
    /// Directory of the web client's static files (`frontend/dist`), served
    /// on the same port as the API; absent = API only.
    #[serde(default)]
    pub frontend_dir: Option<String>,
    /// How long RPCs wait for RabbitMQ before answering UNAVAILABLE.
    #[serde(default = "default_publish_wait")]
    pub publish_wait_sec: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OidcProvider {
    pub issuer: String,
    /// Expected `aud`.
    pub audience: String,
    #[serde(default = "default_username_claim")]
    pub username_claim: String,
    #[serde(default = "default_groups_claim")]
    pub groups_claim: String,
}

fn default_listen() -> String {
    "0.0.0.0:8080".to_string()
}
fn default_session_ttl() -> i64 {
    12 * 3600
}
fn default_admin() -> String {
    "admin".to_string()
}
fn default_origins() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_publish_wait() -> u64 {
    5
}
fn default_username_claim() -> String {
    "preferred_username".to_string()
}
fn default_groups_claim() -> String {
    "groups".to_string()
}

/// D19 checks of the external level.
pub fn external_checks(config: &Config) -> Vec<Result<(), String>> {
    vec![check_external(config)]
}

fn check_external(config: &Config) -> Result<(), String> {
    let level = config.common.security.external;
    let section = &config.backend;
    let mut v = Violations::default();
    if level >= Level::Encrypted {
        v.require(
            (section.tls_cert.is_some() && section.tls_key.is_some()) || section.tls_offloaded,
            "TLS is required: backend.tls_cert + backend.tls_key, or backend.tls_offloaded = true behind a TLS proxy",
        );
        for provider in &config.oidc {
            v.require(
                provider.issuer.starts_with("https://"),
                format!("OIDC issuer {} must be https://", provider.issuer),
            );
        }
    }
    if level >= Level::Hardened {
        v.require(
            section.session_ttl_sec <= 3600,
            "backend.session_ttl_sec must be <= 3600",
        );
    }
    v.into_result("external", level)
}
