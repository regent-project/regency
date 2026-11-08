//! End-to-end: the runner binary executes localhost tasks from RabbitMQ.
//! `cargo test -p runner -- --ignored` (docker; or REGENCY_TEST_EXTERNAL=1).

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use lapin::options::{BasicAckOptions, BasicConsumeOptions, QueueBindOptions, QueueDeclareOptions};
use lapin::types::FieldTable;
use regencylib::amqp::Publisher;
use regencylib::control::{AbortPublisher, environment};
use regencylib::hashing::bucket;
use regencylib::messages::*;
use regencylib::testing::{infra, unique};
use regencylib::topology::{self, EX_LIFECYCLE, EX_OUTCOMES, EX_TASKS, TopologyConfig};
use regent_sdk::ExpectedState;
use regent_sdk::hosts::handlers::{ConnectionMethod, TargetUser};
use regent_sdk::hosts::managed_host::ManagedHostBuilder;
use regent_sdk::task::{Job, RegentTask};

const PARTITIONS: u32 = 4;

fn task(host: &str, job: Job, key: &str, user: TargetUser) -> TaskEnvelope {
    let builder =
        ManagedHostBuilder::new(host, "localhost", Some(ConnectionMethod::Localhost(user)));
    let task = RegentTask::from(builder, ExpectedState::new(), job).with_idempotency_key(key);
    TaskEnvelope::new(task, chrono::Utc::now(), "c-test")
}

// multi-thread: rabbitmq-stream-client stalls on a current-thread runtime
fn ssh_task(key: &str) -> TaskEnvelope {
    use regent_sdk::hosts::handlers::ssh2::Ssh2Auth;
    let method = ConnectionMethod::Ssh2(Ssh2Auth::key(
        "ops",
        regent_sdk::secrets::SecretReference::from("id_ed25519", None),
    ));
    let builder = ManagedHostBuilder::new("ssh-host", "127.0.0.1:1", Some(method));
    let task =
        RegentTask::from(builder, ExpectedState::new(), Job::Assess).with_idempotency_key(key);
    TaskEnvelope::new(task, chrono::Utc::now(), "c-test")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker"]
async fn runner_executes_and_honours_aborts() {
    let infra = infra().await;
    let pool = unique("p");
    let dir = std::env::temp_dir().join(unique("runner-test-"));
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
stream_port = {stream_port}
username = "regency"
password = {{ SecRef = "{rp}" }}
[couchdb]
url = "{couch}"
username = "{cu}"
password = {{ SecRef = "{cp}" }}
[health]
listen = "127.0.0.1:{health_port}"
[log]
format = "text"
[security]
internal = 1
external = 2
[runner]
pool = "{pool}"
lock_retry_sec = 1
[topology]
reach_partitions = {PARTITIONS}
"#,
        amqp = infra.rabbitmq.url,
        stream_port = infra.rabbitmq.stream_port,
        rp = infra.rabbitmq.password.as_ref().unwrap().sec_ref(),
        couch = infra.couchdb.url,
        cu = infra.couchdb.username.as_deref().unwrap(),
        cp = infra.couchdb.password.as_ref().unwrap().sec_ref(),
    );
    let config_path = dir.join("runner.toml");
    std::fs::write(&config_path, config).unwrap();

    let connection = regencylib::amqp::connect(&infra.rabbitmq, &infra.secrets, "test")
        .await
        .unwrap();
    let channel = connection.create_channel().await.unwrap();
    let topology_config = TopologyConfig {
        reach_partitions: PARTITIONS,
        ..Default::default()
    };
    topology::declare_exchanges(&channel).await.unwrap();
    topology::declare_pool(&channel, &topology_config, &pool)
        .await
        .unwrap();
    topology::declare_control_stream(&channel, &topology_config)
        .await
        .unwrap();
    let mut streams = Vec::new();
    for exchange in [EX_OUTCOMES, EX_LIFECYCLE] {
        let queue = unique("test.runner.");
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
                exchange.into(),
                "#".into(),
                QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await
            .unwrap();
        streams.push(
            channel
                .basic_consume(
                    queue.as_str().into(),
                    "".into(),
                    BasicConsumeOptions::default(),
                    FieldTable::default(),
                )
                .await
                .unwrap(),
        );
    }
    let mut lifecycle = streams.pop().unwrap();
    let mut outcomes = streams.pop().unwrap();

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_regency-runner"))
        .args(["--config", config_path.to_str().unwrap()])
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // wait until ready (control stream followed, partitions owned)
    let mut ready = false;
    for _ in 0..100 {
        if let Ok(r) = reqwest::get(format!("http://127.0.0.1:{health_port}/readyz")).await
            && r.status().is_success()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(ready, "runner not ready");
    tokio::time::sleep(Duration::from_secs(2)).await; // partitions acquired

    // an abort before the task is queued
    let aborted_key = unique("run:c:");
    let environment = environment(&infra.rabbitmq, &infra.secrets).await.unwrap();
    let abort_publisher =
        tokio::time::timeout(Duration::from_secs(10), AbortPublisher::new(&environment))
            .await
            .expect("producer timeout")
            .unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        abort_publisher.publish(&AbortRequest::new(&aborted_key, "User::\"admin\"")),
    )
    .await
    .expect("abort publish timeout")
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let publisher = Publisher::new(&connection).await.unwrap();
    let assess_key = unique("run:c:");
    let reach_key = unique("run:c:");
    let raw_key = unique("run:c:");
    let ssh_key = unique("run:c:");
    let other_sdk_key = unique("run:c:");
    let host = unique("h");
    let reach_route = topology::reach_key(&pool, PARTITIONS, bucket(&host, PARTITIONS));
    let assess_route = topology::assess_key(&pool);
    for (envelope, route) in [
        (
            task(&host, Job::Assess, &assess_key, TargetUser::current_user()),
            &assess_route,
        ),
        (
            task(&host, Job::Reach, &reach_key, TargetUser::current_user()),
            &reach_route,
        ),
        (
            task(&host, Job::Assess, &aborted_key, TargetUser::current_user()),
            &assess_route,
        ),
        (
            task(&host, Job::Assess, &raw_key, TargetUser::user_raw("u", "p")),
            &assess_route,
        ),
        (ssh_task(&ssh_key), &assess_route),
    ] {
        publisher
            .publish_json(EX_TASKS, route, &envelope.to_bytes().unwrap())
            .await
            .unwrap();
    }
    // D20: a task serialized by another regent-sdk is refused, not misread
    let mut other_sdk = serde_json::to_value(task(
        &host,
        Job::Assess,
        &other_sdk_key,
        TargetUser::current_user(),
    ))
    .unwrap();
    other_sdk["sdk_version"] = "0.0.1".into();
    other_sdk["task"] = serde_json::json!({"layout": "unknown"});
    publisher
        .publish_json(
            EX_TASKS,
            &assess_route,
            &serde_json::to_vec(&other_sdk).unwrap(),
        )
        .await
        .unwrap();

    let mut results = HashMap::new();
    while results.len() < 5 {
        let delivery = tokio::time::timeout(Duration::from_secs(30), outcomes.next())
            .await
            .expect("outcome")
            .unwrap()
            .unwrap();
        delivery.ack(BasicAckOptions::default()).await.unwrap();
        let decoded = decode_outcome(&delivery.data).unwrap();
        assert_eq!(decoded.origin, "c-test");
        results.insert(decoded.result.idempotency_key().to_string(), decoded);
    }
    assert!(
        results[&assess_key]
            .result
            .host_status()
            .unwrap()
            .is_already_compliant()
    );
    assert!(
        results[&reach_key]
            .result
            .host_status()
            .unwrap()
            .is_already_compliant()
    );
    assert!(
        results[&raw_key]
            .result
            .error()
            .unwrap()
            .to_string()
            .contains("D9")
    );
    assert!(!results.contains_key(&aborted_key));
    // D19: external level 2 refuses SSH without a HostKey (unverified server)
    let refusal = results[&ssh_key].result.error().unwrap().to_string();
    assert!(refusal.contains("security level"), "{refusal}");
    let refusal = results[&other_sdk_key].result.error().unwrap().to_string();
    assert!(refusal.contains("regent-sdk 0.0.1"), "{refusal}");

    let mut states: HashMap<(String, JobState), ()> = HashMap::new();
    while !(states.contains_key(&(aborted_key.clone(), JobState::Aborted))
        && states.contains_key(&(assess_key.clone(), JobState::Started)))
    {
        let delivery = tokio::time::timeout(Duration::from_secs(10), lifecycle.next())
            .await
            .expect("lifecycle")
            .unwrap()
            .unwrap();
        delivery.ack(BasicAckOptions::default()).await.unwrap();
        let event = LifecycleEvent::from_bytes(&delivery.data).unwrap();
        states.insert((event.job_id, event.state), ());
    }

    child.kill().await.unwrap();
    for partition in 0..PARTITIONS {
        let _ = channel
            .queue_delete(
                topology::reach_queue(&pool, PARTITIONS, partition)
                    .as_str()
                    .into(),
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
}
