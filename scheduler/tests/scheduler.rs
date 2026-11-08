//! End-to-end: the scheduler binary fires an order and expands a run command.
//! `cargo test -p scheduler -- --ignored` (docker; or REGENCY_TEST_EXTERNAL=1).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use futures::StreamExt;
use lapin::options::{BasicAckOptions, BasicConsumeOptions, QueueBindOptions, QueueDeclareOptions};
use lapin::types::FieldTable;
use regencylib::amqp::Publisher;
use regencylib::couch::Couch;
use regencylib::messages::*;
use regencylib::model::*;
use regencylib::schedule::Schedule;
use regencylib::selector::HostSelector;
use regencylib::testing::{infra, unique};
use regencylib::topology::{self, EX_COMMANDS, EX_TASKS};
use regent_sdk::ExpectedState;
use regent_sdk::hosts::handlers::{ConnectionMethod, TargetUser};
use regent_sdk::hosts::managed_host::ManagedHostBuilder;
use regent_sdk::task::Job;

async fn put<T: serde::Serialize>(couch: &Couch, db: &str, id: &str, doc: T) {
    couch.put(db, &Stored::new(id, doc)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker"]
async fn scheduler_fires_orders_and_runs() {
    let infra = infra().await;
    let couch = Couch::from_config(&infra.couchdb, &infra.secrets)
        .await
        .unwrap();
    for db in regencylib::databases::STATIC {
        regencylib::databases::ensure(&couch, db).await.unwrap();
    }
    let source = unique("src");
    let pool = unique("pool");
    let host = unique("h");
    let item = |body| CatalogItem {
        body,
        revision: "r1".into(),
    };
    let source_item = item(CatalogBody::Source(SourceDef {
        source_id: source.clone(),
        kind: SourceKind::Api,
        repo: None,
        branch: None,
        path: None,
        poll_interval_sec: 300,
        credentials: None,
    }));
    let inventory = item(CatalogBody::Inventory(InventoryDef {
        source_id: source.clone(),
        name: "web".into(),
        default_connection_method: Some(ConnectionMethod::Localhost(TargetUser::current_user())),
        global_vars: None,
        pool: Some(pool.clone()),
    }));
    let hostdef = item(CatalogBody::Hostdef(HostDef {
        source_id: source.clone(),
        host_id: host.clone(),
        builder: ManagedHostBuilder::new(&host, "localhost", None),
        inventories: BTreeSet::from(["web".to_string()]),
        labels: BTreeMap::new(),
        pool: None,
    }));
    let state = item(CatalogBody::ExpectedState(ExpectedStateDef {
        source_id: source.clone(),
        name: "base".into(),
        state: ExpectedState::new(),
    }));
    let assignment = item(CatalogBody::Assignment(AssignmentDef {
        source_id: source.clone(),
        name: "web".into(),
        selector: HostSelector {
            inventories: BTreeSet::from([format!("{source}/web")]),
            ..Default::default()
        },
        expected_state: format!("{source}/base"),
        priority: 0,
    }));
    for doc in [&source_item, &inventory, &hostdef, &state, &assignment] {
        put(&couch, DB_CATALOG, &doc.doc_id(), doc).await;
    }
    let order = |id: &str, author: &str| Order {
        order_id: id.to_string(),
        schedule: Schedule::Every { seconds: 2 },
        assignment: format!("{source}/web"),
        job: Job::Assess,
        pool: None,
        author: author.to_string(),
        author_groups: vec![],
        enabled: true,
    };
    let allowed = unique("o");
    let denied = unique("o");
    put(
        &couch,
        DB_ORDERS,
        &order_doc_id(&allowed),
        order(&allowed, "User::\"admin\""),
    )
    .await;
    put(
        &couch,
        DB_ORDERS,
        &order_doc_id(&denied),
        order(&denied, "User::\"nobody\""),
    )
    .await;

    // observe the pool's tasks
    let connection = regencylib::amqp::connect(&infra.rabbitmq, &infra.secrets, "test")
        .await
        .unwrap();
    let channel = connection.create_channel().await.unwrap();
    topology::declare_exchanges(&channel).await.unwrap();
    let queue = unique("test.scheduler.");
    channel
        .queue_declare(
            queue.as_str().into(),
            QueueDeclareOptions {
                exclusive: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .unwrap();
    channel
        .queue_bind(
            queue.as_str().into(),
            EX_TASKS.into(),
            format!("{pool}.#").as_str().into(),
            QueueBindOptions::default(),
            FieldTable::default(),
        )
        .await
        .unwrap();
    let mut tasks = channel
        .basic_consume(
            queue.as_str().into(),
            "".into(),
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .unwrap();

    let dir = std::env::temp_dir().join(unique("scheduler-test-"));
    std::fs::create_dir_all(&dir).unwrap();
    let health_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let config = format!(
        r#"
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
[scheduler]
shards = 4
lock_retry_sec = 1
[topology]
reach_partitions = 2
"#,
        amqp = infra.rabbitmq.url,
        couch = infra.couchdb.url,
        rp = infra.rabbitmq.password.as_ref().unwrap().sec_ref(),
        cp = infra.couchdb.password.as_ref().unwrap().sec_ref(),
    );
    let config_path = dir.join("scheduler.toml");
    std::fs::write(&config_path, config).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_regency-scheduler"))
        .args(["--config", config_path.to_str().unwrap()])
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    // the allowed order fires (with a TTL), the denied one never does
    let delivery = tokio::time::timeout(Duration::from_secs(20), tasks.next())
        .await
        .expect("a task")
        .unwrap()
        .unwrap();
    delivery.ack(BasicAckOptions::default()).await.unwrap();
    let envelope = TaskEnvelope::from_bytes(&delivery.data).unwrap();
    let key = envelope.task.idempotency_key().to_string();
    assert!(key.starts_with(&format!("rec:{allowed}:")), "{key}");
    assert!(key.ends_with(&format!(":{host}")));
    assert_eq!(envelope.header.origin, allowed);
    assert!(
        envelope.header.state_revision.is_some() && !envelope.header.one_shot,
        "effective state (D23)"
    );
    assert!(delivery.properties.expiration().is_some());
    assert!(
        envelope
            .task
            .managed_host_builder()
            .host_connection_method
            .is_some(),
        "inventory default applied"
    );

    // a run command
    let publisher = Publisher::new(&connection).await.unwrap();
    let command_id = unique("cmd");
    let command = Command::new(
        command_id.clone(),
        "User::\"admin\"",
        CommandBody::Run(RunCommand {
            selector: HostSelector::hosts([host.clone()]),
            one_shot: None,
            job: Job::Reach,
            pool: None,
        }),
    );
    publisher
        .publish_json(EX_COMMANDS, "run", &command.to_bytes().unwrap())
        .await
        .unwrap();
    let mut saw_run = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(delivery))) =
            tokio::time::timeout(Duration::from_secs(5), tasks.next()).await
        else {
            continue;
        };
        delivery.ack(BasicAckOptions::default()).await.unwrap();
        let envelope = TaskEnvelope::from_bytes(&delivery.data).unwrap();
        let key = envelope.task.idempotency_key();
        assert!(
            !key.starts_with(&format!("rec:{denied}:")),
            "denied order fired"
        );
        if key == run_key(&command_id, &host) {
            assert_eq!(envelope.task.job(), Job::Reach);
            assert!(
                delivery
                    .routing_key
                    .as_str()
                    .starts_with(&format!("{pool}.2."))
            );
            saw_run = true;
            break;
        }
    }
    assert!(saw_run, "run command not expanded");
    child.kill().await.unwrap();
    for q in [
        topology::assess_queue(&pool),
        topology::reach_queue(&pool, 2, 0),
        topology::reach_queue(&pool, 2, 1),
    ] {
        let _ = channel
            .queue_delete(q.as_str().into(), Default::default())
            .await;
    }
    for id in [order_doc_id(&allowed), order_doc_id(&denied)] {
        if let Some(stored) = couch
            .get::<serde_json::Value>(DB_ORDERS, &id)
            .await
            .unwrap()
        {
            couch
                .delete(DB_ORDERS, &id, stored.rev.as_deref().unwrap())
                .await
                .unwrap();
        }
    }
}
