# Services

Every service is a single binary (`regency-<service>`), configured by a TOML
file, environment variables and the [central configuration](../operations/configuration.md),
exposing `/healthz`, `/readyz` and Prometheus `/metrics` on a health port.

## scheduler

- Holds recurring **orders** (stored in CouchDB). Orders are spread over
  *shards* (256 by default); each shard is led by exactly one scheduler at a
  time, which fires the shard's orders. Leadership is an exclusive RabbitMQ
  queue: it disappears when its holder's connection does.
- Firing is aligned to wall-clock slots (`every 15 min` fires at :00, :15…),
  so two schedulers briefly leading the same shard produce the same task
  keys, and the duplicate is recorded once. Missed slots are never replayed.
- Expands **run now** commands from backend into one task per selected host.
- Re-checks the order's author against the current policies at every firing.
- Publishes tasks on `regency.tasks`, then a `PENDING` event.

## runner

- Consumes tasks of its **pool** (`default` unless configured): assessments
  from one shared queue, remediations from per-host partitions it owns, so a
  host never has two remediations in flight.
- Resolves secret references with **its own** secret providers: a runner
  pool can hold credentials no other pool has.
- Checks every task against its [security level](../operations/security.md)
  (e.g. at level 2, SSH hosts must declare their host key).
- Publishes `STARTED`, then the outcome; acknowledges the task only once the
  outcome is safely in RabbitMQ.
- Follows the `regency.control` stream to abort queued or running tasks.

## endpoint

- gRPC server for **regops** agents: `ReportOutcome`, `Enroll`,
  `RenewCertificate`, always over TLS with client certificates.
- Three trust modes, usable together: an **external CA** (your PKI), the
  **built-in CA** (enrollment tokens issued by an admin, short-lived
  certificates renewed automatically), and **pinned self-signed**
  certificates approved in the UI.
- Refuses an outcome whose host id differs from the certificate identity;
  forwards the host's bytes unchanged.

## sourcesync

- Polls each Git source at its interval (or on demand), reads
  `inventories/*.yaml` and `expected_states/*.yaml` under the source's path,
  and publishes the differences as catalog commands with the commit sha.
- Sources are sharded across replicas like the scheduler's orders.

## dbwriter

- Consumes outcomes, job events, dead-lettered tasks, commands and instance
  statuses, and writes them to CouchDB with deterministic merges (safe
  against concurrent writers and redeliveries).
- Runs **retention**: monthly databases of jobs and outcomes older than the
  retention setting are dropped (hosts are never removed by retention).

## backend

- The single API: gRPC for programs, gRPC-Web for browsers, and optionally
  the web client's files on the same port.
- Authentication: local accounts, API tokens, OIDC providers, and the
  bootstrap admin. Authorization: Cedar policies.
- Live views: the client opens a stream with a query (a host selection, a
  job, an order…); backend sends a snapshot, then every change.
- Commands are validated, authorized, then published; backend answers
  "accepted" and the result appears in the live views.
