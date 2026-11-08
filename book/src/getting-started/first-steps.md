# First steps in the web UI

## 1. Add a host

**Configuration → Hosts → New host.** If there is no editable source yet,
click **Create source "ui"** first: hosts created in the UI belong to it.

| Field | Example |
|---|---|
| Host id | `web-01` (global id of the host) |
| Endpoint | `10.0.0.5:22` |
| Inventories | `web` |
| Labels | `env=prod, team=web` |
| Connection method | prefilled with an SSH template |

The connection method is regent-sdk JSON. Secrets are references only, for
example an SSH key file that the runner can read:

```json
{
  "Ssh2": {
    "AuthMethod": { "Key": { "Username": "ops", "Key": { "SecRef": "/etc/regency/host-secrets/ops_ed25519", "Provider": "files" } } },
    "HostKey": { "Fingerprints": ["SHA256:…"] }
  }
}
```

Get the fingerprint with `ssh-keyscan host | ssh-keygen -lf -`. In the
all-in-one image, mount host credentials into the container (for example
`/var/lib/regency/host-secrets`) and reference them by path.

## 2. Add an expected state

Expected states usually come from Git (see [Sources](../operations/sources.md)):
create a repository with `expected_states/base.yaml`, add it in
**Configuration → Sources**, and click **Sync now**.

## 3. Assess

Open the host (**Hosts** → `web-01`), choose the expected state, click
**Assess**. The job appears in **Jobs** (`PENDING` → `STARTED` →
`FINISHED`), the host's dot on the **Map** takes its color, and the host
page shows the remediations that would be needed and the outcome history.

**Reach** applies them. Both are recorded; nothing is lost if a service
restarts meanwhile.

## 4. Look inside

- **Configuration → Services**: every running instance, its version, health
  and effective configuration (where each value comes from).
- **Configuration → Security**: the security level of every service.
- RabbitMQ's management UI and CouchDB's Fauxton are not exposed by the
  quick start; add their ports to the compose file to explore the queues
  and documents.
