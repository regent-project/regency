# Introduction

**regency** is a fleet-scale control plane for configuration management. You
describe what your machines should look like (*expected states*), regency
checks them (**Assess**) and fixes them (**Reach**) on thousands of hosts,
records every outcome, and shows the live state of the fleet in a web UI.

It is built on [regent-sdk](https://github.com/regent-project/regent-sdk), a
Rust library for declarative configuration management, and on two proven
pieces of infrastructure: **RabbitMQ** distributes the work, **CouchDB**
keeps the state.

```text
          you (web UI, API)                 Git (hosts, expected states)
                  │                                     │
                  ▼                                     ▼
            ┌──────────┐   commands    ┌─────────────────────────────┐
            │ backend  │──────────────▶│ RabbitMQ                    │
            └──────────┘               │  tasks · outcomes · events  │
                  ▲ live views         └─────────────────────────────┘
                  │                       │            ▲          │
            ┌──────────┐            ┌──────────┐ ┌──────────┐ ┌──────────┐
            │ CouchDB  │◀───────────│ dbwriter │ │ runners  │ │scheduler │
            └──────────┘            └──────────┘ └──────────┘ └──────────┘
                                                      │ SSH / SSHOQ
                                                      ▼
                                              passive hosts
            regops agents on proactive hosts ──▶ endpoint ──▶ RabbitMQ
```

## What you get

- **Hosts selected by labels, the Kubernetes way.** No host lists to
  maintain. Each host carries labels, and each assignment selects its hosts
  with a label selector (`{role="web", env=~"prod|staging"}`), as a
  Kubernetes Service selects pods. Label a host `role=web` and it gets the
  web expected states. A host's assignments add up by priority, and
  regency reports conflicts between them (see
  [Assignments](operations/assignments.md)).
- **Two ways to manage a host.** *Passive* hosts are reached by regency's
  runners over SSH or SSHOQ (SSH over QUIC); nothing is installed on them.
  *Proactive* hosts run the small [regops](https://github.com/regent-project/regops)
  agent, which pulls its expected state from Git, applies it itself and
  reports to regency. Both kinds show up the same way.
- **GitOps or UI.** Hosts, expected states and assignments come from Git
  repositories (synced automatically), from the API, or from the web UI.
- **No single point of failure.** Every service runs as several replicas;
  RabbitMQ and CouchDB run as clusters.
- **Security that grows with you.** Four security levels, from *open* (try
  it, look inside) to *hardened* (mutual TLS everywhere, TLS 1.3, short-lived
  credentials), set per service, inside and outside.
- **Built for operations.** Structured JSON logs ready for Elasticsearch or
  Loki, Prometheus metrics, health endpoints, packages for Debian, Red Hat
  and Windows, container images, a Helm chart.

## How to read this book

- New to regency: read [Concepts](concepts/regent-sdk.md), then the
  [Quick start](getting-started/quick-start.md): one command starts a
  complete regency on your machine.
- Planning a production deployment: [Architecture](architecture/overview.md),
  then [Deployment](deployment/overview.md) and
  [Security levels](operations/security.md).
- Running regency day to day: [Operations](operations/configuration.md).

regency is open source (Apache-2.0).
