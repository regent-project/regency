//! Structured logs (D15): one JSON object per line on stdout, for a log
//! collector (Loki promtail/alloy, Filebeat/Elastic Agent, Vector, Fluent Bit)
//! to ship to Grafana Loki, Elasticsearch, …
//!
//! Every line has the same shape in every service:
//!
//! ```json
//! {"timestamp":"2026-11-04T09:23:15.123456Z","level":"info",
//!  "service":"endpoint","instance":"endpoint-7f9c","version":"0.1.0",
//!  "target":"endpoint::service","message":"outcome accepted",
//!  "span":"report_outcome","host_id":"web-01","job_id":"rec:o1:3-a:5900"}
//! ```
//!
//! - fields of the enclosing spans are merged into the line (innermost span
//!   wins, then the event's own fields), so a `job_id` set once on a span is on
//!   every line logged inside it;
//! - fields are flat and typed (numbers stay numbers); an event field that
//!   would clash with a fixed key above is renamed `field_<name>`;
//! - use the shared field names of [`fields`] so that queries work across
//!   services (e.g. `{service=~".+"} | json | host_id="web-01"` in Loki, or
//!   `host_id:"web-01"` in Kibana).
//!
//! Low-cardinality keys (`service`, `instance`, `level`) suit Loki labels;
//! keep ids (`job_id`, `host_id`, …) as JSON fields, not labels.

use std::collections::BTreeMap;
use std::io::Write;

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::config::LogConfig;

/// Shared field names. Use them (with `tracing`'s `field = value` syntax)
/// rather than inventing synonyms.
pub mod fields {
    /// Job id = the task's idempotency key (D7).
    pub const JOB_ID: &str = "job_id";
    pub const HOST_ID: &str = "host_id";
    /// `assess` / `reach`.
    pub const JOB: &str = "job";
    /// Lifecycle state (`PENDING`, …).
    pub const STATE: &str = "state";
    pub const ORDER_ID: &str = "order_id";
    pub const COMMAND_ID: &str = "command_id";
    /// Command kind / routing key (`run`, `order.upsert`, …).
    pub const KIND: &str = "kind";
    /// Command origin (D10).
    pub const ORIGIN: &str = "origin";
    /// Principal (`User::"alice"`).
    pub const PRINCIPAL: &str = "principal";
    pub const POOL: &str = "pool";
    pub const SHARD: &str = "shard";
    pub const PARTITION: &str = "partition";
    pub const QUEUE: &str = "queue";
    pub const EXCHANGE: &str = "exchange";
    pub const ROUTING_KEY: &str = "routing_key";
    pub const DB: &str = "db";
    pub const DOC_ID: &str = "doc_id";
    pub const SOURCE_ID: &str = "source_id";
    pub const PEER: &str = "peer";
    pub const FINGERPRINT: &str = "fingerprint";
    /// gRPC method.
    pub const RPC: &str = "rpc";
    /// Duration of an operation, milliseconds.
    pub const DURATION_MS: &str = "duration_ms";
    /// Error text (`error = %e`).
    pub const ERROR: &str = "error";
}

const FIXED_KEYS: [&str; 9] = [
    "timestamp",
    "level",
    "service",
    "instance",
    "version",
    "target",
    "message",
    "span",
    "spans",
];

/// Install the global subscriber for `service` / `instance`. `RUST_LOG`
/// overrides `config.level`. `config.format = "text"` gives human-readable
/// lines for local development.
pub fn init(config: &LogConfig, service: &str, version: &str, instance: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));
    if let Some(path) = &config.file {
        match open_log_file(path) {
            Ok(file) => {
                let writer = std::sync::Mutex::new(file);
                let result = if config.format == "text" {
                    tracing_subscriber::registry()
                        .with(filter)
                        .with(
                            tracing_subscriber::fmt::layer()
                                .with_ansi(false)
                                .with_writer(writer),
                        )
                        .try_init()
                } else {
                    tracing_subscriber::registry()
                        .with(filter)
                        .with(JsonLayer::new(service, version, instance, writer))
                        .try_init()
                };
                let _ = result;
                return;
            }
            // fall back to stdout, and say why
            Err(e) => eprintln!("cannot open log file {path}: {e}; logging to stdout"),
        }
    }
    let result = if config.format == "text" {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .try_init()
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(JsonLayer::new(service, version, instance, std::io::stdout))
            .try_init()
    };
    // already installed: tests, or several services in one process
    let _ = result;
}

fn open_log_file(path: &str) -> std::io::Result<std::fs::File> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// The JSON-lines layer.
pub struct JsonLayer<W> {
    fixed: Map<String, Value>,
    make_writer: W,
}

impl<W> JsonLayer<W> {
    pub fn new(service: &str, version: &str, instance: &str, make_writer: W) -> Self {
        let mut fixed = Map::new();
        fixed.insert("service".into(), Value::from(service));
        fixed.insert("instance".into(), Value::from(instance));
        fixed.insert("version".into(), Value::from(version));
        Self { fixed, make_writer }
    }
}

/// Fields of a span, stored in its extensions.
struct SpanFields(BTreeMap<String, Value>);

struct Collector<'a>(&'a mut BTreeMap<String, Value>);

impl Collector<'_> {
    fn put(&mut self, field: &Field, value: Value) {
        self.0.insert(field.name().to_string(), value);
    }
}

impl Visit for Collector<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Value::from(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::from(value));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, Value::from(value));
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, Value::from(value.to_string()));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.put(field, Value::from(format!("{value:?}")));
    }
}

impl<S, W> Layer<S> for JsonLayer<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + 'static,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut values = BTreeMap::new();
        attrs.record(&mut Collector(&mut values));
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields(values));
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        record: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        if let Some(span) = ctx.span(id)
            && let Some(fields) = span.extensions_mut().get_mut::<SpanFields>()
        {
            record.record(&mut Collector(&mut fields.0));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut merged: BTreeMap<String, Value> = BTreeMap::new();
        let mut span_names = Vec::new();
        if let Some(scope) = ctx.event_scope(event) {
            // root first, so inner spans override outer ones
            for span in scope.from_root() {
                span_names.push(Value::from(span.name()));
                if let Some(fields) = span.extensions().get::<SpanFields>() {
                    merged.extend(fields.0.clone());
                }
            }
        }
        let mut own = BTreeMap::new();
        event.record(&mut Collector(&mut own));
        let message = own.remove("message").unwrap_or(Value::from(""));
        merged.extend(own);

        let metadata = event.metadata();
        let mut line = Map::new();
        line.insert(
            "timestamp".into(),
            Value::from(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)),
        );
        line.insert(
            "level".into(),
            Value::from(metadata.level().as_str().to_ascii_lowercase()),
        );
        line.extend(self.fixed.clone());
        line.insert("target".into(), Value::from(metadata.target()));
        line.insert("message".into(), message);
        if let Some(innermost) = span_names.last() {
            line.insert("span".into(), innermost.clone());
        }
        if span_names.len() > 1 {
            line.insert("spans".into(), Value::Array(span_names));
        }
        for (key, value) in merged {
            // `log` crate records forwarded by tracing-log
            if key.starts_with("log.") {
                continue;
            }
            if FIXED_KEYS.contains(&key.as_str()) {
                line.insert(format!("field_{key}"), value);
            } else {
                line.insert(key, value);
            }
        }
        if let Ok(mut bytes) = serde_json::to_vec(&Value::Object(line)) {
            bytes.push(b'\n');
            // one write per line, so concurrent lines never interleave
            let _ = self.make_writer.make_writer().write_all(&bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buffer {
        type Writer = Buffer;
        fn make_writer(&'a self) -> Buffer {
            self.clone()
        }
    }

    #[test]
    fn lines_are_flat_json_with_span_fields() {
        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::registry().with(JsonLayer::new(
            "runner",
            "1.2.3",
            "r-1",
            buffer.clone(),
        ));
        tracing::subscriber::with_default(subscriber, || {
            let outer = tracing::info_span!("task", job_id = "k1", host_id = "web-01");
            let _o = outer.enter();
            let inner = tracing::info_span!("run", attempt = 2, host_id = "web-02");
            let _i = inner.enter();
            tracing::warn!(
                duration_ms = 1500u64,
                service = "spoof",
                error = "boom",
                "task finished"
            );
        });
        let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.lines().count(), 1);
        let line: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(line["level"], "warn");
        assert_eq!(line["service"], "runner");
        assert_eq!(line["field_service"], "spoof");
        assert_eq!(line["instance"], "r-1");
        assert_eq!(line["message"], "task finished");
        assert_eq!(line["job_id"], "k1");
        assert_eq!(line["host_id"], "web-02", "innermost span wins");
        assert_eq!(line["attempt"], 2);
        assert_eq!(line["duration_ms"], 1500);
        assert_eq!(line["span"], "run");
        assert_eq!(line["spans"], serde_json::json!(["task", "run"]));
        assert!(line["timestamp"].as_str().unwrap().ends_with('Z'));
    }
}
