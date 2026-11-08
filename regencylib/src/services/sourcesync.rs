//! `/etc/regency/sourcesync.toml`.

use crate::config::CommonConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(flatten)]
    pub common: CommonConfig,
    #[serde(default)]
    pub sourcesync: SourcesyncSection,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourcesyncSection {
    #[serde(default = "default_shards")]
    pub shards: u32,
    #[serde(default = "default_lock_retry")]
    pub lock_retry_sec: u64,
    /// Bare clones, one directory per source.
    #[serde(default = "default_workdir")]
    pub workdir: String,
    #[serde(default = "default_branch")]
    pub default_branch: String,
}

fn default_shards() -> u32 {
    64
}
fn default_lock_retry() -> u64 {
    5
}
fn default_workdir() -> String {
    crate::process::default_state_dir("sourcesync")
}
fn default_branch() -> String {
    "main".to_string()
}

impl Default for SourcesyncSection {
    fn default() -> Self {
        Self {
            shards: default_shards(),
            lock_retry_sec: default_lock_retry(),
            workdir: default_workdir(),
            default_branch: default_branch(),
        }
    }
}

/// D19 checks of the external level: per Git source
/// (`security::check_git_url`), none at startup.
pub fn external_checks(_config: &Config) -> Vec<Result<(), String>> {
    Vec::new()
}
