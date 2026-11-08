//! End-to-end D21: central configuration and instance status, with dbwriter
//! as both the writer of `regency_config` and the reconfigured service.
//! `cargo test -p dbwriter -- --ignored` (docker; or REGENCY_TEST_EXTERNAL=1).

use std::path::Path;
use std::time::Duration;

use regencylib::amqp::Publisher;
use regencylib::central::{InstanceDoc, Origin, RESTART_EXIT_CODE, instance_doc_id};
use regencylib::couch::Couch;
use regencylib::messages::{Command, CommandBody};
use regencylib::model::DB_STATE;
use regencylib::testing::{infra, unique};
use regencylib::topology::{self, EX_COMMANDS};
use serde_json::json;

async fn eventually<T, F, Fut>(what: &str, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    for _ in 0..150 {
        if let Some(value) = probe().await {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for {what}");
}

fn spawn(config: &Path) -> tokio::process::Child {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_regency-dbwriter"))
        .args(["--config", config.to_str().unwrap()])
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn config_set(publisher: &Publisher, settings: serde_json::Value) {
    let command = Command::new(
        unique("cfg-"),
        "User::\"admin\"",
        CommandBody::ConfigSet {
            service: "dbwriter".into(),
            pool: None,
            settings: settings.as_object().unwrap().clone(),
        },
    );
    publisher
        .publish_json(
            EX_COMMANDS,
            command.routing_key(),
            &command.to_bytes().unwrap(),
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker"]
async fn central_configuration_restarts_the_service() {
    let infra = infra().await;
    let couch = Couch::from_config(&infra.couchdb, &infra.secrets)
        .await
        .unwrap();
    for db in regencylib::databases::STATIC {
        regencylib::databases::ensure(&couch, db).await.unwrap();
    }
    let dir = std::env::temp_dir().join(unique("central-test-"));
    std::fs::create_dir_all(&dir).unwrap();
    let health_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let instance = unique("dbw-");
    let config = format!(
        r#"
instance = "{instance}"
[rabbitmq]
url = "{amqp}"
username = "regency"
password = {{ SecRef = "{rp}" }}
[couchdb]
url = "{couch}"
username = "admin"
password = {{ SecRef = "{cp}" }}
[health]
listen = "127.0.0.1:{health_port}"
[log]
format = "text"
[security]
internal = 1
external = 1
[central]
restart_jitter_sec = 0
status_interval_sec = 5
"#,
        amqp = infra.rabbitmq.url,
        couch = infra.couchdb.url,
        rp = infra.rabbitmq.password.as_ref().unwrap().sec_ref(),
        cp = infra.couchdb.password.as_ref().unwrap().sec_ref(),
    );
    let config_path = dir.join("dbwriter.toml");
    std::fs::write(&config_path, config).unwrap();
    let mut child = spawn(&config_path);

    let connection = regencylib::amqp::connect(&infra.rabbitmq, &infra.secrets, "test")
        .await
        .unwrap();
    let publisher = Publisher::new(&connection).await.unwrap();
    let doc_id = instance_doc_id("dbwriter", &instance);
    let status = |predicate: fn(&InstanceDoc) -> bool| {
        let (couch, doc_id) = (couch.clone(), doc_id.clone());
        async move {
            couch
                .get::<InstanceDoc>(DB_STATE, &doc_id)
                .await
                .ok()
                .flatten()
                .map(|s| s.doc)
                .filter(predicate)
        }
    };

    // the instance reports itself, with its own version (D20)
    let doc = eventually("instance status", || status(|d| d.status.ready)).await;
    assert_eq!(doc.status.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(doc.status.security_internal, 1);
    assert_eq!(
        doc.status.origins.get("security.internal"),
        Some(&Origin::File)
    );
    // dbwriter's command queue must exist before commands are published
    eventually("dbwriter queues", || async {
        let channel = connection.create_channel().await.ok()?;
        channel
            .queue_declare(
                topology::Q_COMMANDS_DBWRITER.into(),
                lapin::options::QueueDeclareOptions {
                    passive: true,
                    ..Default::default()
                },
                Default::default(),
            )
            .await
            .ok()
            .filter(|q| q.consumer_count() > 0)
    })
    .await;

    // an invalid central configuration is refused, the service keeps running
    config_set(&publisher, json!({ "dbwriter": { "nope": 1 } })).await;
    let doc = eventually("config error reported", || {
        status(|d| d.status.config_error.is_some())
    })
    .await;
    assert!(
        doc.status.config_error.as_deref().unwrap().contains("nope"),
        "{:?}",
        doc.status.config_error
    );
    assert!(child.try_wait().unwrap().is_none(), "kept running");

    // a valid one restarts it (exit status 3) ...
    config_set(&publisher, json!({ "dbwriter": { "prefetch": 7 } })).await;
    let exit = tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .expect("restart")
        .unwrap();
    assert_eq!(exit.code(), Some(RESTART_EXIT_CODE));

    // ... and it comes back with the central value
    let mut child = spawn(&config_path);
    let doc = eventually("new configuration applied", || {
        status(|d| d.status.config["dbwriter"]["prefetch"] == json!(7) && d.status.ready)
    })
    .await;
    assert_eq!(
        doc.status.origins.get("dbwriter.prefetch"),
        Some(&Origin::Central)
    );
    assert!(doc.status.config_error.is_none());

    // clean up (shared dev database): removing the document restarts it again
    config_set(&publisher, json!({})).await;
    let exit = tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .expect("restart after removal")
        .unwrap();
    assert_eq!(exit.code(), Some(RESTART_EXIT_CODE));
}
