//! `/etc/regency/scheduler.toml`.

use crate::config::CommonConfig;
use crate::topology::TopologyConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(flatten)]
    pub common: CommonConfig,
    #[serde(default)]
    pub scheduler: SchedulerSection,
    #[serde(default)]
    pub topology: TopologyConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerSection {
    /// Order shards (D3); part of the lock queue names.
    #[serde(default = "default_shards")]
    pub shards: u32,
    #[serde(default = "default_lock_retry")]
    pub lock_retry_sec: u64,
    /// The firing loop must tick within this delay, else every lock is
    /// dropped (D3 watchdog).
    #[serde(default = "default_watchdog")]
    pub watchdog_timeout_sec: u64,
    /// `run` commands expanded concurrently.
    #[serde(default = "default_prefetch")]
    pub command_prefetch: u16,
    /// Bootstrap admin name (always authorized, D12).
    #[serde(default = "default_admin")]
    pub bootstrap_admin: String,
}

fn default_shards() -> u32 {
    256
}
fn default_lock_retry() -> u64 {
    5
}
fn default_watchdog() -> u64 {
    30
}
fn default_prefetch() -> u16 {
    8
}
fn default_admin() -> String {
    "admin".to_string()
}

impl Default for SchedulerSection {
    fn default() -> Self {
        Self {
            shards: default_shards(),
            lock_retry_sec: default_lock_retry(),
            watchdog_timeout_sec: default_watchdog(),
            command_prefetch: default_prefetch(),
            bootstrap_admin: default_admin(),
        }
    }
}

/// D19 checks of the external level (scheduler has no external surface).
pub fn external_checks(_config: &Config) -> Vec<Result<(), String>> {
    Vec::new()
}
