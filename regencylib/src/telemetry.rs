//! Health/metrics port (D15) and process helpers; logging is in [`crate::logging`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use prometheus::{Encoder, Registry, TextEncoder};
use tracing::{info, warn};

use crate::config::{HealthConfig, LogConfig};

/// Install structured logging; see [`crate::logging`]. `version` is the
/// service's own version (D20: `env!("CARGO_PKG_VERSION")` of its crate).
pub fn init_logging(config: &LogConfig, service: &str, version: &str, instance: &str) {
    crate::logging::init(config, service, version, instance);
}

/// Readiness of a process: a set of named checks, all of which must be OK.
/// Liveness (`/healthz`) is the process answering at all.
#[derive(Clone, Default)]
pub struct Health {
    checks: Arc<Mutex<BTreeMap<String, Result<(), String>>>>,
    registry: Registry,
}

impl Health {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a check, initially not ready.
    pub fn register(&self, name: &str) {
        self.checks
            .lock()
            .expect("health lock")
            .insert(name.to_string(), Err("starting".to_string()));
    }

    pub fn set(&self, name: &str, state: Result<(), String>) {
        let mut checks = self.checks.lock().expect("health lock");
        let changed = checks.get(name) != Some(&state);
        checks.insert(name.to_string(), state.clone());
        if changed {
            match state {
                Ok(()) => info!(check = name, "ready"),
                Err(reason) => warn!(check = name, reason, "not ready"),
            }
        }
    }

    pub fn ready(&self, name: &str) {
        self.set(name, Ok(()));
    }

    pub fn not_ready(&self, name: &str, reason: impl Into<String>) {
        self.set(name, Err(reason.into()));
    }

    pub fn is_ready(&self) -> bool {
        self.checks
            .lock()
            .expect("health lock")
            .values()
            .all(Result::is_ok)
    }

    /// Readiness checks: name → `ok` or the reason.
    pub fn checks(&self) -> BTreeMap<String, String> {
        self.checks
            .lock()
            .expect("health lock")
            .iter()
            .map(|(name, state)| {
                (
                    name.clone(),
                    match state {
                        Ok(()) => "ok".to_string(),
                        Err(reason) => reason.clone(),
                    },
                )
            })
            .collect()
    }

    fn report(&self) -> String {
        self.checks
            .lock()
            .expect("health lock")
            .iter()
            .map(|(name, state)| match state {
                Ok(()) => format!("{name}: ok\n"),
                Err(reason) => format!("{name}: {reason}\n"),
            })
            .collect()
    }

    /// Prometheus registry served on `/metrics`.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Serve `/healthz`, `/readyz` and `/metrics` until the process ends.
    pub async fn serve(self, config: HealthConfig) -> std::io::Result<()> {
        let ready = self.clone();
        let metrics = self.clone();
        let app = Router::new()
            .route("/healthz", get(|| async { "ok\n" }))
            .route(
                "/readyz",
                get(move || {
                    let health = ready.clone();
                    async move {
                        let status = if health.is_ready() {
                            StatusCode::OK
                        } else {
                            StatusCode::SERVICE_UNAVAILABLE
                        };
                        (status, health.report()).into_response()
                    }
                }),
            )
            .route(
                "/metrics",
                get(move || {
                    let health = metrics.clone();
                    async move {
                        let mut buffer = Vec::new();
                        let families = health.registry.gather();
                        match TextEncoder::new().encode(&families, &mut buffer) {
                            Ok(()) => (StatusCode::OK, buffer).into_response(),
                            Err(e) => {
                                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
                            }
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind(&config.listen).await?;
        info!(listen = %config.listen, "health and metrics port");
        axum::serve(listener, app).await
    }

    /// Spawn [`Health::serve`] in the background (logging a failure).
    pub fn spawn(&self, config: HealthConfig) {
        let health = self.clone();
        tokio::spawn(async move {
            if let Err(e) = health.serve(config).await {
                tracing::error!(error = %e, "health port failed");
            }
        });
    }
}

/// Counter registered in `health`'s registry.
pub fn counter(
    health: &Health,
    name: &str,
    help: &str,
    labels: &[&str],
) -> prometheus::IntCounterVec {
    let counter = prometheus::IntCounterVec::new(prometheus::Opts::new(name, help), labels)
        .expect("valid metric");
    let _ = health.registry().register(Box::new(counter.clone()));
    counter
}

/// Gauge registered in `health`'s registry.
pub fn gauge(health: &Health, name: &str, help: &str) -> prometheus::IntGauge {
    let gauge = prometheus::IntGauge::new(name, help).expect("valid metric");
    let _ = health.registry().register(Box::new(gauge.clone()));
    gauge
}

static STOP: std::sync::LazyLock<tokio_util::sync::CancellationToken> =
    std::sync::LazyLock::new(tokio_util::sync::CancellationToken::new);

/// Ask the process to stop gracefully, like SIGTERM (e.g. the Windows
/// Service Control Manager, D22).
pub fn request_shutdown() {
    STOP.cancel();
}

/// Resolves when SIGTERM or Ctrl-C is received, when a stop is requested
/// ([`request_shutdown`]), or when the service must restart to apply a new
/// central configuration (D21).
pub async fn shutdown_signal() {
    let restart = crate::central::restart_signal();
    tokio::select! {
        _ = os_signal() => {}
        _ = restart.cancelled() => {}
        _ = STOP.cancelled() => {}
    }
    info!("shutdown requested");
}

async fn os_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}
