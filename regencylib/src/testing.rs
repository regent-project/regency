//! Test infrastructure (feature `testing`, D18): RabbitMQ (with streams) and
//! CouchDB in containers started by `testcontainers` (needs docker).
//!
//! With `REGENCY_TEST_EXTERNAL=1`, tests use the dev compose instances
//! instead (`dev/compose.yaml`: localhost, regency/regency, admin/admin); test
//! names are randomized so runs do not collide.

use std::path::PathBuf;

use regent_sdk::secrets::{SecretProvidersPool, SecretReference};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

use crate::config::{CouchDbConfig, RabbitMqConfig, SecretsConfig};

/// Running infrastructure; containers stop when dropped.
pub struct Infra {
    pub rabbitmq: RabbitMqConfig,
    pub couchdb: CouchDbConfig,
    pub secrets: SecretProvidersPool,
    _containers: Vec<ContainerAsync<GenericImage>>,
}

/// Random suffix for names in a shared broker/database.
pub fn unique(prefix: &str) -> String {
    format!("{prefix}{:08x}", rand::random::<u32>())
}

fn secret_file(value: &str) -> SecretReference {
    let path: PathBuf = std::env::temp_dir().join(unique("regency-test-secret-"));
    std::fs::write(&path, value).expect("temp secret");
    SecretReference::from(
        path.to_str().expect("utf-8 temp path"),
        Some("files".to_string()),
    )
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("free port")
}

pub async fn infra() -> Infra {
    let secrets = SecretsConfig::default().pool().expect("secrets pool");
    if std::env::var("REGENCY_TEST_EXTERNAL").is_ok_and(|v| v == "1") {
        return Infra {
            rabbitmq: RabbitMqConfig {
                username: Some("regency".into()),
                password: Some(secret_file("regency")),
                ..RabbitMqConfig::default()
            },
            couchdb: CouchDbConfig {
                url: "http://localhost:5984".into(),
                username: Some("admin".into()),
                password: Some(secret_file("admin")),
                ..CouchDbConfig::default()
            },
            secrets,
            _containers: Vec::new(),
        };
    }

    // the stream protocol advertises host:port to clients: map a free host
    // port to 5552 and advertise it
    let stream_port = free_port();
    let amqp_port = free_port();
    let rabbit = GenericImage::new("rabbitmq", "4.1-management")
        .with_exposed_port(5672.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Server startup complete"))
        .with_entrypoint("sh")
        .with_cmd([
            "-c",
            "echo '[rabbitmq_management,rabbitmq_stream].' > /etc/rabbitmq/enabled_plugins && exec docker-entrypoint.sh rabbitmq-server",
        ])
        .with_env_var("RABBITMQ_DEFAULT_USER", "regency")
        .with_env_var("RABBITMQ_DEFAULT_PASS", "regency")
        .with_env_var(
            "RABBITMQ_SERVER_ADDITIONAL_ERL_ARGS",
            format!("-rabbitmq_stream advertised_host \"localhost\" -rabbitmq_stream advertised_port {stream_port}"),
        )
        .with_mapped_port(stream_port, 5552.tcp())
        .with_mapped_port(amqp_port, 5672.tcp())
        .start()
        .await
        .expect("RabbitMQ container");
    let couch = GenericImage::new("couchdb", "3.5")
        .with_exposed_port(5984.tcp())
        .with_wait_for(WaitFor::message_on_either_std("Apache CouchDB has started"))
        .with_env_var("COUCHDB_USER", "admin")
        .with_env_var("COUCHDB_PASSWORD", "admin")
        .start()
        .await
        .expect("CouchDB container");
    let couch_port = couch.get_host_port_ipv4(5984).await.expect("couch port");
    let couchdb = CouchDbConfig {
        url: format!("http://localhost:{couch_port}"),
        username: Some("admin".into()),
        password: Some(secret_file("admin")),
        ..CouchDbConfig::default()
    };
    // single node: the system databases are not created automatically
    let client = crate::couch::Couch::new(&couchdb.url, Some(("admin".into(), "admin".into())));
    for db in ["_users", "_replicator"] {
        for _ in 0..30 {
            if client.create_db(db).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
    Infra {
        rabbitmq: RabbitMqConfig {
            url: format!("amqp://localhost:{amqp_port}"),
            stream_port,
            username: Some("regency".into()),
            password: Some(secret_file("regency")),
            ..RabbitMqConfig::default()
        },
        couchdb,
        secrets,
        _containers: vec![rabbit, couch],
    }
}
