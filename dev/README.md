# Development stack

Single-node RabbitMQ (streams enabled) and CouchDB, then every service under
its own least-privilege account (D4, D8).

All at once: `dev/run_locally` starts RabbitMQ and CouchDB, builds the
services and the web client, runs `regencyctl init`, then every service, with
prefixed logs in the terminal and in `dev/logs/<instance>.log`; a service
exiting with status 3 (central configuration changed, D21) is restarted;
Ctrl-C stops them all. It runs a fleet-sized setup:

- **12 runners** (`runner-01` = `dev/runner.toml`, the others with their own
  instance name and health port 9121…; the last two in the `edge` pool,
  declared in `dev/regencyctl.toml`) and **3 dbwriters** (dbwriter-1 first:
  it declares `regency.status.dbwriter`);
- a **representative dataset** once backend is ready
  (`backend/examples/dataset.rs`, also runnable alone: `cargo run -p backend
  --example dataset -- --hosts N`): API source `dataset`, 3000 hosts in 12
  inventories (some hosts in two), labels `env`, `region`, `role`, `team`,
  expected states `baseline`, `web`, `db`, `edge` and their assignments
  (D23: `baseline` at priority 0 on every inventory but `bastion`, whose 30
  hosts stay unassigned; `web` (web + api), `db`, `edge` at 100 on top),
  5 recurring orders on those assignments (one disabled), then a first
  Assess of every host against its effective state. Every host is this machine
  (Localhost) with its own files under `/tmp/regency-dev/hosts/<host>/` and
  its own host lock (`/tmp/regency-dev/locks/`); each load rewrites the files
  with a fixed mix (about 73 % compliant, 24 % drifting); the 90 `legacy`
  hosts are unreachable SSH hosts (error outcomes); `edge` hosts are served
  by the `edge` runners only. The catalog is only rewritten when it changed.

Options: `--runners N`, `--dbwriters N`, `--hosts N`, `--no-dataset`,
`--release` (much faster under this load), `--no-frontend`, `--no-endpoint`
(the endpoint only starts when `dev/tls/` exists), `--down` (stop the
containers on exit, data kept), `--reset` (remove the containers and their
data first); `--help`. Note that `docker compose -f dev/compose.yaml down`
also loses the data (no named volumes); use `stop`.

Step by step, from `regency/`:

```bash
docker compose -f dev/compose.yaml up -d
cargo run -p regencyctl -- --config dev/regencyctl.toml init   # idempotent

cargo run -p dbwriter   -- --config dev/dbwriter.toml
cargo run -p runner     -- --config dev/runner.toml
cargo run -p scheduler  -- --config dev/scheduler.toml
cargo run -p backend    -- --config dev/backend.toml      # gRPC + gRPC-Web + web UI on 127.0.0.1:8080
cargo run -p sourcesync -- --config dev/sourcesync.toml
# endpoint: needs TLS material in dev/tls/ (see dev/endpoint.toml)

cargo run -p backend --example smoke   # logs in as admin, runs an Assess on this machine

# web UI: build the client once (backend serves frontend/dist), open http://127.0.0.1:8080
(cd frontend && trunk build --release)
```

- Logins: `admin` / content of `dev/secrets/bootstrap-admin`. Management UI:
  http://localhost:15672 (regency / regency); Fauxton: http://localhost:5984/_utils
  (admin / admin).
- `dev/secrets/` holds development-only credentials; never reuse them.
- Health/metrics ports: endpoint 9091, dbwriter 9092, runner 9093, scheduler
  9094, backend 9095, sourcesync 9096 (`/healthz`, `/readyz`, `/metrics`).
- Security levels (D19, `SECURITY-LEVELS.md`): the dev configs and tests run
  at `internal = 1`, `external = 1` (credentials, no TLS). Without a
  `[security]` section a service defaults to level 3 and refuses this plain
  stack.
- Logs are JSON lines by default; the dev configs set `[log] format = "text"`.
- Integration tests do not need this stack (they start containers with
  `testcontainers`), but can reuse it: `REGENCY_TEST_EXTERNAL=1 cargo test -- --include-ignored`.
