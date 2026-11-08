# Observability

## Logs

Every service writes **one JSON object per line** on stdout (or to
`[log] file`), designed to be shipped as is to Elasticsearch, Grafana Loki or
any log platform by a collector (Fluent Bit, Vector, Grafana Alloy, the
OpenTelemetry Collector…):

```json
{"timestamp":"2026-11-05T18:12:58.004Z","level":"INFO","service":"runner","instance":"runner-7f9c","version":"1.2.0",
 "target":"regency_runner::executor","message":"task finished","span":"task",
 "job_id":"run:ui-run-…:web-01","host_id":"web-01","job":"assess","pool":"default","duration_ms":4210,"compliance":"compliant"}
```

- Fixed keys: `timestamp`, `level`, `service`, `instance`, `version`,
  `target`, `message`, `span`.
- Ids are shared across services (`job_id`, `host_id`, `order_id`,
  `command_id`, `principal`…): one query follows a job from the scheduler
  through the runner to dbwriter.
- For Loki, use only `service`, `instance` and `level` as labels (low
  cardinality); query the rest as JSON fields.
- `[log] level` takes `tracing` directives (`info,lapin=warn`); `RUST_LOG`
  overrides it. `[log] format = "text"` is for local development.

## Metrics and health

Each service serves, on its health port (`[health] listen`):

| Path | Meaning |
|---|---|
| `/healthz` | the process answers (liveness) |
| `/readyz` | every dependency is ready (readiness); the body lists the checks |
| `/metrics` | Prometheus metrics (`regency_<service>_…`) |

| Metric | Service |
|---|---|
| `regency_scheduler_tasks_total`, `regency_scheduler_skips_total`, `regency_scheduler_shards` | scheduler |
| `regency_runner_tasks_total`, `regency_runner_running_tasks`, `regency_runner_reach_partitions` | runner |
| `regency_dbwriter_writes_total`, `regency_dbwriter_rejected_total` | dbwriter |
| `regency_endpoint_outcomes_total` | endpoint |
| `regency_sourcesync_syncs_total` | sourcesync |
| `regency_backend_rpcs_total` | backend |

The health port is never authenticated: keep it on a private network.

## In the web UI

**Configuration → Services** shows every instance's readiness checks,
version and configuration errors, refreshed every 10 seconds.
