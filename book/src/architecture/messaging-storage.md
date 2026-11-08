# Messaging and storage

## RabbitMQ

| Exchange / stream | Carries | Consumed by |
|---|---|---|
| `regency.tasks` | tasks (JSON) | runners (`regency.tasks.<pool>.assess`, `regency.tasks.<pool>.<P>.<N>`) |
| `regency.outcomes` | outcomes (protobuf) | dbwriter |
| `regency.lifecycle` | job events (`PENDING`, `STARTED`, …) | dbwriter |
| `regency.commands` | commands (run, order, catalog, settings, users, policies, configuration…) | scheduler (`run`), dbwriter (the rest), sourcesync (`source.refresh`) |
| `regency.status` | instance statuses | dbwriter |
| `regency.dead` | dead letters | dbwriter (`regency.dead`), kept for inspection (`regency.dead.parked`) |
| stream `regency.control` | abort requests | runners |

- Every queue is a **quorum queue** (replicated). Publishers wait for the
  broker's confirmation and use `mandatory` (an unroutable message is an
  error, never silently dropped).
- Each service account may publish only on the exchanges it needs and
  consume only its own queues (least privilege, set by `regencyctl init`).
- A message that keeps failing is dead-lettered after a delivery limit; a
  recurring task that waited longer than its slot expires and is recorded
  as `EXPIRED`.

## CouchDB

| Database | Holds |
|---|---|
| `regency_catalog` | sources, inventories, host definitions, expected states |
| `regency_orders` | recurring orders |
| `regency_state` | hosts (existence and latest state), instance statuses, certificate pins |
| `regency_jobs_YYYYMM` | jobs (lifecycle), one database per month |
| `regency_outcomes_YYYYMM` | outcomes (history, with the original protobuf bytes) |
| `regency_users` | local accounts and API tokens (hashed) |
| `regency_policies` | Cedar policies |
| `regency_settings` | settings (retention, deny list) |
| `regency_config` | central service configuration |

Every database refuses writes from anyone but dbwriter. Monthly databases
make retention cheap: dropping a month is dropping a database.

## Message evolution

Outcomes are protobuf and keep unknown fields, so hosts running different
regent-sdk versions coexist. JSON messages carry a `schema_version` and only
change additively within a version. Tasks also carry the regent-sdk version
that built them: a runner with another SDK version refuses the task (the job
records why) instead of misreading it.
