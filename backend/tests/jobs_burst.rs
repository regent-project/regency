//! End-to-end: a burst of job updates larger than backend's internal
//! channel, while the WatchJobs client does not read, must not lose any job
//! (each job ends in its final state on the client side). Job documents are
//! written directly, standing in for dbwriter.
//! `cargo test -p backend --test jobs_burst -- --ignored` (docker; or
//! REGENCY_TEST_EXTERNAL=1).

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use regencylib::api::regency_client::RegencyClient;
use regencylib::couch::Couch;
use regencylib::messages::JobState;
use regencylib::model::*;
use regencylib::proto as api;
use regencylib::testing::{infra, unique};
use regencylib::topology;
use regent_sdk::task::Job;
use serde_json::Value;
use tonic::transport::Channel;

/// More updates than backend's job channel holds (16384).
const JOBS: usize = 12_000;
const BULK: usize = 2_000;

fn job_doc(id: &str, origin: &str, state: JobState) -> JobDoc {
    let now = chrono::Utc::now();
    JobDoc {
        kind: "job".into(),
        job_id: id.into(),
        host_id: format!("host-{id}"),
        job: Job::Reach,
        origin: origin.into(),
        created_at: now,
        state,
        states: [(state, now)].into(),
        outcome: None,
        result: None,
        details: Default::default(),
    }
}

/// Write `docs` in bulk; returns each doc's new revision.
async fn write(couch: &Couch, db: &str, docs: Vec<Stored<JobDoc>>) -> HashMap<String, String> {
    let mut revs = HashMap::new();
    for chunk in docs.chunks(BULK) {
        let values: Vec<Value> = chunk
            .iter()
            .map(|d| serde_json::to_value(d).unwrap())
            .collect();
        for result in couch.bulk_docs(db, values).await.unwrap() {
            assert!(result.error.is_none(), "{result:?}");
            revs.insert(result.id.unwrap(), result.rev.unwrap());
        }
    }
    revs
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker"]
async fn job_burst_with_a_slow_client_loses_no_job() {
    let infra = infra().await;
    let couch = Couch::from_config(&infra.couchdb, &infra.secrets)
        .await
        .unwrap();
    for db in regencylib::databases::STATIC {
        regencylib::databases::ensure(&couch, db).await.unwrap();
    }
    let jobs_db = jobs_db(&month_of(chrono::Utc::now()));
    regencylib::databases::ensure(&couch, &jobs_db)
        .await
        .unwrap();
    let connection = regencylib::amqp::connect(&infra.rabbitmq, &infra.secrets, "test")
        .await
        .unwrap();
    let channel = connection.create_channel().await.unwrap();
    topology::declare_exchanges(&channel).await.unwrap();
    topology::declare_live_stream(&channel, &Default::default())
        .await
        .unwrap();

    let dir = std::env::temp_dir().join(unique("backend-burst-"));
    std::fs::create_dir_all(&dir).unwrap();
    let key_path = dir.join("session.key");
    std::fs::write(&key_path, "0123456789abcdef0123456789abcdef-test").unwrap();
    let admin_password = dir.join("admin.password");
    std::fs::write(&admin_password, "bootstrap-password-123\n").unwrap();
    let free_port = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let (port, health_port) = (free_port(), free_port());
    // external level 0: callers without credentials act as the bootstrap admin
    let config = format!(
        r#"
[backend]
listen = "127.0.0.1:{port}"
session_key = {{ SecRef = "{key}" }}
bootstrap_admin = "{admin}"
bootstrap_password = {{ SecRef = "{admin_password}" }}
[rabbitmq]
url = "{amqp}"
stream_port = {stream_port}
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
external = 0
"#,
        key = key_path.display(),
        admin = unique("admin"),
        admin_password = admin_password.display(),
        amqp = infra.rabbitmq.url,
        stream_port = infra.rabbitmq.stream_port,
        couch = infra.couchdb.url,
        rp = infra.rabbitmq.password.as_ref().unwrap().sec_ref(),
        cp = infra.couchdb.password.as_ref().unwrap().sec_ref(),
    );
    let config_path = dir.join("backend.toml");
    std::fs::write(&config_path, config).unwrap();
    let _child = tokio::process::Command::new(env!("CARGO_BIN_EXE_regency-backend"))
        .args(["--config", config_path.to_str().unwrap()])
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut client: Option<RegencyClient<Channel>> = None;
    for _ in 0..100 {
        if let Ok(c) = RegencyClient::connect(format!("http://127.0.0.1:{port}")).await {
            client = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let mut client = client.expect("backend up");
    for _ in 0..100 {
        if reqwest::get(format!("http://127.0.0.1:{health_port}/readyz"))
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // the client's view of a run's jobs (WatchJobs by origin)
    let origin = unique("burst-");
    let mut request = api::WatchJobsRequest::new();
    request.set_origin(origin.as_str());
    request.set_since_hours(1);
    let mut stream = client.watch_jobs(request).await.unwrap().into_inner();
    let first = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("snapshot")
        .unwrap()
        .unwrap();
    assert!(first.snapshot_done(), "empty snapshot expected");
    // backend follows the jobs database from "now": let it start
    tokio::time::sleep(Duration::from_secs(2)).await;

    // the burst, unread: every job PENDING, then FINISHED
    let ids: Vec<String> = (0..JOBS).map(|i| format!("{origin}:{i:05}")).collect();
    let pending: Vec<_> = ids
        .iter()
        .map(|id| Stored::new(job_doc_id(id), job_doc(id, &origin, JobState::Pending)))
        .collect();
    let revs = write(&couch, &jobs_db, pending).await;
    let finished: Vec<_> = ids
        .iter()
        .map(|id| {
            let mut stored = Stored::new(job_doc_id(id), job_doc(id, &origin, JobState::Finished));
            stored.rev = Some(revs[&job_doc_id(id)].clone());
            stored
        })
        .collect();
    write(&couch, &jobs_db, finished).await;
    // the client was busy: backend's channel overflowed meanwhile
    tokio::time::sleep(Duration::from_secs(5)).await;

    // now read: every job must end FINISHED
    let mut seen: HashMap<String, String> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while seen.len() < JOBS || seen.values().any(|s| s != "FINISHED") {
        let event = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap_or_else(|_| {
                let finished = seen.values().filter(|s| *s == "FINISHED").count();
                panic!("{} jobs seen, {finished} finished, of {JOBS}", seen.len())
            })
            .expect("stream open")
            .unwrap();
        if event.has_upsert() {
            let job = event.upsert();
            seen.insert(job.job_id().to_string(), job.state().to_string());
        }
    }
}
