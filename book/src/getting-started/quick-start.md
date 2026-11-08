# Quick start

One command starts a complete regency on your machine: the **all-in-one
image** (every regency service and the web UI in one container) plus
single-node RabbitMQ and CouchDB.

> This setup is for trying regency: one node, plain HTTP inside, generated
> secrets. See [Deployment](../deployment/overview.md) for production.

## Requirements

- Docker with the compose plugin
- ports 8080 (web UI and API) and 8443 (endpoint for regops agents) free

## Start

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
