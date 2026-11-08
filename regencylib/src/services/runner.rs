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

impl Default for RunnerSection {
    fn default() -> Self {
        Self {
            pool: default_pool(),
            assess_concurrency: default_concurrency(),
            reach_prefetch: default_reach_prefetch(),
            control_lookback_sec: default_lookback(),
            shutdown_grace_sec: default_grace(),
            lock_retry_sec: default_lock_retry(),
        }
    }
}

/// D19 checks of the external level: per task (`security::check_host`), none
/// at startup.
pub fn external_checks(_config: &Config) -> Vec<Result<(), String>> {
    Vec::new()
}
