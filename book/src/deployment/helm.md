# Kubernetes (Helm)

```bash
helm install regency oci://ghcr.io/regent-project/charts/regency \
  --namespace regency --create-namespace
```

The chart deploys:

- every service as a Deployment (two replicas each by default) with
  readiness and liveness probes, a PodDisruptionBudget and anti-affinity;
- a 3-node RabbitMQ cluster and a 3-node CouchDB cluster (StatefulSets),
  unless you use external ones;
- the generated secrets (service account passwords, session and enrollment
  keys, bootstrap admin password, cluster cookies) and a built-in CA with
  the endpoint's certificate — kept across upgrades;
- `regencyctl init` as a post-install/post-upgrade hook Job;
- Services for backend (web UI and API) and the endpoint, and optionally an
  Ingress for backend.

## Main values

| Value | Default | Meaning |
|---|---|---|
| `services.<svc>.replicas` | 2 | replicas per service |
| `services.<svc>.tag` | release versions | image tag of each service |
| `services.<svc>.env` | `{}` | extra `REGENCY_<SECTION>_<KEY>` variables |
| `services.runner.pool` | `default` | runner pool |
| `services.runner.hostSecrets` | `""` | Secret with host credentials, mounted in `/etc/regency/host-secrets` |
| `services.endpoint.service.type` | `LoadBalancer` | how regops agents reach the endpoint (no TLS-terminating proxy: mTLS end to end) |
| `services.endpoint.tlsSecret`, `caSecret` | `""` | your own server certificate / CA instead of generated ones |
| `security.internal`, `security.external` | 1, 1 | security levels |
| `ingress.enabled`, `ingress.host`, `ingress.tlsSecret` | off | ingress to backend (sets `tls_offloaded` when TLS is on) |
| `adminPassword` | generated | initial password of `admin` |
| `existingSecret` | `""` | use your own Secret with every key instead of generated ones |
| `rabbitmq.enabled`, `couchdb.enabled` | true | bundled clusters; `false` + `*.external.*` for yours |

The initial admin password:

```bash
kubectl get secret regency-secrets -n regency -o jsonpath='{.data.bootstrap-admin}' | base64 -d
```

## External RabbitMQ and CouchDB

```yaml
rabbitmq:
  enabled: false
  external:
    url: amqps://rabbitmq.example.internal:5671
    streamHost: rabbitmq.example.internal
    streamPort: 5551
    managementUrl: https://rabbitmq.example.internal:15671
    adminUser: admin
couchdb:
  enabled: false
  external:
    url: https://couchdb.example.internal:6984
    adminUser: admin
```

Put the admin passwords (`rabbitmq-admin`, `couchdb-admin`) in the chart's
Secret (or in your `existingSecret`).

## Notes

- Instances are named after their pod; the web UI lists them under
  **Configuration → Services**.
- The security levels set by the chart are in each service's configuration
  file, so they appear as *pinned* in the UI; change them with Helm values.
- Kubernetes restarts a container that exits to apply a new central
  configuration, as for any exit.
