//! D19 internal links at security level 3 against real brokers configured
//! for TLS 1.3 with mandatory client certificates: AMQP (`amqps://`), the
//! stream port, and CouchDB (`https://`). `cargo test -p regencylib --test
//! tls -- --ignored` (docker).

use std::path::PathBuf;

use regencylib::config::{CouchDbConfig, RabbitMqConfig, SecretsConfig};
use regencylib::control::{AbortPublisher, environment};
use regencylib::couch::Couch;
use regencylib::messages::AbortRequest;
use regencylib::security::{Level, check_internal};
use regencylib::topology::{TopologyConfig, declare_control_stream};
use regent_sdk::secrets::SecretReference;
use testcontainers::CopyTargetOptions;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

struct Pki {
    ca_pem: String,
    server: (String, String),
    client: (String, String),
}

fn pki() -> Pki {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(rcgen::DnType::CommonName, "regency test CA");
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::from_ca_cert_pem(&ca_cert.pem(), ca_key).unwrap();
    let leaf = |name: &str| {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec![name.to_string()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        (
            params.signed_by(&key, &issuer).unwrap().pem(),
            key.serialize_pem(),
        )
    };
    Pki {
        ca_pem: ca_cert.pem(),
        server: leaf("localhost"),
        client: leaf("regency-test-client"),
    }
}

fn file(dir: &std::path::Path, name: &str, content: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path.to_str().unwrap().to_string()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn copy(path: &str) -> CopyTargetOptions {
    CopyTargetOptions::new(path).with_mode(0o644)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker"]
async fn level_3_internal_links_with_mutual_tls() {
    let pki = pki();
    let dir = std::env::temp_dir().join(format!("regency-tls-{:08x}", rand::random::<u32>()));
    std::fs::create_dir_all(&dir).unwrap();
    let ca_file = file(&dir, "ca.pem", &pki.ca_pem);
    let cert_file = file(&dir, "client.pem", &pki.client.0);
    let key_file = file(&dir, "client.key", &pki.client.1);
    let rabbit_password = file(&dir, "rabbit.password", "regency-strong-password");
    let couch_password = file(&dir, "couch.password", "admin-strong-password");

    let (amqps_port, stream_port) = (free_port(), free_port());
    let rabbit_conf = format!(
        "listeners.ssl.default = 5671\n\
         ssl_options.cacertfile = /etc/rabbitmq/tls/ca.pem\n\
         ssl_options.certfile = /etc/rabbitmq/tls/server.pem\n\
         ssl_options.keyfile = /etc/rabbitmq/tls/server.key\n\
         ssl_options.verify = verify_peer\n\
         ssl_options.fail_if_no_peer_cert = true\n\
         ssl_options.versions.1 = tlsv1.3\n\
         stream.listeners.ssl.default = 5551\n\
         stream.advertised_host = localhost\n\
         stream.advertised_tls_port = {stream_port}\n"
    );
    let rabbit = GenericImage::new("rabbitmq", "4.1-management")
        .with_wait_for(WaitFor::message_on_stdout("Server startup complete"))
        .with_entrypoint("sh")
        .with_cmd([
            "-c",
            "echo '[rabbitmq_management,rabbitmq_stream].' > /etc/rabbitmq/enabled_plugins && exec docker-entrypoint.sh rabbitmq-server",
        ])
        .with_env_var("RABBITMQ_DEFAULT_USER", "regency")
        .with_env_var("RABBITMQ_DEFAULT_PASS", "regency-strong-password")
        .with_copy_to(copy("/etc/rabbitmq/conf.d/90-tls.conf"), rabbit_conf.into_bytes())
        .with_copy_to(copy("/etc/rabbitmq/tls/ca.pem"), pki.ca_pem.clone().into_bytes())
        .with_copy_to(copy("/etc/rabbitmq/tls/server.pem"), pki.server.0.clone().into_bytes())
        .with_copy_to(copy("/etc/rabbitmq/tls/server.key"), pki.server.1.clone().into_bytes())
        .with_mapped_port(amqps_port, 5671.tcp())
        .with_mapped_port(stream_port, 5551.tcp())
        .start()
        .await
        .expect("RabbitMQ with TLS");

    let https_port = free_port();
    let couch_ini = "[ssl]\nenable = true\nport = 6984\n\
        cert_file = /opt/couchdb/etc/tls/server.pem\nkey_file = /opt/couchdb/etc/tls/server.key\n\
        cacert_file = /opt/couchdb/etc/tls/ca.pem\nverify_ssl_certificates = true\nfail_if_no_peer_cert = true\n\
        [couchdb]\nsingle_node = true\n";
    let couch = GenericImage::new("couchdb", "3.5")
        .with_wait_for(WaitFor::message_on_either_std("Apache CouchDB has started"))
        .with_env_var("COUCHDB_USER", "admin")
        .with_env_var("COUCHDB_PASSWORD", "admin-strong-password")
        .with_copy_to(
            copy("/opt/couchdb/etc/local.d/90-tls.ini"),
            couch_ini.as_bytes().to_vec(),
        )
        .with_copy_to(
            copy("/opt/couchdb/etc/tls/ca.pem"),
            pki.ca_pem.clone().into_bytes(),
        )
        .with_copy_to(
            copy("/opt/couchdb/etc/tls/server.pem"),
            pki.server.0.clone().into_bytes(),
        )
        .with_copy_to(
            copy("/opt/couchdb/etc/tls/server.key"),
            pki.server.1.clone().into_bytes(),
        )
        .with_mapped_port(https_port, 6984.tcp())
        .start()
        .await
        .expect("CouchDB with TLS");

    let secrets = SecretsConfig::default().pool().unwrap();
    let reference = |path: &str| SecretReference::from(path, Some("files".to_string()));
    let rabbitmq = RabbitMqConfig {
        url: format!("amqps://localhost:{amqps_port}"),
        stream_port,
        username: Some("regency".into()),
        password: Some(reference(&rabbit_password)),
        ca_file: Some(ca_file.clone()),
        cert_file: Some(cert_file.clone()),
        key: Some(reference(&key_file)),
        ..RabbitMqConfig::default()
    };
    let couchdb = CouchDbConfig {
        url: format!("https://localhost:{https_port}"),
        username: Some("admin".into()),
        password: Some(reference(&couch_password)),
        ca_file: Some(ca_file.clone()),
        cert_file: Some(cert_file.clone()),
        key: Some(reference(&key_file)),
    };
    // the configuration itself satisfies level 3
    let violations = check_internal(Level::Hardened, Some(&rabbitmq), Some(&couchdb));
    assert!(violations.is_empty(), "{:?}", violations.0);

    // AMQP over TLS 1.3 with a client certificate
    let connection = regencylib::amqp::connect(&rabbitmq, &secrets, "tls-test")
        .await
        .expect("amqps with mTLS");
    let channel = connection.create_channel().await.unwrap();
    declare_control_stream(&channel, &TopologyConfig::default())
        .await
        .unwrap();
    // without the client certificate the broker refuses
    let anonymous = RabbitMqConfig {
        cert_file: None,
        key: None,
        ..rabbitmq.clone()
    };
    assert!(
        regencylib::amqp::connect(&anonymous, &secrets, "tls-test-no-cert")
            .await
            .is_err()
    );

    // stream port over TLS with the client certificate
    let environment = environment(&rabbitmq, &secrets)
        .await
        .expect("stream over TLS");
    AbortPublisher::new(&environment)
        .await
        .expect("stream producer")
        .publish(&AbortRequest::new("tls-test", "test"))
        .await
        .expect("published over TLS");

    // CouchDB over HTTPS with the client certificate
    let mut ping = None;
    for _ in 0..50 {
        let client = Couch::from_config(&couchdb, &secrets).await.unwrap();
        if let Ok(answer) = client.ping().await {
            ping = Some(answer);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    assert_eq!(ping.expect("https with mTLS")["couchdb"], "Welcome");
    let no_cert = CouchDbConfig {
        cert_file: None,
        key: None,
        ..couchdb.clone()
    };
    assert!(
        Couch::from_config(&no_cert, &secrets)
            .await
            .unwrap()
            .ping()
            .await
            .is_err()
    );
    let wrong_ca = CouchDbConfig {
        ca_file: None,
        ..couchdb
    };
    // system roots do not know the test CA: the server is not trusted
    assert!(
        Couch::from_config(&wrong_ca, &secrets)
            .await
            .unwrap()
            .ping()
            .await
            .is_err()
    );

    drop((rabbit, couch));
    let _ = std::fs::remove_dir_all(PathBuf::from(&dir));
}
