//! regency backend (D1, D10, D12, D17): stateless, replicated gRPC and
//! gRPC-Web gateway. Follows CouchDB `_changes` for the live view (filtered
//! by Cedar), serves history, and turns authorized calls into commands on
//! `regency.commands`.

mod api;
mod auth;
mod config;
mod convert;
mod view;

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use regencylib::amqp::PublisherLink;
use regencylib::api::regency_server::RegencyServer;
use regencylib::config::resolve_secret;
use regencylib::couch::Couch;
use regencylib::messages::{Command, CommandBody};
use regencylib::model::{AccountKind, UserDoc};
use regencylib::security::Level;
use regencylib::telemetry::{Health, counter, shutdown_signal};
use regencylib::tokens::KeyRing;
use regencylib::topology::EX_COMMANDS;
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tracing::{error, info, warn};

use crate::config::Config;

const SERVICE: &str = "backend";

#[derive(Parser)]
#[command(
    name = "regency-backend",
    version,
    about = "regency backend (gRPC / gRPC-Web)"
)]
struct Args {
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
        "regency-backend",
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
        regencylib::services::backend::external_checks,
    )
    .await;
    let (config, instance) = (central.config.clone(), central.instance.clone());
    if let Err(e) = run(config, instance, central).await {
        error!(error = %format!("{e:#}"), "backend stopped");
        return 1;
    }
    regencylib::central::exit_code()
}

/// D12: create the bootstrap admin once (it can then be disabled, never
/// deleted). Every replica may try: the command is idempotent.
async fn bootstrap_admin(
    view: Arc<view::View>,
    publisher: PublisherLink,
    health: Health,
    name: String,
    password: String,
) {
    // wait for the accounts cache
    while !health.is_ready() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if view.user(&name).is_some() {
        return;
    }
    let hash = match tokio::task::spawn_blocking(move || auth::hash_secret(&password)).await {
        Ok(Ok(hash)) => hash,
        _ => {
            error!("cannot hash the bootstrap password");
            return;
        }
    };
    let command = Command::new(
        format!("bootstrap-admin-{name}"),
        "service:backend",
        CommandBody::UserUpsert(UserDoc {
            name: name.clone(),
            account: AccountKind::User,
            password_hash: Some(hash),
            tokens: Vec::new(),
            groups: Default::default(),
            disabled: false,
            bootstrap: true,
        }),
    );
    let Some(publisher) = publisher.wait(Duration::from_secs(60)).await else {
        warn!("bootstrap admin not created: RabbitMQ unavailable");
        return;
    };
    match command.to_bytes() {
        Ok(bytes) => match publisher
            .publish_json(EX_COMMANDS, command.routing_key(), &bytes)
            .await
        {
            Ok(()) => info!(user = %name, "bootstrap admin created"),
            Err(e) => warn!(error = %e, "bootstrap admin not created"),
        },
        Err(e) => warn!(error = %e, "bootstrap admin not created"),
    }
}

async fn run(
    config: Config,
    instance: String,
    central: regencylib::central::Resolved<Config>,
) -> anyhow::Result<()> {
    let secrets = config.common.secrets.pool()?;
    let health = Health::new();
    health.spawn(config.common.health.clone());
    central.spawn(&health);
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            cancel.cancel();
        });
    }
    let section = &config.backend;
    let session_key = resolve_secret(&secrets, &section.session_key).await?;
    anyhow::ensure!(
        session_key.len() >= 32,
        "backend.session_key must be at least 32 bytes"
    );
    let enrollment_keys = match &section.enrollment_key {
        Some(reference) => {
            let key = resolve_secret(&secrets, reference).await?;
            anyhow::ensure!(
                key.len() >= 32,
                "backend.enrollment_key must be at least 32 bytes"
            );
            Some(KeyRing::single(key.as_bytes()))
        }
        None => None,
    };

    let couch = Couch::from_config(&config.common.couchdb, &secrets).await?;
    let view = view::View::new(couch, section.bootstrap_admin.clone());
    view.spawn(&health, &cancel);
    let publisher = PublisherLink::spawn(
        config.common.rabbitmq.clone(),
        secrets.clone(),
        format!("{SERVICE}/{instance}"),
        health.clone(),
        cancel.clone(),
    );
    if let Some(reference) = &section.bootstrap_password {
        let password = resolve_secret(&secrets, reference).await?;
        tokio::spawn(bootstrap_admin(
            view.clone(),
            publisher.clone(),
            health.clone(),
            section.bootstrap_admin.clone(),
            password,
        ));
    }
    let authenticator = Arc::new(auth::Authenticator::new(
        view.clone(),
        KeyRing::single(session_key.as_bytes()),
        section.session_ttl_sec,
        section.bootstrap_admin.clone(),
        config.oidc.clone(),
        config.common.security.external,
    ));
    let service = api::Api {
        view,
        auth: authenticator,
        publisher,
        publish_wait: Duration::from_secs(section.publish_wait_sec),
        rabbitmq: config.common.rabbitmq.clone(),
        secrets: secrets.clone(),
        aborts: tokio::sync::Mutex::new(None),
        enrollment_keys,
        shutdown: cancel.clone(),
        rpcs: counter(
            &health,
            "regency_backend_rpcs_total",
            "RPCs, by method and status",
            &["rpc", "code"],
        ),
    };

    // browsers: gRPC-Web + CORS (D14)
    let cors = CorsLayer::new()
        .allow_headers(Any)
        .allow_methods(Any)
        .expose_headers(Any)
        .allow_origin(if section.cors_origins.iter().any(|o| o == "*") {
            AllowOrigin::any()
        } else {
            AllowOrigin::list(section.cors_origins.iter().filter_map(|o| o.parse().ok()))
        });
    // gRPC-Web translation on the API only (it refuses any other HTTP/1.1
    // request, e.g. the web client's files)
    let mut routes = tonic::service::Routes::new(tower_layer::Layer::layer(
        &tonic_web::GrpcWebLayer::new(),
        RegencyServer::new(service),
    ));
    if let Some(dir) = &section.frontend_dir {
        // the web client from the same origin (no CORS, one port); unknown
        // paths answer index.html (client-side routes such as /hosts/<id>)
        let index = std::path::Path::new(dir).join("index.html");
        info!(dir = %dir, "serving the web client");
        routes = tonic::service::Routes::from(
            routes.into_axum_router().fallback_service(
                tower_http::services::ServeDir::new(dir)
                    .fallback(tower_http::services::ServeFile::new(index)),
            ),
        );
    }
    let server = Server::builder()
        .accept_http1(true)
        .layer(cors)
        .add_routes(routes);
    if let (Some(cert), Some(key)) = (&section.tls_cert, &section.tls_key) {
        // own rustls acceptor: TLS 1.3 only at security level 3 (D19)
        let chain_pem =
            std::fs::read(cert).map_err(|e| anyhow::anyhow!("cannot read {cert}: {e}"))?;
        let chain =
            rustls_pemfile::certs(&mut chain_pem.as_slice()).collect::<Result<Vec<_>, _>>()?;
        let key_pem = resolve_secret(&secrets, key).await?;
        let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())?
            .ok_or_else(|| anyhow::anyhow!("no private key in backend.tls_key"))?;
        let versions: &[&rustls::SupportedProtocolVersion] =
            if config.common.security.external >= Level::Hardened {
                &[&rustls::version::TLS13]
            } else {
                rustls::DEFAULT_VERSIONS
            };
        let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_protocol_versions(versions)?
        .with_no_client_auth()
        .with_single_cert(chain, key)?;
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = tokio::net::TcpListener::bind(&section.listen).await?;
        info!(listen = %section.listen, tls = true, "serving regency.v1.Regency (gRPC and gRPC-Web)");
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let accept_cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, peer) = tokio::select! {
                    _ = accept_cancel.cancelled() => return,
                    accepted = listener.accept() => match accepted {
                        Ok(accepted) => accepted,
                        Err(e) => {
                            warn!(error = %e, "accept failed");
                            continue;
                        }
                    },
                };
                let (acceptor, tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await
                    {
                        Ok(Ok(stream)) => {
                            let _ = tx.send(Ok::<_, std::io::Error>(stream)).await;
                        }
                        Ok(Err(e)) => {
                            tracing::debug!(peer = %peer, error = %e, "TLS handshake failed")
                        }
                        Err(_) => tracing::debug!(peer = %peer, "TLS handshake timed out"),
                    }
                });
            }
        });
        server
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::ReceiverStream::new(rx),
                cancel.cancelled(),
            )
            .await?;
    } else {
        info!(listen = %section.listen, tls = false, offloaded = section.tls_offloaded, "serving regency.v1.Regency (gRPC and gRPC-Web)");
        server
            .serve_with_shutdown(section.listen.parse()?, cancel.cancelled())
            .await?;
    }
    info!("backend stopped");
    Ok(())
}
