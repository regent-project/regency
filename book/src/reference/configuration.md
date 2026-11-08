# Service configuration keys

Keys marked *local* cannot come from the central configuration. Every key
can be set by `REGENCY_<SECTION>_<KEY>` (top-level keys: `REGENCY_<KEY>`).
Templates with every key: `packaging/config/<service>.toml` in the repository.

## Common to every service

| Key | Default | Meaning |
|---|---|---|
| `instance` *(local)* | hostname | instance name in logs, statuses, consumer tags |
| `[security] internal` *(local)* | 3 | level of the links to RabbitMQ and CouchDB |
| `[security] external` | 3 | level of what the service exposes or reaches |
| `[rabbitmq] url` *(local)* | `amqp://localhost:5672` | `amqp://` or `amqps://`, without credentials |
| `[rabbitmq] stream_host`, `stream_port` *(local)* | `localhost`, 5552 | stream endpoint (`regency.control`) |
| `[rabbitmq] vhost` *(local)* | `/` | |
| `[rabbitmq] username`, `password` *(local)* | — | the service account (password: secret reference) |
| `[rabbitmq] heartbeat_sec` *(local)* | 10 | AMQP heartbeat |
| `[rabbitmq] ca_file`, `cert_file`, `key` *(local)* | — | TLS trust anchor, client certificate and key (level 2/3) |
| `[couchdb] url`, `username`, `password`, `ca_file`, `cert_file`, `key` *(local)* | `http://localhost:5984` | same for CouchDB |
| `[health] listen` *(local)* | `0.0.0.0:9090` | `/healthz`, `/readyz`, `/metrics` |
| `[log] format` *(local)* | `json` | `json` or `text` |
| `[log] level` *(local)* | `info,…` | `tracing` directives (`RUST_LOG` overrides) |
| `[log] file` *(local)* | stdout | append the logs to this file instead |
| `[secrets] providers`, `default` *(local)* | `files`, `env` | secret providers (`kind = "files"` or `"env"`) |
| `[central] enabled` *(local)* | true | follow the central configuration |
| `[central] restart_jitter_sec` *(local)* | 30 | random delay before restarting for a new configuration |
| `[central] status_interval_sec` *(local)* | 30 | instance status period |

## scheduler

| Key | Default | Meaning |
|---|---|---|
| `[scheduler] shards` | 256 | order shards (part of the lock names) |
| `[scheduler] lock_retry_sec` | 5 | shard ownership round |
| `[scheduler] watchdog_timeout_sec` | 30 | a stalled firing loop drops every lock |
| `[scheduler] command_prefetch` | 8 | `run` commands expanded concurrently |
| `[scheduler] bootstrap_admin` | `admin` | always-authorized account |
| `[topology] …` | | see below |

## runner

| Key | Default | Meaning |
|---|---|---|
| `[runner] pool` *(local)* | `default` | pool served by this runner |
| `[runner] assess_concurrency` | 16 | parallel assessments |
| `[runner] reach_prefetch` | 4 | per owned partition |
| `[runner] control_lookback_sec` | 86400 | how far back aborts are read at start |
| `[runner] shutdown_grace_sec` | 60 | time left to running tasks on stop |
| `[runner] lock_retry_sec` | 5 | partition ownership round |

## dbwriter

| Key | Default | Meaning |
|---|---|---|
| `[dbwriter] prefetch` | 32 | messages in flight per queue |
| `[dbwriter] cleanup_interval_hours` | 24 | retention period |
| `[dbwriter] first_cleanup_delay_sec` | 60 | first retention run after start |

## backend

| Key | Default | Meaning |
|---|---|---|
| `[backend] listen` | `0.0.0.0:8080` | gRPC, gRPC-Web, web client |
| `[backend] tls_cert`, `tls_key` | — | terminate TLS (TLS 1.3 only at level 3) |
| `[backend] tls_offloaded` | false | TLS terminated by a proxy in front |
| `[backend] session_key` | required | signing key of sessions (≥ 32 bytes), shared by replicas |
| `[backend] session_ttl_sec` | 43200 | session lifetime (≤ 3600 at level 3) |
| `[backend] bootstrap_admin`, `bootstrap_password` | `admin`, — | bootstrap admin and its initial password |
| `[backend] enrollment_key` | — | key of enrollment tokens (shared with the endpoint) |
| `[backend] cors_origins` | `["*"]` | browsers allowed when the web client is served elsewhere |
| `[backend] frontend_dir` | — | serve the web client's files from this directory |
| `[backend] publish_wait_sec` | 5 | wait for RabbitMQ before answering UNAVAILABLE |
| `[[oidc]] issuer`, `audience`, `username_claim`, `groups_claim` | | OIDC providers |

## endpoint

| Key | Default | Meaning |
|---|---|---|
| `[endpoint] listen` | `0.0.0.0:8443` | gRPC over TLS |
| `[endpoint] cert`, `key` | required | server certificate and key |
| `[endpoint] external_ca_bundle` | — | external CA mode |
| `[endpoint] handshake_timeout_sec`, `publish_wait_sec` | 10, 5 | |
| `[ca] cert`, `key`, `enrollment_key`, `cert_lifetime_days` | —, —, —, 30 | built-in CA mode |
| `[pinning] enabled`, `trust_on_first_use` | false, false | self-signed mode |

## sourcesync

| Key | Default | Meaning |
|---|---|---|
| `[sourcesync] shards` | 64 | source shards |
| `[sourcesync] lock_retry_sec` | 5 | |
| `[sourcesync] workdir` | `/var/lib/regency/sourcesync` | bare clones |
| `[sourcesync] default_branch` | `main` | |

## Topology (scheduler, runner, dbwriter, regencyctl)

| Key | Default | Meaning |
|---|---|---|
| `[topology] reach_partitions` | 256 | Reach partitions per pool (part of queue names) |
| `[topology] delivery_limit` | 5 | deliveries before dead-lettering |
| `[topology] task_consumer_timeout_ms` | 14400000 | longest realistic task |
| `[topology] control_max_age_sec` | 172800 | retention of the abort stream |

Changing `shards` or `reach_partitions` is a rolling restart: the count is
part of the lock and queue names, and the same value must be used by every
instance (and by `regencyctl init` for the partitions).
