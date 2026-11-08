# Choosing a deployment

| Form | For | High availability |
|---|---|---|
| [All-in-one + compose](../getting-started/quick-start.md) | trying regency, labs | no |
| [Docker Compose](compose.md) | small installations on one or a few machines | partly |
| [Kubernetes (Helm)](helm.md) | production | yes |
| [All-in-one package (deb, rpm)](packages.md#all-in-one-regency-on-one-host) | one virtual machine or server, with your RabbitMQ and CouchDB | no |
| [Packages (deb, rpm)](packages.md#one-package-per-service) | virtual machines, bare metal | yes, with several machines |
| [Windows (MSI)](windows.md) | Windows servers | yes, with several machines |

Every form runs the same binaries with the same configuration keys, and can
be mixed (for example runners installed from packages in a DMZ, the rest on
Kubernetes).

## Before production

- RabbitMQ (≥ 3 nodes, quorum queues, streams) and CouchDB (≥ 3 nodes): see
  [RabbitMQ and CouchDB](infrastructure.md).
- TLS material: a CA for internal links, certificates for backend and the
  endpoint, client certificates at security level 3.
- Run `regencyctl init` once (and after upgrades); it is idempotent.
- Decide your [security levels](../operations/security.md): production
  should run at 3 inside and outside.
- Ship logs to your log platform and scrape `/metrics`
  ([Observability](../operations/observability.md)).
