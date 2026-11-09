# Quick start

One command gives you a complete regency on one machine, in either form:

- **on a Linux machine** (virtual machine, server): the installer script
  sets up RabbitMQ, CouchDB and the all-in-one package;
- **with Docker**: the **all-in-one image** (every regency service and the
  web UI in one container) plus single-node RabbitMQ and CouchDB.

> Both are for trying regency: one node, plain HTTP, generated secrets. See
> [Deployment](../deployment/overview.md) for production.

## On a Linux machine: one-line install

```bash
curl -fsSL https://github.com/regent-project/regency/releases/latest/download/install.sh | sh
```

Requirements: x86_64, systemd, and Debian 13+, Ubuntu 24.04+, RHEL 10 (or
Rocky, Alma, CentOS Stream, Oracle Linux 10) or Fedora 40+ (best effort:
CouchDB publishes no Fedora packages, its RHEL 10 ones are used); root or
sudo; ports 8080 and 8443 free.

The script:

1. installs RabbitMQ 4 (Team RabbitMQ's repositories) with the management
   and stream plugins, listening on localhost only, and an admin account
   `regency-admin`;
2. installs CouchDB 3 (Apache's repositories), single node, on localhost
   only, with an admin account;
3. installs the `regency-all-in-one` package of the release (checked
   against the release's `SHA256SUMS`), writes the admin credentials to
   `/etc/regency/all-in-one.env` (readable by root and the `regency`
   group; only `regencyctl init` uses them);
4. starts `regency-all-in-one` and waits until it is ready, then prints the
   web UI's address and the generated password of `admin` (shown once).

Options go after `sh -s --`:

```bash
curl -fsSL https://github.com/regent-project/regency/releases/latest/download/install.sh \
  | sh -s -- --version 0.3.0 --admin-password 'choose-a-password'
```

| Option | Environment variable | Meaning |
|---|---|---|
| `--version <x.y.z>` | `REGENCY_VERSION` | release to install (default: the latest) |
| `--package <file>` | `REGENCY_PACKAGE` | install this `regency-all-in-one` `.deb`/`.rpm` instead of downloading one |
| `--admin-password <pw>` | `REGENCY_ADMIN_PASSWORD` | initial password of `admin` (first install only) |
| `--no-start` | | install and configure only |

Running it again is safe: it keeps the generated credentials and upgrades
regency to the requested (or latest) release. A RabbitMQ or CouchDB already
installed on the machine is **reused, not reconfigured**: the script only
enables the RabbitMQ plugins and adds its own admin accounts (for CouchDB,
give an existing one with `COUCHDB_ADMIN_USER` / `COUCHDB_ADMIN_PASSWORD`
if you prefer).

Then continue with [Log in](#log-in): <http://your-machine:8080>.
Logs: `journalctl -u regency-all-in-one -f`.

To remove it:

```bash
sudo systemctl disable --now regency-all-in-one
sudo apt purge regency-all-in-one    # or: sudo dnf remove regency-all-in-one
sudo rm -rf /var/lib/regency /etc/regency
# RabbitMQ and CouchDB too, if nothing else uses them (their data stays in
# /var/lib/rabbitmq and /opt/couchdb/data unless you delete it):
sudo apt purge rabbitmq-server couchdb   # or: sudo dnf remove rabbitmq-server couchdb
```

## With Docker

Requirements:

- Docker with the compose plugin
- ports 8080 (web UI and API) and 8443 (endpoint for regops agents) free

Start:

```bash
git clone https://github.com/regent-project/regency.git
cd regency
REGENCY_ADMIN_PASSWORD='choose-a-password' \
  docker compose -f deploy/compose/quickstart.yaml up -d
```

To use an image you built yourself instead of the published one:

```bash
docker build -f deploy/docker/Dockerfile --target aio -t regency-aio:local .
REGENCY_AIO_IMAGE=regency-aio:local REGENCY_ADMIN_PASSWORD='choose-a-password' \
  docker compose -f deploy/compose/quickstart.yaml up -d
```

On first start, the container:

1. generates every service account password, the session and enrollment
   keys, and a built-in CA with a certificate for the endpoint (kept in the
   `regency` volume);
2. runs `regencyctl init`: RabbitMQ users, permissions and queues, CouchDB
   databases, users and security, the bootstrap admin;
3. starts every service under its own least-privilege account.

Follow it with `docker compose -f deploy/compose/quickstart.yaml logs -f regency`
(one JSON object per line). Without `REGENCY_ADMIN_PASSWORD`, a random
password is generated and printed **once** in these logs (`bootstrap-admin`).

## Log in

Open <http://localhost:8080> and log in as `admin`. The **Map** shows your
fleet — empty for now. Continue with [First steps](first-steps.md).

## Stop, reset

```bash
docker compose -f deploy/compose/quickstart.yaml down       # keeps data
docker compose -f deploy/compose/quickstart.yaml down -v    # deletes everything
```

## What runs inside

| Process | Health port (inside the container) |
|---|---|
| dbwriter | 9091 |
| scheduler | 9092 |
| runner (pool `default`) | 9093 |
| backend (+ web UI, port 8080) | 9094 |
| endpoint (port 8443) | 9095 |
| sourcesync | 9096 |

The services run at security level 1 (authenticated, no TLS) inside and
outside. Raise the outside level with `REGENCY_SECURITY_EXTERNAL` once you
put TLS in front (see [Security levels](../operations/security.md)).
