//! End-to-end: the endpoint binary between a regops-like client and RabbitMQ.
//! `cargo test -p endpoint -- --ignored` (docker; or REGENCY_TEST_EXTERNAL=1).

use std::time::Duration;

use futures::StreamExt;
use lapin::options::{BasicAckOptions, BasicConsumeOptions, QueueBindOptions, QueueDeclareOptions};
use lapin::types::FieldTable;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use regencylib::messages::{Command, CommandBody, decode_outcome};
use regencylib::testing::{infra, unique};
use regencylib::topology::{EX_COMMANDS, EX_OUTCOMES, declare_exchanges};
use regent_sdk::proto::grpc::reporting::reporting_client::ReportingClient;
use regent_sdk::proto::{ReportLiveRequest, ReportOutcomeRequest};
use regent_sdk::state::compliance::ManagedHostStatus;
use regent_sdk::task::{Job, RegentTaskResult};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

struct Pki {
    ca_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl Pki {
    fn new() -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "test CA");
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let cert = params.self_signed(&key).unwrap();
        let ca_pem = cert.pem();
        Self {
            issuer: Issuer::from_ca_cert_pem(&ca_pem, key).unwrap(),
            ca_pem,
        }
    }

    /// (cert PEM, key PEM) for `name`.
    fn issue(&self, name: &str) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![name.to_string()]).unwrap();
        params.distinguished_name.push(DnType::CommonName, name);
        (
            params.signed_by(&key, &self.issuer).unwrap().pem(),
            key.serialize_pem(),
        )
    }
}

fn self_signed(name: &str) -> (String, String) {
    let key = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec![name.to_string()]).unwrap();
    (params.self_signed(&key).unwrap().pem(), key.serialize_pem())
}

fn write(dir: &std::path::Path, name: &str, content: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path.to_str().unwrap().to_string()
}

fn outcome(host: &str) -> ReportOutcomeRequest {
    let now = chrono::Utc::now();
    let result = RegentTaskResult::from(
        unique("pass-"),
        host.to_string(),
        Job::Assess,
        now,
        now,
        Ok(ManagedHostStatus::already_compliant()),
    );
    let mut request = ReportOutcomeRequest::new();
    request.set_result(result.to_proto());
    request
}

fn live_update(host: &str, run: &str, seq: u64) -> regent_sdk::hosts::live::LiveUpdate {
    regent_sdk::hosts::live::LiveUpdate {
        run_id: run.to_string(),
        host_id: host.to_string(),
        job: Job::Assess,
        seq,
        at: chrono::Utc::now(),
        event: regent_sdk::hosts::live::LiveEvent::Output {
            exec_id: 1,
            stream: regent_sdk::hosts::live::OutputStream::Stdout,
            line: format!("line {seq} of a rather verbose command output"),
        },
    }
}

fn live_batch(host: &str, run: &str, seqs: std::ops::RangeInclusive<u64>) -> ReportLiveRequest {
    let mut batch = ReportLiveRequest::new();
    for seq in seqs {
        batch
            .updates_mut()
            .push(live_update(host, run, seq).to_proto());
    }
    batch
}

async fn client(port: u16, ca_pem: &str, identity: (String, String)) -> ReportingClient<Channel> {
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca_pem))
        .domain_name("localhost")
        .identity(Identity::from_pem(identity.0, identity.1));
    for _ in 0..100 {
        if let Ok(channel) = Endpoint::from_shared(format!("https://127.0.0.1:{port}"))
            .unwrap()
            .tls_config(tls.clone())
            .unwrap()
            .connect()
            .await
        {
            return ReportingClient::new(channel);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("endpoint did not come up");
}

#[tokio::test]
#[ignore = "needs docker"]
async fn outcomes_flow_through_the_endpoint() {
    let infra = infra().await;
    let dir = std::env::temp_dir().join(unique("endpoint-test-"));
    std::fs::create_dir_all(&dir).unwrap();
    let pki = Pki::new();
    let (server_cert, server_key) = pki.issue("localhost");
    let ca_path = write(&dir, "ca.pem", &pki.ca_pem);
    let cert_path = write(&dir, "server.pem", &server_cert);
    let key_path = write(&dir, "server.key", &server_key);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let health_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let rabbit_password = infra
        .rabbitmq
        .password
        .as_ref()
        .unwrap()
        .sec_ref()
        .to_string();
    let couch_password = infra
        .couchdb
        .password
        .as_ref()
        .unwrap()
        .sec_ref()
        .to_string();
    let config = format!(
        r#"
[endpoint]
listen = "127.0.0.1:{port}"
cert = "{cert_path}"
key = {{ SecRef = "{key_path}" }}
external_ca_bundle = "{ca_path}"
[pinning]
enabled = true
trust_on_first_use = true
[rabbitmq]
url = "{amqp}"
stream_port = {stream_port}
username = "regency"
password = {{ SecRef = "{rabbit_password}" }}
[couchdb]
url = "{couch}"
username = "admin"
password = {{ SecRef = "{couch_password}" }}
[health]
listen = "127.0.0.1:{health_port}"
[log]
format = "text"
[security]
internal = 1
external = 1
"#,
        amqp = infra.rabbitmq.url,
        stream_port = infra.rabbitmq.stream_port,
        couch = infra.couchdb.url,
    );
    let config_path = write(&dir, "endpoint.toml", &config);

    // databases normally created by `regencyctl init`
    let couch = regencylib::couch::Couch::from_config(&infra.couchdb, &infra.secrets)
        .await
        .unwrap();
    for db in [regencylib::model::DB_STATE, regencylib::model::DB_SETTINGS] {
        couch.create_db(db).await.unwrap();
    }

    // observe what the endpoint publishes
    let connection = regencylib::amqp::connect(&infra.rabbitmq, &infra.secrets, "test")
        .await
        .unwrap();
    let channel = connection.create_channel().await.unwrap();
    declare_exchanges(&channel).await.unwrap();
    regencylib::topology::declare_live_stream(&channel, &Default::default())
        .await
        .unwrap();
    let mut consumers = Vec::new();
    for (exchange, key) in [(EX_OUTCOMES, "#"), (EX_COMMANDS, "host.*")] {
        let queue = unique("test.endpoint.");
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
                key.into(),
                QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await
            .unwrap();
        consumers.push(
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
    let mut commands = consumers.pop().unwrap();
    let mut outcomes = consumers.pop().unwrap();

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_regency-endpoint"))
        .args(["--config", &config_path])
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    // CA-issued identity
    let mut web01 = client(port, &pki.ca_pem, pki.issue("web-01")).await;
    let request = outcome("web-01");
    let sent = request.result().to_owned();
    web01.report_outcome(request).await.expect("accepted");
    let delivery = tokio::time::timeout(Duration::from_secs(10), outcomes.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    delivery.ack(BasicAckOptions::default()).await.unwrap();
    let decoded = decode_outcome(&delivery.data).unwrap();
    assert_eq!(decoded.result.host_id(), "web-01");
    assert_eq!(decoded.origin, "regops");
    assert_eq!(delivery.routing_key.as_str(), "assess");
    use protobuf::Serialize;
    assert_eq!(decoded.result_bytes, sent.serialize().unwrap());

    // D5: an outcome for another host is refused
    let refused = web01.report_outcome(outcome("web-02")).await.unwrap_err();
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);

    // D24: live updates go to regency.live, only for the host's own identity
    let live_batches = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let live_cancel = tokio_util::sync::CancellationToken::new();
    {
        let batches = live_batches.clone();
        tokio::spawn(regencylib::live::follow(
            infra.rabbitmq.clone(),
            infra.secrets.clone(),
            Duration::from_secs(0),
            live_cancel.clone(),
            |_| {},
            move |batch| batches.lock().unwrap().push(batch.to_vec()),
        ));
    }
    tokio::time::sleep(Duration::from_secs(1)).await; // subscribed
    let pass = unique("pass-");
    let accepted = web01
        .report_live(tokio_stream::iter(vec![live_batch("web-01", &pass, 1..=3)]))
        .await
        .expect("live accepted");
    assert_eq!(accepted.get_ref().accepted(), 3);
    let mut streamed = 0;
    for _ in 0..50 {
        streamed = live_batches
            .lock()
            .unwrap()
            .iter()
            .flat_map(|b| {
                regencylib::live::decode_batch(b)
                    .unwrap()
                    .updates()
                    .iter()
                    .map(|u| u.run_id().to_string())
                    .collect::<Vec<_>>()
            })
            .filter(|run| run == &pass)
            .count();
        if streamed == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(streamed, 3, "forwarded to regency.live");
    live_cancel.cancel();
    let refused = web01
        .report_live(tokio_stream::iter(vec![live_batch("web-02", &pass, 4..=4)]))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);

    // D24: a run log larger than the run_log_max_bytes setting is cropped
    let limit = couch
        .get::<regencylib::model::SettingsDoc>(
            regencylib::model::DB_SETTINGS,
            regencylib::model::SETTINGS_DOC_ID,
        )
        .await
        .unwrap()
        .map(|s| s.doc.run_log_max_bytes())
        .unwrap_or(regencylib::model::DEFAULT_RUN_LOG_MAX_BYTES);
    if (1..=8 * 1024 * 1024).contains(&limit) {
        let mut request = outcome("web-01");
        let job = request.result().idempotency_key().to_string();
        let mut log = regent_sdk::hosts::live::RunLog::new(0);
        let mut seq = 0;
        while log.size_bytes() <= limit + 1000 {
            seq += 1;
            log.push(live_update("web-01", &job, seq));
        }
        request.set_run_log(log.to_proto());
        web01.report_outcome(request).await.expect("accepted");
        let delivery = tokio::time::timeout(Duration::from_secs(10), outcomes.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        delivery.ack(BasicAckOptions::default()).await.unwrap();
        let decoded = decode_outcome(&delivery.data).unwrap();
        let kept = regent_sdk::hosts::live::RunLog::from_proto_bytes(
            decoded.run_log_bytes.as_ref().expect("run log kept"),
        )
        .unwrap();
        assert!(kept.size_bytes() <= limit, "cropped to {limit}");
        assert!(kept.omitted_updates() > 0 && kept.is_truncated());
        assert_eq!(kept.limit_bytes(), limit);
    }

    // self-signed + trust on first use: accepted and announced
    let edge_id = unique("edge-");
    let (ss_cert, ss_key) = self_signed(&edge_id);
    let mut edge = client(port, &pki.ca_pem, (ss_cert.clone(), ss_key)).await;
    edge.report_outcome(outcome(&edge_id))
        .await
        .expect("TOFU accepted");
    let delivery = tokio::time::timeout(Duration::from_secs(10), commands.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let command = Command::from_bytes(&delivery.data).unwrap();
    match command.body {
        CommandBody::HostCertPending {
            host_id,
            auto_approve,
            ..
        } => {
            assert_eq!(host_id, edge_id);
            assert!(auto_approve);
        }
        other => panic!("unexpected command {other:?}"),
    }
    // another self-signed cert for the same host is now refused
    let mut impostor = client(port, &pki.ca_pem, self_signed(&edge_id)).await;
    let refused = impostor
        .report_outcome(outcome(&edge_id))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::PermissionDenied);

    // readiness (the caches may need one long-poll round)
    for _ in 0..50 {
        if reqwest::get(format!("http://127.0.0.1:{health_port}/readyz"))
            .await
            .unwrap()
            .status()
            .is_success()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let ready = reqwest::get(format!("http://127.0.0.1:{health_port}/readyz"))
        .await
        .unwrap();
    assert!(
        ready.status().is_success(),
        "{}",
        ready.text().await.unwrap()
    );
    let metrics = reqwest::get(format!("http://127.0.0.1:{health_port}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("regency_endpoint_outcomes_total"));
    child.kill().await.unwrap();
}
