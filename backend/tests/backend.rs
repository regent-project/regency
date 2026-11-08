//! End-to-end: the backend binary with CouchDB and RabbitMQ (documents are
//! written directly, standing in for dbwriter).
//! `cargo test -p backend -- --ignored` (docker; or REGENCY_TEST_EXTERNAL=1).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};
use futures::StreamExt;
use lapin::options::{BasicAckOptions, BasicConsumeOptions, QueueBindOptions, QueueDeclareOptions};
use lapin::types::FieldTable;
use regencylib::api::regency_client::RegencyClient;
use regencylib::couch::Couch;
use regencylib::messages::{Command, CommandBody};
use regencylib::model::*;
use regencylib::proto as api;
use regencylib::selector::HostSelector;
use regencylib::testing::{infra, unique};
use regencylib::topology::{self, EX_COMMANDS};
use regent_sdk::ExpectedState;
use regent_sdk::hosts::managed_host::ManagedHostBuilder;
use tonic::Request;
use tonic::transport::Channel;

fn authed<T>(message: T, token: &str) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker"]
async fn backend_authenticates_authorizes_and_streams() {
    let infra = infra().await;
    let couch = Couch::from_config(&infra.couchdb, &infra.secrets)
        .await
        .unwrap();
    for db in regencylib::databases::STATIC {
        regencylib::databases::ensure(&couch, db).await.unwrap();
    }
    let source = unique("src");
    let host = unique("h");
    let alice = unique("alice");
    let env = unique("test");
    let hash = argon2::Argon2::default()
        .hash_password(b"alice-password-123", &SaltString::generate(&mut OsRng))
        .unwrap()
        .to_string();
    couch
        .put(
            DB_USERS,
            &Stored::new(
                user_doc_id(&alice),
                UserDoc {
                    name: alice.clone(),
                    account: AccountKind::User,
                    password_hash: Some(hash),
                    tokens: vec![],
                    groups: BTreeSet::from(["operators".to_string()]),
                    disabled: false,
                    bootstrap: false,
                },
            ),
        )
        .await
        .unwrap();
    let items = [
        CatalogBody::Source(SourceDef {
            source_id: source.clone(),
            kind: SourceKind::Api,
            repo: None,
            branch: None,
            path: None,
            poll_interval_sec: 300,
            credentials: None,
        }),
        CatalogBody::Hostdef(HostDef {
            source_id: source.clone(),
            host_id: host.clone(),
            builder: ManagedHostBuilder::new(&host, "10.0.0.1:22", None),
            inventories: BTreeSet::new(),
            labels: BTreeMap::from([("env".to_string(), env.clone())]),
            pool: None,
        }),
        CatalogBody::ExpectedState(ExpectedStateDef {
            source_id: source.clone(),
            name: "base".into(),
            state: ExpectedState::new(),
        }),
        CatalogBody::Assignment(AssignmentDef {
            source_id: source.clone(),
            name: "base".into(),
            selector: HostSelector::hosts([host.clone()]),
            expected_state: format!("{source}/base"),
            priority: 0,
        }),
    ];
    for body in items {
        let item = CatalogItem {
            body,
            revision: "1".into(),
        };
        couch
            .put(DB_CATALOG, &Stored::new(item.doc_id(), item))
            .await
            .unwrap();
    }
    let mut host_doc = HostDoc::empty(&host, chrono::Utc::now());
    host_doc.state.compliance = "not_compliant".into();
    let rev = couch
        .put(DB_STATE, &Stored::new(host_doc_id(&host), host_doc.clone()))
        .await
        .unwrap();

    // commands published by backend
    let connection = regencylib::amqp::connect(&infra.rabbitmq, &infra.secrets, "test")
        .await
        .unwrap();
    let channel = connection.create_channel().await.unwrap();
    topology::declare_exchanges(&channel).await.unwrap();
    let queue = unique("test.backend.");
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
            EX_COMMANDS.into(),
            "#".into(),
            QueueBindOptions::default(),
            FieldTable::default(),
        )
        .await
        .unwrap();
    let mut commands = channel
        .basic_consume(
            queue.as_str().into(),
            "".into(),
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .unwrap();

    let dir = std::env::temp_dir().join(unique("backend-test-"));
    std::fs::create_dir_all(&dir).unwrap();
    let key_path = dir.join("session.key");
    std::fs::write(&key_path, "0123456789abcdef0123456789abcdef-test").unwrap();
    let admin_password = dir.join("admin.password");
    std::fs::write(&admin_password, "bootstrap-password-123\n").unwrap();
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
    let admin = unique("admin");
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
external = 1
"#,
        key = key_path.display(),
        admin_password = admin_password.display(),
        amqp = infra.rabbitmq.url,
        stream_port = infra.rabbitmq.stream_port,
        couch = infra.couchdb.url,
        rp = infra.rabbitmq.password.as_ref().unwrap().sec_ref(),
        cp = infra.couchdb.password.as_ref().unwrap().sec_ref(),
    );
    let config_path = dir.join("backend.toml");
    std::fs::write(&config_path, config).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_regency-backend"))
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
            .unwrap()
            .status()
            .is_success()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // bootstrap admin creation (D12)
    let delivery = tokio::time::timeout(Duration::from_secs(10), commands.next())
        .await
        .expect("bootstrap")
        .unwrap()
        .unwrap();
    delivery.ack(BasicAckOptions::default()).await.unwrap();
    match Command::from_bytes(&delivery.data).unwrap().body {
        CommandBody::UserUpsert(user) => {
            assert_eq!(user.name, admin);
            assert!(user.bootstrap);
            assert!(user.password_hash.unwrap().starts_with("$argon2id$"));
        }
        other => panic!("unexpected {other:?}"),
    }

    // login
    let mut login = api::LoginRequest::new();
    login.set_username(alice.as_str());
    login.set_password("wrong");
    assert_eq!(
        client.login(login.clone()).await.unwrap_err().code(),
        tonic::Code::Unauthenticated
    );
    login.set_password("alice-password-123");
    let token = client
        .login(login)
        .await
        .unwrap()
        .into_inner()
        .token()
        .to_string();
    let me = client
        .who_am_i(authed(api::WhoAmIRequest::new(), &token))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(me.principal().to_string(), format!("User::\"{alice}\""));

    // D23 one-shot builder: the SDK's attribute schema, and YAML <-> JSON
    let schema = client
        .get_attribute_schema(authed(api::GetAttributeSchemaRequest::new(), &token))
        .await
        .unwrap()
        .into_inner();
    let schema: serde_json::Value =
        serde_json::from_str(&schema.schema_json().to_string()).unwrap();
    assert!(schema["properties"]["Detail"].is_object(), "{schema}");
    assert!(schema["$defs"]["LineInFileExpectedState"].is_object());
    let mut convert = api::ConvertAttributesRequest::new();
    convert.set_yaml(
        "Name: motd\nPrivilege: !None\nDetail: !LineInFile\n  FilePath: /etc/motd\n  State: !Present\n    Create: true\n  Line: !Raw hello\n",
    );
    let converted = client
        .convert_attributes(authed(convert, &token))
        .await
        .unwrap()
        .into_inner();
    assert!(
        converted.error().to_string().is_empty(),
        "{}",
        converted.error()
    );
    let json: serde_json::Value = serde_json::from_str(&converted.json().to_string()).unwrap();
    assert_eq!(json[0]["Detail"]["LineInFile"]["FilePath"], "/etc/motd");
    let mut back = api::ConvertAttributesRequest::new();
    back.set_json(converted.json().to_string().as_str());
    let yaml = client
        .convert_attributes(authed(back, &token))
        .await
        .unwrap()
        .into_inner();
    assert!(yaml.yaml().to_string().contains("!LineInFile"));
    assert_eq!(
        client
            .who_am_i(Request::new(api::WhoAmIRequest::new()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );

    // live view: snapshot then update
    let mut watch = api::WatchHostsRequest::new();
    let mut selector = api::HostSelector::new();
    selector.labels_mut().insert("env", env.as_str());
    watch.set_selector(selector);
    let mut stream = client
        .watch_hosts(authed(watch, &token))
        .await
        .unwrap()
        .into_inner();
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if event.has_snapshot_done() {
            break;
        }
        seen.push(event.upsert().host_id().to_string());
    }
    assert_eq!(seen, std::slice::from_ref(&host));
    host_doc.state.compliance = "compliant".into();
    let mut updated = Stored::new(host_doc_id(&host), host_doc);
    updated.rev = Some(rev);
    couch.put(DB_STATE, &updated).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(event.upsert().compliance().to_string(), "compliant");
    assert_eq!(
        event
            .upsert()
            .assignments()
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>(),
        [format!("{source}/base")]
    );
    assert!(!event.upsert().effective_revision().is_empty());
    assert_eq!(
        event.upsert().labels().get("env").map(|v| v.to_string()),
        Some(env.clone())
    );

    // commands: operators may assess, not reach, nor change settings
    let run = |job: api::JobKind| {
        let mut run = api::RunNowRequest::new();
        run.set_command_id(unique("c"));
        let mut selector = api::HostSelector::new();
        selector.hosts_mut().push(host.as_str());
        run.set_selector(selector);
        run.set_job(job);
        run
    };
    let assess = run(api::JobKind::Assess);
    let command_id = assess.command_id().to_string();
    client.run_now(authed(assess, &token)).await.unwrap();
    let delivery = tokio::time::timeout(Duration::from_secs(10), commands.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let command = Command::from_bytes(&delivery.data).unwrap();
    assert_eq!(command.command_id, command_id);
    assert_eq!(command.issued_by, format!("User::\"{alice}\""));
    assert!(matches!(command.body, CommandBody::Run(_)));
    assert_eq!(
        client
            .run_now(authed(run(api::JobKind::Reach), &token))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    let mut settings = api::SetSettingsRequest::new();
    settings.set_command_id("c-settings");
    settings.set_key("retention_months");
    settings.set_value_json("6");
    assert_eq!(
        client
            .set_settings(authed(settings, &token))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );

    // gRPC-Web over HTTP/1.1 (browsers, D14)
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/regency.v1.Regency/WhoAmI"))
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .header("authorization", format!("Bearer {token}"))
        .body(vec![0u8, 0, 0, 0, 0])
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let body = response.bytes().await.unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains(&alice),
        "grpc-web answer names the caller"
    );

    child.kill().await.unwrap();
}
