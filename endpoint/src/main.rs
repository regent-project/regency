//! regency endpoint: gRPC gateway for proactive hosts (D5, D6, D13).
//!
//! regops pushes one outcome per compliance pass (`ReportOutcome`, mTLS); the
//! endpoint checks the identity, then republishes the host's bytes unchanged
//! on `regency.outcomes` and answers OK only after the publisher confirm.
//! Stateless: run as many replicas as needed behind a TCP load balancer (TLS
//! is terminated here, the client certificate is the identity).

mod ca;
mod config;
mod identity;
mod service;
mod trust;

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use regencylib::amqp::PublisherLink;
use regencylib::config::resolve_secret;
use regencylib::couch::Couch;
use regencylib::telemetry::{Health, counter, shutdown_signal};
use regencylib::tokens::KeyRing;
use regent_sdk::proto::grpc::reporting::reporting_server::ReportingServer;
use rustls::ServerConfig;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use regencylib::security::Level;

use crate::config::Config;

const SERVICE: &str = "endpoint";

#[derive(Parser)]
#[command(
    name = "regency-endpoint",
    version,
    about = "regency gateway for proactive hosts (regops)"
)]
struct Args {
    /// Configuration file (missing = defaults + REGENCY_* env vars).
    #[arg(short, long, default_value_t = regencylib::config::default_path(SERVICE))]
    config: String,
    /// Run under the Windows Service Control Manager (D22; set by the MSI's
    /// service registration).
    #[arg(long, hide = true)]
    windows_service: bool,
}

fn main() {
    let args = Args::parse();
    let windows_service = args.windows_service;
    regencylib::process::launch(
        "regency-endpoint",
        windows_service,
        Box::new(move || {
            tokio::runtime::Runtime::new()
                .expect("tokio runtime")
                .block_on(async_main(args))
        }),
    );
}

async fn async_main(args: Args) -> i32 {
    let central = regencylib::central::startup::<Config>(
        SERVICE,
        env!("CARGO_PKG_VERSION"),
        &args.config,
        true,
        true,
        |config| &config.common,
        regencylib::services::endpoint::external_checks,
    )
    .await;
    let (config, instance) = (central.config.clone(), central.instance.clone());
    if let Err(e) = run(config, instance, central).await {
        error!(error = %format!("{e:#}"), "endpoint stopped");
        return 1;
    }
    regencylib::central::exit_code()
}

async fn run(
    config: Config,
    instance: String,
    central: regencylib::central::Resolved<Config>,
) -> anyhow::Result<()> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let secrets = config.common.secrets.pool()?;
    let health = Health::new();
    health.spawn(config.common.health.clone());
    central.spawn(&health);
    let cancel = CancellationToken::new();

    // modes (D13)
    let ca = match &config.ca {
        Some(section) => {
            let ca_pem = std::fs::read_to_string(&section.cert)
                .map_err(|e| anyhow::anyhow!("cannot read CA certificate {}: {e}", section.cert))?;
            let key_pem = resolve_secret(&secrets, &section.key).await?;
            let enrollment_key = resolve_secret(&secrets, &section.enrollment_key).await?;
            anyhow::ensure!(
                enrollment_key.len() >= 32,
                "ca.enrollment_key must be at least 32 bytes"
            );
            Some((
                Arc::new(ca::BuiltInCa::new(
                    &ca_pem,
                    &key_pem,
                    section.cert_lifetime_days,
                )?),
                KeyRing::single(enrollment_key.as_bytes()),
            ))
        }
        None => None,
    };
    let external = config
        .endpoint
        .external_ca_bundle
        .as_deref()
        .map(identity::load_certs)
        .transpose()?;
    info!(
        external_ca = external.is_some(),
        builtin_ca = ca.is_some(),
        pinning = config.pinning.enabled,
        trust_on_first_use = config.pinning.trust_on_first_use,
        "client trust modes"
    );
    let verifiers = identity::Verifiers::new(
        external,
        ca.as_ref().map(|(ca, _)| ca.ca_der()),
        config.pinning.enabled,
        config.common.security.external == Level::Open,
        provider.clone(),
    )?;

    // dependencies
    let couch = Couch::from_config(&config.common.couchdb, &secrets).await?;
    let trust = Arc::new(trust::TrustState::default());
    trust.spawn_followers(couch, health.clone(), cancel.clone());
    let publisher = PublisherLink::spawn(
        config.common.rabbitmq.clone(),
        secrets.clone(),
        format!("{SERVICE}/{instance}"),
        health.clone(),
        cancel.clone(),
    );

    let service = service::ReportingService {
        verifiers,
        trust,
        ca: ca.as_ref().map(|(ca, _)| ca.clone()),
        enrollment_keys: ca.map(|(_, keys)| keys),
        trust_on_first_use: config.pinning.trust_on_first_use,
        publisher,
        publish_wait: Duration::from_secs(config.endpoint.publish_wait_sec),
        instance: instance.clone(),
        outcomes: counter(
            &health,
            "regency_endpoint_outcomes_total",
            "Outcomes received from proactive hosts, by result",
            &["result"],
        ),
    };

    // TLS: any client certificate at the handshake, trust decided per request
    let chain = identity::load_certs(&config.endpoint.cert)?;
    let key_pem = resolve_secret(&secrets, &config.endpoint.key).await?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())?
        .ok_or_else(|| anyhow::anyhow!("no private key in endpoint.key"))?;
    // D19 level 3: TLS 1.3 only
    let versions: &[&rustls::SupportedProtocolVersion] =
        if config.common.security.external >= Level::Hardened {
            &[&rustls::version::TLS13]
        } else {
            rustls::DEFAULT_VERSIONS
        };
    let mut tls = ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(versions)?
        .with_client_cert_verifier(Arc::new(identity::AnyClientCert::new(&provider)))
        .with_single_cert(chain, key)?;
    tls.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(tls));

    let listener = TcpListener::bind(&config.endpoint.listen).await?;
    info!(listen = %config.endpoint.listen, "serving regent.v1.Reporting");
    let (tx, rx) = tokio::sync::mpsc::channel(128);
    let handshake_timeout = Duration::from_secs(config.endpoint.handshake_timeout_sec);
    let accept_cancel = cancel.clone();
    tokio::spawn(async move {
        loop {
            let (tcp, peer) = tokio::select! {
                _ = accept_cancel.cancelled() => return,
                accepted = listener.accept() => match accepted {
                    Ok(accepted) => accepted,
                    Err(e) => {
                        warn!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                },
            };
            let acceptor = acceptor.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                match tokio::time::timeout(handshake_timeout, acceptor.accept(tcp)).await {
                    Ok(Ok(stream)) => {
                        let _ = tx.send(Ok::<_, std::io::Error>(stream)).await;
                    }
                    Ok(Err(e)) => debug!(peer = %peer, error = %e, "TLS handshake failed"),
                    Err(_) => debug!(peer = %peer, "TLS handshake timed out"),
                }
            });
        }
    });
    health.ready("listener");

    let shutdown = cancel.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        shutdown.cancel();
    });
    tonic::transport::Server::builder()
        .add_service(ReportingServer::new(service))
        .serve_with_incoming_shutdown(
            tokio_stream::wrappers::ReceiverStream::new(rx),
            cancel.cancelled(),
        )
        .await?;
    info!("endpoint stopped");
    Ok(())
}
