//! `/etc/regency/dbwriter.toml`.

use crate::config::CommonConfig;
use crate::topology::TopologyConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(flatten)]
    pub common: CommonConfig,
    #[serde(default)]
    pub dbwriter: DbWriterSection,
    #[serde(default)]
    pub topology: TopologyConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DbWriterSection {
    /// Messages in flight per queue.
    #[serde(default = "default_prefetch")]
    pub prefetch: u16,
    /// Retention cleanup period (D8: daily), with jitter.
    #[serde(default = "default_cleanup_hours")]
    pub cleanup_interval_hours: u64,
    /// Delay before the first cleanup after start.
    #[serde(default = "default_first_cleanup")]
    pub first_cleanup_delay_sec: u64,
}

fn default_prefetch() -> u16 {
    32
}
fn default_cleanup_hours() -> u64 {
    24
}
fn default_first_cleanup() -> u64 {
    60
}

impl Default for DbWriterSection {
    fn default() -> Self {
        Self {
            prefetch: default_prefetch(),
            cleanup_interval_hours: default_cleanup_hours(),
            first_cleanup_delay_sec: default_first_cleanup(),
        }
    }
}

/// D19 checks of the external level (dbwriter has no external surface).
pub fn external_checks(_config: &Config) -> Vec<Result<(), String>> {
    Vec::new()
}
