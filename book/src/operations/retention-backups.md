# Retention and backups

## Retention

Jobs and outcomes live in **monthly** databases. `retention_months`
(default 3, **Configuration → Settings**) sets how many months are kept:
with 3, in November, August to November are kept. Every dbwriter runs the
cleanup daily; the current and previous months are never dropped.

Hosts are never removed by retention: a host whose last outcome is dropped
keeps its existence and "last seen" date, with an *unknown* state.

## What to back up

| Data | Where | Back up? |
|---|---|---|
| catalog (API/UI items), orders, users, policies, settings, central configuration | CouchDB | **yes** |
| hosts, jobs, outcomes | CouchDB | as needed (history) |
| Git-sourced items | your Git repositories | already versioned |
| secrets, TLS material, built-in CA | your secret store / files / Kubernetes Secrets | **yes** |
| RabbitMQ queues | RabbitMQ | no (transient work) |

Back up CouchDB with its usual tools (replication to a backup cluster, or
file-level snapshots of every node). Keep the built-in CA key: without it,
proactive hosts must enroll again.
