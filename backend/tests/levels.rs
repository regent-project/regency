//! D19 security levels of backend's external surface. No infrastructure
//! needed: these checks happen before RabbitMQ or CouchDB are involved.

use std::sync::Arc;
use std::time::Duration;

use regencylib::api::regency_client::RegencyClient;
use regencylib::proto as api;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};

struct Backend {
    child: tokio::process::Child,
    port: u16,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn write(dir: &std::path::Path, name: &str, content: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path.to_str().unwrap().to_string()
}

/// Config for `external` with extra `[backend]` lines; nothing listens on
/// the infrastructure ports (internal level 0).
fn config(dir: &std::path::Path, external: u8, backend_extra: &str) -> (String, u16) {
    let port = free_port();
    let key = write(
        dir,
        "session.key",
        "0123456789abcdef0123456789abcdef-levels",
    );
    let config = format!(
        r#"
# no CouchDB here: no central configuration (D21)
[central]
enabled = false
[security]
internal = 0
external = {external}
[backend]
listen = "127.0.0.1:{port}"
session_key = {{ SecRef = "{key}" }}
{backend_extra}
[rabbitmq]
url = "amqp://127.0.0.1:1"
[couchdb]
url = "http://127.0.0.1:1"
[health]
listen = "127.0.0.1:{health}"
[log]
format = "text"
"#,
        health = free_port(),
    );
    (
        write(dir, &format!("backend-{external}.toml"), &config),
        port,
    )
}

fn start(config: &str, port: u16) -> Backend {
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_regency-backend"))
        .args(["--config", config])
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    Backend { child, port }
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("backend-levels-{:08x}", rand::random::<u32>()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn level_2_refuses_to_start_without_tls() {
    let dir = tempdir();
    let (config, _) = config(&dir, 2, "");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_regency-backend"))
        .args(["--config", &config])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("TLS is required"), "{stderr}");
}

#[tokio::test]
async fn level_0_callers_are_the_bootstrap_admin() {
    let dir = tempdir();
    let (config, port) = config(&dir, 0, "");
    let mut backend = start(&config, port);
    let mut client = None;
    for _ in 0..100 {
        if let Ok(c) = RegencyClient::connect(format!("http://127.0.0.1:{}", backend.port)).await {
            client = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let me = client
        .unwrap()
        .who_am_i(api::WhoAmIRequest::new())
        .await
        .unwrap()
        .into_inner();
    assert!(me.bootstrap_admin());
    assert_eq!(me.principal().to_string(), "User::\"admin\"");
    backend.child.kill().await.unwrap();
}

#[tokio::test]
async fn level_3_tls13_only_and_no_password_logins() {
    let dir = tempdir();
    // a CA and a server certificate for 127.0.0.1
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::from_ca_cert_pem(&ca_cert.pem(), ca_key).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    let cert_path = write(&dir, "backend.pem", &cert.pem());
    let key_path = write(&dir, "backend.key", &key.serialize_pem());
    let extra = format!(
        "tls_cert = \"{cert_path}\"\ntls_key = {{ SecRef = \"{key_path}\" }}\nsession_ttl_sec = 3600"
    );
    let (config, port) = config(&dir, 3, &extra);
    let mut backend = start(&config, port);

    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca_cert.pem()))
        .domain_name("localhost");
    let mut client = None;
    for _ in 0..100 {
        let endpoint = Endpoint::from_shared(format!("https://127.0.0.1:{port}"))
            .unwrap()
            .tls_config(tls.clone())
            .unwrap();
        if let Ok(channel) = endpoint.connect().await {
            client = Some(RegencyClient::new(channel));
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut client = client.expect("backend over TLS");
    // no anonymous access, no password login but the bootstrap admin's
    assert_eq!(
        client
            .who_am_i(api::WhoAmIRequest::new())
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    let mut login = api::LoginRequest::new();
    login.set_username("alice");
    login.set_password("whatever-password");
    assert_eq!(
        client.login(login).await.unwrap_err().code(),
        tonic::Code::PermissionDenied
    );

    // a TLS 1.2-only client is refused
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(
            ca_cert.der().to_vec(),
        ))
        .unwrap();
    let tls12 = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS12])
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let handshake = tokio_rustls::TlsConnector::from(Arc::new(tls12))
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            tcp,
        )
        .await;
    assert!(handshake.is_err(), "TLS 1.2 must be refused at level 3");
    backend.child.kill().await.unwrap();
}
