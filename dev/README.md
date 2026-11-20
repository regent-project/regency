# Development stack

Two ways to run regency while developing:

- **`dev/run_homelab`**: your build runs on a dedicated machine (the
  homelab, all-in-one package with its own RabbitMQ and CouchDB) and the
  dataset's managed hosts are this machine, through the real network: two
  thirds reached over SSH by the homelab's runner, one third as regops agents
  reporting to its endpoint (see [On the homelab](#on-the-homelab-devrun_homelab));
- **`dev/run_managed`**: the same, but the managed hosts live on a second
  machine: regency on "inference", every simulated host (SSH and regops
  agents) on "managed", this machine only builds, deploys and browses (see
  [Three machines](#three-machines-devrun_managed));
- **`dev/run_locally`**: everything on this machine (below).

Locally: single-node RabbitMQ (streams enabled) and CouchDB, then every
service under its own least-privilege account (D4, D8).

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

## On the homelab: `dev/run_homelab`

regency runs on a dedicated machine (the homelab) and the managed hosts are
this machine, through the real network, instead of everything on localhost:
**two thirds passive** (the homelab's runner reaches them over SSH) and **one
third proactive** (a regops agent each, running here and reporting to the
homelab's endpoint, D5). `dev/run_homelab`:

1. builds the release binaries, the web client and the `regency-all-in-one`
   .deb of this checkout (`--jobs 2` by default: lto needs memory);
2. deploys it with **reg** (inventory of the homelab, by default
   `../homelab/homelab_inventory.yaml` next to `regency/`, or `--inventory`):
   the package and `install.sh` are served to the homelab by a temporary HTTP
   server on this machine (port 8765), the package is replaced even at the
   same version, then `install.sh --package` installs RabbitMQ and CouchDB
   the first time, or resumes;
3. has the homelab generate an SSH key for its runner
   (`/var/lib/regency/secrets/dev-hosts-ssh-key`, never leaves it) and
   authorizes it in your `~/.ssh/authorized_keys`, from the homelab's address
   only, without pty or forwarding (marker comment `regency-dev-hosts`);
4. loads the dataset into the homelab's regency (`--hosts`, default 300, of
   which `--agents`, default a third, are regops agents): the passive hosts
   are reached as your user over SSH at this machine's address, with your
   sshd's host key fingerprints (checked by the runner), their files under
   `/tmp/regency-dev/` here, every host in the `default` pool (the all-in-one
   has one runner, no `edge` pool);
5. runs the regops agents here (`../regops` built in release, or
   `--regops FILE`), one process per proactive host `regops-<region>-NNNN`,
   until Ctrl-C. Each has its configuration, Git clone, spool and
   certificate under `/tmp/regency-dev/agents/<host>/`, enrolls once with a
   token of the homelab's built-in CA (D13; issued by the dataset loader
   through backend, `IssueEnrollmentToken`) and trusts the endpoint's CA
   (fetched with reg); `server_name` is `regency`, a name of the endpoint's
   certificate. Their expected states (baseline, plus web or db for two
   thirds) come from the local Git repository `/tmp/regency-dev/agents-repo`
   (regops is GitOps; the script commits it); one agent in four runs in
   `Enforce` mode (Reach), the others in `Assess`, every 60 s, live view on.
   Logs in `dev/logs/agents/<host>.log`.

The agents' hosts are **not in the catalog**: a catalog host would also get
tasks from the scheduler, and regops cannot take its expected state from
regency yet (D23's regops side, waiting for a regent-sdk release). So they
show as *unassigned* (grey) on the map, with their outcomes, live view and
logs on their host pages and in Jobs.

Once, on this machine:

- sshd running and reachable from the homelab, with
  `dev/homelab/sshd-regency-dev.conf` installed (the runner opens many
  connections at once: sshd's default `MaxStartups` refuses part of them):
  instructions in the file;
- the web UI's admin password in `dev/secrets/homelab-admin` (the initial
  one is in `/var/lib/regency/secrets/bootstrap-admin` on the homelab);
- `reg`, `cargo-deb` and `trunk` installed.

Options: `--hosts N`, `--agents N` (0: SSH only), `--regops FILE`,
`--no-build` (deploy the last package, `target/debian/`),
`--no-deploy` (keep what the homelab runs), `--no-dataset`, `--homelab ADDR`,
`--privilege WithSudo|WithSudoRs` (default `WithSudoRs`, Ubuntu 25.10+), `--port`,
`--help`. Outputs in `dev/logs/` (`homelab-deploy.log`, `dataset.log`,
`agents/`);
services' logs on the homelab: `journalctl -u regency-all-in-one -f`.

Security: whoever can run jobs in the homelab's regency (its `admin`, a
one-shot attribute) can run commands as your user on this machine. Use a
dedicated account for the SSH user if that matters (the dataset files must
then be writable by it).

## Three machines: `dev/run_managed`

Like `dev/run_homelab`, but with the managed hosts on a machine of their own:

| Machine | Default | Runs |
|---|---|---|
| inference | `192.168.1.40` (Endpoint of the inventory, `--inference`) | regency all-in-one, RabbitMQ, CouchDB |
| managed | `192.168.1.50` (`--managed`, `REGENCY_MANAGED`) | the dataset's hosts: SSH target of inference's runner, regops agents |
| this machine | | build, deploy (reg), web browser on `http://<inference>:8080` |

`dev/run_managed` builds and deploys to inference exactly like
`dev/run_homelab`, then:

- fetches inference's runner key and CA every time (so `--no-deploy` works
  with a fresh managed machine) and authorizes the key in
  `~/.ssh/authorized_keys` of the managed user on "managed" (from inference's
  address only, marker `regency-dev-hosts`);
- copies the dataset loader and regops to `/tmp/regency-dev/bin/` on
  "managed" and **runs the loader there**, so every file the dataset writes
  (hosts' files, locks, agents' configurations, enrollment tokens, their Git
  repository) is under `/tmp/regency-dev/` on "managed". The admin password
  goes over SSH's stdin;
- starts the regops agents on "managed" through one SSH session (logs there,
  `/tmp/regency-dev/logs/agents/<host>.log`); Ctrl-C, or the session
  ending, stops them.

Once:

- SSH key access from this machine to `<managed-user>@<managed>`
  (`ssh-copy-id`; `--managed-user`, default `managed`);
- on "managed": `git`, `flock` (util-linux), a glibc at least as recent as
  this machine's (binaries are built here; the script checks with `ldd`),
  and `dev/homelab/sshd-regency-dev.conf` installed with inference's address
  (the script prints the commands if it is missing);
- `dev/secrets/homelab-admin` (inference's admin password), `reg`,
  `cargo-deb`, `trunk`, as for `dev/run_homelab`.

Options: those of `dev/run_homelab` (`--hosts`, `--agents`,
`--regops`, `--no-build`, `--no-deploy`, `--no-dataset`, `--privilege`,
`--port`, `--jobs`; `--inference` instead of `--homelab`), plus
`--managed ADDR`, `--managed-user USER`, `--no-agents`, `--wipe`,
`--capacity` and `--no-capacity`.

**Sizing.** Before wiping or deploying anything, the script measures what
"managed" can run, and without `--hosts` the dataset gets that size; with
`--hosts` above it, it warns. `--capacity` only measures and prints, and
`--no-capacity` skips the measurement (then `--hosts` defaults to 1000). It
takes about a minute and measures:

- **a regops agent:** one real agent runs standalone for 30 s (5 s passes)
  on a host like the dataset's. The script reads its peak memory, threads,
  CPU per pass, disk use and log growth;
- **a passive host:** 20 full SSH connections like the runner's (host lock
  plus a few commands) give the CPU per session. 8 sessions held open give
  the memory per open session;
- **the machine:** available memory, CPUs, free space and inodes on `/tmp`
  (counted as memory when it is a tmpfs), and the user's task limit
  (`ulimit -u`, systemd `TasksMax`).

These costs go against budgets of 80 % of available memory (minus 256 MiB
and 16 open runner sessions), 60 % of the CPUs and 80 % of `/tmp`. The
assumptions are written at the top of `capacity_report`:

- agents report to regency (TLS, gRPC, live view), counted as 1.5 × the
  standalone probe;
- agents pass once a minute;
- passive hosts get the dataset's order rates, about 10.8 SSH sessions an
  hour each;
- agents' logs are kept for one day.

The result is the most hosts with a third of agents (or, with `--agents
N`, the most passive hosts besides them) and the limiting resource. It also
prints how many passive hosts inference's runner can keep up with, as
information.

`--wipe` starts from scratch, after you type inference's address to confirm
(`--yes` skips that). On inference it purges the `regency*`,
`rabbitmq-server` and `couchdb` packages and deletes everything they leave
behind: `/var/lib/regency`, `/etc/regency`, `/var/lib/rabbitmq`,
`/etc/rabbitmq`, `/opt/couchdb`, `/var/lib/couchdb`, their logs and system
users. Erlang and the apt repositories stay. On "managed" it deletes
`/tmp/regency-dev`, because the agents' certificates were signed by the old
CA. Then it installs again. The installer generates a new admin password,
which is fetched into `dev/secrets/homelab-admin` (also fetched whenever
that file is missing):

```bash
dev/run_managed --wipe
``` Outputs here:
`dev/logs/inference-deploy.log`, `inference-access.log`, `dataset.log`.

Security: whoever can run jobs in inference's regency can run commands as
the managed user on "managed": use a dedicated account there
(`--managed-user`) if that matters.

## On this machine: `dev/run_locally`, step by step

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
