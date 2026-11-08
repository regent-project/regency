//! End-to-end: the dbwriter binary between RabbitMQ and CouchDB.
//! `cargo test -p dbwriter -- --ignored` (docker; or REGENCY_TEST_EXTERNAL=1).

use std::time::Duration;

use chrono::Utc;
use lapin::BasicProperties;
use regencylib::amqp::Publisher;
use regencylib::couch::Couch;
use regencylib::messages::*;
use regencylib::model::*;
use regencylib::schedule::Schedule;
use regencylib::testing::{infra, unique};
use regencylib::topology::{
    self, EX_COMMANDS, EX_LIFECYCLE, EX_OUTCOMES, EX_TASKS, TopologyConfig,
};
use regent_sdk::ExpectedState;
use regent_sdk::hosts::managed_host::ManagedHostBuilder;
use regent_sdk::state::compliance::ManagedHostStatus;
use regent_sdk::task::{Job, RegentTask, RegentTaskResult};
use serde_json::Value;

async fn eventually<T, F, Fut>(what: &str, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    for _ in 0..100 {
        if let Some(value) = probe().await {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for {what}");
}

fn result(key: &str, host: &str, offset_ms: i64) -> RegentTaskResult {
    let now = Utc::now();
    RegentTaskResult::from(
        key.to_string(),
        host.to_string(),
        Job::Assess,
        now,
        now + chrono::Duration::milliseconds(offset_ms),
        Ok(ManagedHostStatus::already_compliant()),
    )
}

#[tokio::test]
#[ignore = "needs docker"]
async fn dbwriter_records_everything() {
    let infra = infra().await;
    let couch = Couch::from_config(&infra.couchdb, &infra.secrets)
        .await
        .unwrap();
    for db in regencylib::databases::STATIC {
        regencylib::databases::ensure(&couch, db).await.unwrap();
    }
    let dir = std::env::temp_dir().join(unique("dbwriter-test-"));
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
[dbwriter]
first_cleanup_delay_sec = 0
"#,
        amqp = infra.rabbitmq.url,
        couch = infra.couchdb.url,
        rp = infra.rabbitmq.password.as_ref().unwrap().sec_ref(),
        cp = infra.couchdb.password.as_ref().unwrap().sec_ref(),
    );
    let config_path = dir.join("dbwriter.toml");
    std::fs::write(&config_path, config).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_regency-dbwriter"))
        .args(["--config", config_path.to_str().unwrap()])
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let connection = regencylib::amqp::connect(&infra.rabbitmq, &infra.secrets, "test")
        .await
        .unwrap();
    let publisher = Publisher::new(&connection).await.unwrap();
    // wait for dbwriter's queues
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

    let host = unique("host-");
    let key = unique("run:c1:");
    let created_at = Utc::now();
    let month = month_of(created_at);

    // lifecycle, out of order: STARTED before PENDING
    for state in [JobState::Started, JobState::Pending] {
        let event = LifecycleEvent::new(&key, state, created_at, "c1", &host, Job::Assess);
        publisher
            .publish_json(
                EX_LIFECYCLE,
                &event.routing_key(),
                &event.to_bytes().unwrap(),
            )
            .await
            .unwrap();
    }
    let job: Stored<JobDoc> = eventually("job STARTED", || async {
        couch
            .get::<JobDoc>(&jobs_db(&month), &job_doc_id(&key))
            .await
            .ok()?
            .filter(|j| j.doc.states.len() == 2)
    })
    .await;
    assert_eq!(job.doc.state, JobState::Started);

    // outcome, then a duplicate with another finished_at: dbwriter handles
    // messages concurrently, so either may be recorded first; the other must
    // be dropped consistently (D4)
    let first = result(&key, &host, 100);
    let second = result(&key, &host, 5000);
    for r in [&first, &second] {
        let envelope = outcome_envelope(
            r.to_proto_bytes().unwrap(),
            vec![],
            created_at,
            "c1",
            "runner/test",
        );
        publisher
            .publish(
                EX_OUTCOMES,
                "assess",
                &encode_outcome(&envelope).unwrap(),
                BasicProperties::default(),
            )
            .await
            .unwrap();
    }
    let job: Stored<JobDoc> = eventually("job FINISHED", || async {
        couch
            .get::<JobDoc>(&jobs_db(&month), &job_doc_id(&key))
            .await
            .ok()?
            .filter(|j| j.doc.state == JobState::Finished)
    })
    .await;
    assert_eq!(job.doc.result.as_ref().unwrap().compliance, "compliant");
    let host_doc: Stored<HostDoc> = eventually("host doc", || async {
        couch
            .get::<HostDoc>(DB_STATE, &host_doc_id(&host))
            .await
            .ok()?
    })
    .await;
    assert_eq!(host_doc.doc.state.compliance, "compliant");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let outcome: Stored<OutcomeDoc> = couch
        .get(&outcomes_db(&month), &outcome_doc_id(&key))
        .await
        .unwrap()
        .unwrap();
    let recorded = [&first, &second]
        .into_iter()
        .find(|r| r.finished_at() == outcome.doc.finished_at)
        .expect("the stored outcome is one of the two");
    let attachment = couch
        .get_attachment(
            &outcomes_db(&month),
            &outcome_doc_id(&key),
            OUTCOME_ATTACHMENT,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(attachment, recorded.to_proto_bytes().unwrap());
    assert_eq!(
        job.doc.outcome.as_ref().unwrap().finished_at,
        outcome.doc.finished_at,
        "job follows the recorded outcome"
    );

    // an expired task becomes EXPIRED
    let pool = unique("testpool");
    let topology_config = TopologyConfig {
        reach_partitions: 1,
        ..Default::default()
    };
    let channel = connection.create_channel().await.unwrap();
    topology::declare_pool(&channel, &topology_config, &pool)
        .await
        .unwrap();
    let dead_key = unique("rec:o1:1-a:");
    let task = RegentTask::from(
        ManagedHostBuilder::new(&host, "x:22", None),
        ExpectedState::new(),
        Job::Assess,
    )
    .with_idempotency_key(dead_key.clone());
    let envelope = TaskEnvelope::new(task, created_at, "o1");
    publisher
        .publish(
            EX_TASKS,
            &topology::assess_key(&pool),
            &envelope.to_bytes().unwrap(),
            BasicProperties::default().with_expiration("1".into()),
        )
        .await
        .unwrap();
    let dead: Stored<JobDoc> = eventually("job EXPIRED", || async {
        couch
            .get::<JobDoc>(&jobs_db(&month), &job_doc_id(&dead_key))
            .await
            .ok()?
    })
    .await;
    assert_eq!(dead.doc.state, JobState::Expired);
    for partition in 0..1 {
        let _ = channel
            .queue_delete(
                topology::reach_queue(&pool, 1, partition).as_str().into(),
                Default::default(),
            )
            .await;
    }
    let _ = channel
        .queue_delete(
            topology::assess_queue(&pool).as_str().into(),
            Default::default(),
        )
        .await;

    // commands
    let order_id = unique("order");
    let order = Order {
        order_id: order_id.clone(),
        schedule: Schedule::Every { seconds: 300 },
        assignment: "git/web".into(),
        job: Job::Assess,
        pool: None,
        author: "User::\"admin\"".into(),
        author_groups: vec![],
        enabled: true,
    };
    let refused_source = unique("nosource");
    let commands = [
        Command::new(
            unique("c"),
            "User::\"admin\"",
            CommandBody::OrderUpsert(order.clone()),
        ),
        Command::new(
            unique("c"),
            "User::\"admin\"",
            CommandBody::SettingsSet {
                key: "retention_months".into(),
                value: Value::from(5),
            },
        ),
        Command::new(
            unique("c"),
            "User::\"admin\"",
            CommandBody::CatalogUpsert(CatalogItem {
                body: CatalogBody::ExpectedState(ExpectedStateDef {
                    source_id: refused_source.clone(),
                    name: "x".into(),
                    state: ExpectedState::new(),
                }),
                revision: "1".into(),
            }),
        ),
    ];
    for command in &commands {
        publisher
            .publish_json(
                EX_COMMANDS,
                command.routing_key(),
                &command.to_bytes().unwrap(),
            )
            .await
            .unwrap();
    }
    let stored: Stored<Order> = eventually("order", || async {
        couch
            .get::<Order>(DB_ORDERS, &order_doc_id(&order_id))
            .await
            .ok()?
    })
    .await;
    assert_eq!(stored.doc, order);
    let first_rev = stored.rev.clone();
    eventually("settings", || async {
        couch
            .get::<SettingsDoc>(DB_SETTINGS, SETTINGS_DOC_ID)
            .await
            .ok()?
            .filter(|s| s.doc.retention_months() == Some(5))
    })
    .await;
    // re-applying the same order keeps its revision (D3 keys stay stable)
    publisher
        .publish_json(
            EX_COMMANDS,
            commands[0].routing_key(),
            &commands[0].to_bytes().unwrap(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let again: Stored<Order> = couch
        .get(DB_ORDERS, &order_doc_id(&order_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.rev, first_rev);
    assert!(
        couch
            .get::<Value>(DB_CATALOG, &format!("expected_state:{refused_source}:x"))
            .await
            .unwrap()
            .is_none()
    );

    // host deletion
    let delete = Command::new(
        unique("c"),
        "User::\"admin\"",
        CommandBody::HostDelete {
            host_id: host.clone(),
        },
    );
    publisher
        .publish_json(
            EX_COMMANDS,
            delete.routing_key(),
            &delete.to_bytes().unwrap(),
        )
        .await
        .unwrap();
    eventually("host deleted", || async {
        let gone = couch
            .get::<Value>(DB_STATE, &host_doc_id(&host))
            .await
            .ok()?
            .is_none()
            && couch
                .get::<Value>(&jobs_db(&month), &job_doc_id(&key))
                .await
                .ok()?
                .is_none()
            && couch
                .get::<Value>(&outcomes_db(&month), &outcome_doc_id(&key))
                .await
                .ok()?
                .is_none();
        gone.then_some(())
    })
    .await;

    // restore the default retention for other test runs
    let reset = Command::new(
        unique("c"),
        "User::\"admin\"",
        CommandBody::SettingsSet {
            key: "retention_months".into(),
            value: Value::from(3),
        },
    );
    publisher
        .publish_json(EX_COMMANDS, reset.routing_key(), &reset.to_bytes().unwrap())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    child.kill().await.unwrap();
}
