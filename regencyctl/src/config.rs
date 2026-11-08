//! `regencyctl` configuration: admin credentials (never given to services)
//! and the passwords of each service account.

use std::collections::BTreeMap;

use regencylib::config::{CouchDbConfig, LogConfig, RabbitMqConfig, SecretsConfig};
use regencylib::topology::TopologyConfig;
use regent_sdk::secrets::SecretReference;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// RabbitMQ admin connection (AMQP, for the topology).
    #[serde(default)]
    pub rabbitmq: RabbitMqConfig,
    /// RabbitMQ management API, e.g. `http://localhost:15672`.
    #[serde(default = "default_management")]
    pub rabbitmq_management_url: String,
    /// CouchDB admin connection.
    #[serde(default)]
    pub couchdb: CouchDbConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub topology: TopologyConfig,
    /// Runner pools whose task queues are declared up front.
    #[serde(default = "default_pools")]
    pub pools: Vec<String>,
    /// Service accounts: `[accounts.<service>]`.
    #[serde(default)]
    pub accounts: BTreeMap<String, Account>,
    #[serde(default)]
    pub bootstrap: Option<Bootstrap>,
    /// D19: `internal` applies to regencyctl's admin links (AMQP, management
    /// API, CouchDB); `external` is unused.
    #[serde(default)]
    pub security: regencylib::security::SecurityConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    /// RabbitMQ user (default `regency-<service>`).
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub rabbitmq_password: Option<SecretReference>,
    #[serde(default)]
    pub couchdb_password: Option<SecretReference>,
}

/// D12 bootstrap admin.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    #[serde(default = "default_admin")]
    pub name: String,
    pub password: SecretReference,
}

fn default_management() -> String {
    "http://localhost:15672".to_string()
}
fn default_pools() -> Vec<String> {
    vec!["default".to_string()]
}
fn default_admin() -> String {
    "admin".to_string()
}
