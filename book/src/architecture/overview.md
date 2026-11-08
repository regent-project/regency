# Architecture overview

```text
commands (UI/API) → backend → RabbitMQ[commands] → scheduler / dbwriter
recurring orders (CouchDB) → scheduler → RabbitMQ[tasks] → runner ⇄ passive hosts (SSH/SSHOQ)
                                                              ↓
proactive hosts (regops) → gRPC → endpoint → RabbitMQ[outcomes] → dbwriter → CouchDB
                                                                                ↓ _changes
                                                  web UI ⇐ gRPC-Web stream ⇐ backend (filters)
```

Three rules shape everything:

1. **RabbitMQ carries all the work.** Services never call each other; they
   publish messages to exchanges and consume from replicated (quorum)
   queues. A message is acknowledged only after its effect is durable.
2. **dbwriter is the only writer of CouchDB.** Every change of state —
   outcomes, job events, catalog edits, settings, users, policies,
   configuration — goes through dbwriter as a message. Everyone else only
   reads.
3. **The browser only talks to backend.** backend follows CouchDB's change
   feeds, filters what each user may see, and streams it; commands from the
   UI become messages.

## Components

| Component | Role | State |
|---|---|---|
| [scheduler](services.md#scheduler) | fires recurring orders, expands "run now" commands into tasks | none |
| [runner](services.md#runner) | runs tasks against passive hosts, publishes outcomes | none |
| [endpoint](services.md#endpoint) | gRPC gateway of proactive hosts (regops), mTLS | none |
| [sourcesync](services.md#sourcesync) | syncs Git sources into the catalog | Git clones (cache) |
| [dbwriter](services.md#dbwriter) | writes everything to CouchDB, retention | none |
| [backend](services.md#backend) | API (gRPC, gRPC-Web), authentication, authorization, live views, web client | none |
| web client | Leptos single-page application (WebAssembly), served by backend or any web server | — |
| regencyctl | admin CLI: `init` sets up RabbitMQ and CouchDB | — |
| regency-aio | supervisor of the all-in-one image | — |
| RabbitMQ | exchanges, quorum queues, the `regency.control` stream | cluster |
| CouchDB | catalog, orders, jobs, outcomes, hosts, users, policies, settings, configuration | cluster |

All services are written in Rust and share one library (`regencylib`):
message formats, the CouchDB model, the RabbitMQ topology, shard ownership,
security checks, configuration and logging.
