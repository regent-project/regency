//! regency-aio (D15): entrypoint of the all-in-one image. Starts every
//! process of its configuration as a child (single-node RabbitMQ and CouchDB,
//! `regencyctl init`, every regency service), restarts crashed ones and stops
//! all on SIGTERM. **Not HA, not for production.**
//!
//! Logs: one stream of JSON lines on stdout (D15). Lines of regency services
//! are already JSON and pass through unchanged; any other line (RabbitMQ,
//! CouchDB, a panic) is wrapped as `{"timestamp", "level", "service":
//! "regency-aio", "process": <name>, "message": <line>}`.
//!
//! Configuration (`/etc/regency/aio.toml`): an optional `[prepare]` section
//! (secrets and TLS material generated at first start, see [`prepare`]), then
//! processes, started in order; a `oneshot` process is retried until it
//! succeeds and the following ones wait for it. `${VAR}` / `${VAR:-default}`
//! in `args` and `env` are taken from the supervisor's environment. Every
//! long-running process is restarted whenever it exits (crash, or a D21
//! configuration change).
//!
//! ```toml
//! [[process]]
//! name = "rabbitmq"
//! command = "rabbitmq-server"
//!
//! [[process]]
//! name = "init"
//! command = "regencyctl"
//! args = ["--config", "/etc/regency/regencyctl.toml", "init"]
//! oneshot = true
//!
//! [[process]]
//! name = "dbwriter"
//! command = "regency-dbwriter"
//! ```

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio_util_free::CancelFlag;

mod prepare;
use tracing::{error, info, warn};

mod tokio_util_free {
    //! Minimal shutdown flag (no extra dependency).
    use tokio::sync::watch;

    #[derive(Clone)]
    pub struct CancelFlag(watch::Sender<bool>, watch::Receiver<bool>);

    impl CancelFlag {
        pub fn new() -> Self {
            let (tx, rx) = watch::channel(false);
            Self(tx, rx)
        }
        pub fn cancel(&self) {
            self.0.send_replace(true);
        }
        pub fn is_cancelled(&self) -> bool {
            *self.1.borrow()
        }
        pub async fn cancelled(&self) {
            let mut rx = self.1.clone();
            let _ = rx.wait_for(|v| *v).await;
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    log: regencylib::config::LogConfig,
    /// Time given to children after SIGTERM before SIGKILL.
    #[serde(default = "default_grace")]
    grace_sec: u64,
    /// First-start preparation (secrets, TLS material).
    #[serde(default)]
    prepare: prepare::Prepare,
    process: Vec<ProcessConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessConfig {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Run to success (retried), then start the following processes.
    #[serde(default)]
    oneshot: bool,
    /// Variables of the supervisor's environment not passed to the process:
    /// those `[prepare] from_env` consumes (secrets; and a `REGENCY_*` name
    /// would read as a configuration override, e.g. `REGENCY_ADMIN_PASSWORD`
    /// as an unknown `[admin]` section).
    #[serde(skip)]
    env_remove: Vec<String>,
}

fn default_grace() -> u64 {
    30
}

#[derive(Parser)]
#[command(
    name = "regency-aio",
    version,
    about = "regency all-in-one supervisor (not for production)"
)]
struct Args {
    #[arg(short, long, default_value = "/etc/regency/aio.toml")]
    config: String,
}

/// Pass JSON lines through, wrap the others (see module docs).
fn normalize(process: &str, line: &str) -> Option<String> {
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('{')
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok_and(|v| v.is_object())
    {
        return Some(trimmed.to_string());
    }
    let level = {
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("error") || lower.contains("panicked") {
            "error"
        } else if lower.contains("warn") {
            "warn"
        } else {
            "info"
        }
    };
    Some(
        serde_json::json!({
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            "level": level,
            "service": "regency-aio",
            "instance": "aio",
            "version": env!("CARGO_PKG_VERSION"),
            "process": process,
            "message": trimmed,
        })
        .to_string(),
    )
}

async fn pump(process: String, stream: impl AsyncRead + Unpin, out: Arc<Mutex<tokio::io::Stdout>>) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(mut json) = normalize(&process, &line) {
            json.push('\n');
            let mut out = out.lock().await;
            let _ = out.write_all(json.as_bytes()).await;
            let _ = out.flush().await;
        }
    }
}

fn spawn(process: &ProcessConfig, out: &Arc<Mutex<tokio::io::Stdout>>) -> std::io::Result<Child> {
    // `${VAR}` / `${VAR:-default}` from the supervisor's environment
    let args: Vec<String> = process.args.iter().map(|a| prepare::expand(a)).collect();
    let env: Vec<(String, String)> = process
        .env
        .iter()
        .map(|(k, v)| (k.clone(), prepare::expand(v)))
        .collect();
    let mut command = Command::new(&process.command);
    for name in &process.env_remove {
        command.env_remove(name);
    }
    let mut child = command
        .args(&args)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(pump(process.name.clone(), stdout, out.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(pump(process.name.clone(), stderr, out.clone()));
    }
    Ok(child)
}

fn terminate(child: &Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: plain kill(2) on our own child's pid
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    // the all-in-one image is Linux only; elsewhere the child is killed
    // after the grace period
    #[cfg(not(unix))]
    let _ = child;
}

/// Keep one long-running process alive until shutdown.
async fn supervise(
    process: ProcessConfig,
    out: Arc<Mutex<tokio::io::Stdout>>,
    stop: CancelFlag,
    grace: Duration,
) {
    let mut backoff = Duration::from_secs(1);
    while !stop.is_cancelled() {
        let started = Instant::now();
        let mut child = match spawn(&process, &out) {
            Ok(child) => child,
            Err(e) => {
                error!(process = %process.name, command = %process.command, error = %e, "cannot start");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        info!(process = %process.name, pid = child.id(), "started");
        tokio::select! {
            status = child.wait() => {
                if started.elapsed() > Duration::from_secs(60) {
                    backoff = Duration::from_secs(1);
                }
                warn!(process = %process.name, status = ?status.ok(), restart_in_ms = backoff.as_millis() as u64, "exited, restarting");
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            _ = stop.cancelled() => {
                terminate(&child);
                match tokio::time::timeout(grace, child.wait()).await {
                    Ok(status) => info!(process = %process.name, status = ?status.ok(), "stopped"),
                    Err(_) => {
                        warn!(process = %process.name, "did not stop in time, killed");
                        let _ = child.kill().await;
                    }
                }
                return;
            }
        }
    }
}

/// Run a one-shot process until it succeeds (or shutdown).
async fn run_oneshot(
    process: &ProcessConfig,
    out: &Arc<Mutex<tokio::io::Stdout>>,
    stop: &CancelFlag,
) -> bool {
    let mut backoff = Duration::from_secs(2);
    while !stop.is_cancelled() {
        match spawn(process, out) {
            Ok(mut child) => {
                let status = tokio::select! {
                    status = child.wait() => status.ok(),
                    _ = stop.cancelled() => {
                        terminate(&child);
                        return false;
                    }
                };
                if status.is_some_and(|s| s.success()) {
                    info!(process = %process.name, "completed");
                    return true;
                }
                warn!(process = %process.name, status = ?status, retry_in_ms = backoff.as_millis() as u64, "failed, retrying");
            }
            Err(e) => error!(process = %process.name, error = %e, "cannot start"),
        }
        tokio::select! {
            _ = stop.cancelled() => return false,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
    false
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let config: Config = match std::fs::read_to_string(&args.config)
        .map_err(|e| e.to_string())
        .and_then(|raw| toml::from_str(&raw).map_err(|e| e.to_string()))
    {
        Ok(config) => config,
        Err(e) => {
            eprintln!("regency-aio: {}: {e}", args.config);
            std::process::exit(2);
        }
    };
    regencylib::logging::init(&config.log, "regency-aio", env!("CARGO_PKG_VERSION"), "aio");
    warn!("all-in-one mode: single node, no high availability, not for production");
    if let Err(e) = prepare::run(&config.prepare) {
        error!(error = %e, "first-start preparation failed");
        std::process::exit(2);
    }

    let out = Arc::new(Mutex::new(tokio::io::stdout()));
    let stop = CancelFlag::new();
    {
        let stop = stop.clone();
        tokio::spawn(async move {
            regencylib::telemetry::shutdown_signal().await;
            stop.cancel();
        });
    }
    let grace = Duration::from_secs(config.grace_sec);
    let mut running = Vec::new();
    let consumed: Vec<String> = config.prepare.from_env.values().cloned().collect();
    for mut process in config.process {
        process.env_remove = consumed.clone();
        if stop.is_cancelled() {
            break;
        }
        if process.oneshot {
            if !run_oneshot(&process, &out, &stop).await {
                break;
            }
        } else {
            running.push(tokio::spawn(supervise(
                process,
                out.clone(),
                stop.clone(),
                grace,
            )));
        }
    }
    stop.cancelled().await;
    // children stop in parallel (each supervisor forwards SIGTERM)
    for handle in running {
        let _ = handle.await;
    }
    info!("all processes stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Variables consumed by `[prepare] from_env` never reach the children.
    #[cfg(unix)]
    #[tokio::test]
    async fn consumed_variables_are_not_inherited() {
        // HOME: inherited from the test environment (no set_var needed); unlike
        // PATH, shells do not give it a default value
        assert!(std::env::var_os("HOME").is_some());
        let out = Arc::new(Mutex::new(tokio::io::stdout()));
        let mut process = ProcessConfig {
            name: "check".into(),
            command: "/bin/sh".into(),
            // not `${..}`: the supervisor expands that in args
            args: vec!["-c".into(), r#"test -z "$HOME""#.into()],
            env: BTreeMap::new(),
            oneshot: true,
            env_remove: vec![],
        };
        let inherited = spawn(&process, &out).unwrap().wait().await.unwrap();
        assert!(!inherited.success(), "HOME should be inherited by default");
        process.env_remove = vec!["HOME".into()];
        let removed = spawn(&process, &out).unwrap().wait().await.unwrap();
        assert!(removed.success(), "HOME should not be inherited");
    }

    #[test]
    fn json_passes_text_is_wrapped() {
        let json = r#"{"service":"runner","message":"x"}"#;
        assert_eq!(normalize("runner", json).unwrap(), json);
        let wrapped: serde_json::Value =
            serde_json::from_str(&normalize("couchdb", "[error] boom").unwrap()).unwrap();
        assert_eq!(wrapped["process"], "couchdb");
        assert_eq!(wrapped["level"], "error");
        assert_eq!(wrapped["service"], "regency-aio");
        assert!(normalize("x", "   ").is_none());
    }
}
