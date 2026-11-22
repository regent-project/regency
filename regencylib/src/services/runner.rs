//! `/etc/regency/runner.toml`.

use crate::config::CommonConfig;
use crate::topology::TopologyConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(flatten)]
    pub common: CommonConfig,
    #[serde(default)]
    pub runner: RunnerSection,
    #[serde(default)]
    pub topology: TopologyConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerSection {
    /// Runner pool served by this process (D4), e.g. a network zone.
    #[serde(default = "default_pool")]
    pub pool: String,
    /// Assessments run concurrently (prefetch of the assess queue).
    #[serde(default = "default_concurrency")]
    pub assess_concurrency: u16,
    /// Prefetch of each owned Reach partition (different hosts run
    /// concurrently; a second Reach for a busy host waits locally).
    #[serde(default = "default_reach_prefetch")]
    pub reach_prefetch: u16,
    /// Reaches run at once by this runner, all partitions together (each
    /// opens a connection to its host); the others wait their turn, still
    /// pending. Without it, a Reach of thousands of hosts would run up to
    /// `reach_prefetch` × owned partitions at once (1024 by default) and
    /// overwhelm SSH servers, bastions or the runner itself. 0 reads as 1.
    #[serde(default = "default_concurrency")]
    pub reach_concurrency: u16,
    /// D7 look-back on `regency.control`: longest task TTL + consumer_timeout.
    #[serde(default = "default_lookback")]
    pub control_lookback_sec: u64,
    /// On SIGTERM: time left to running tasks before they are stopped and
    /// handed back to the queue.
    #[serde(default = "default_grace")]
    pub shutdown_grace_sec: u64,
    /// Shard-ownership round period (D3 machinery for Reach partitions).
    #[serde(default = "default_lock_retry")]
    pub lock_retry_sec: u64,
    /// D24: publish what running tasks do on `regency.live` (live views). The
    /// run's log is kept with its outcome either way (`run_log_max_bytes`
    /// setting).
    #[serde(default = "default_live")]
    pub live: bool,
    /// D25: the runner's SSH identity, an OpenSSH private key file (public
    /// key in `<file>.pub`), generated (ed25519) at startup when missing. Its
    /// public key is advertised in the instance status; new SSH hosts
    /// reference the file (`files` secret provider). Unset = none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_key: Option<String>,
}

fn default_pool() -> String {
    "default".to_string()
}
fn default_concurrency() -> u16 {
    16
}
fn default_reach_prefetch() -> u16 {
    4
}
fn default_lookback() -> u64 {
    86_400
}
fn default_grace() -> u64 {
    60
}
fn default_lock_retry() -> u64 {
    5
}
fn default_live() -> bool {
    true
}

impl Default for RunnerSection {
    fn default() -> Self {
        Self {
            pool: default_pool(),
            assess_concurrency: default_concurrency(),
            reach_prefetch: default_reach_prefetch(),
            reach_concurrency: default_concurrency(),
            control_lookback_sec: default_lookback(),
            shutdown_grace_sec: default_grace(),
            lock_retry_sec: default_lock_retry(),
            live: default_live(),
            ssh_key: None,
        }
    }
}

/// D19 checks of the external level: per task (`security::check_host`), none
/// at startup.
pub fn external_checks(_config: &Config) -> Vec<Result<(), String>> {
    Vec::new()
}
