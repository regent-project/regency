# Sources (Git)

A **source** is where catalog items come from:

| Kind | Items edited in |
|---|---|
| `git` | the Git repository (read-only in regency) |
| `api` | the API |
| `frontend` | the web UI |

## Git repository layout

```text
<path>/
  inventories/
    web.yaml
    databases.yaml
  expected_states/
    base.yaml
    nginx.yaml
  assignments/
    base.yaml
    web.yaml
```

An inventory is a regent-sdk inventory, with regency's optional `Pool` (for
the whole inventory) and per-host `Labels` and `Pool`:

```yaml
Pool: dmz
DefaultConnectionMethod: !Ssh2
  AuthMethod: !Key
    Username: ops
    Key: { SecRef: /etc/regency/host-secrets/ops_ed25519, Provider: files }
  HostKey: !KnownHostsFile /etc/regency/known_hosts
GlobalVars:
  ntp_server: ntp.example.com
Hosts:
  - Id: web-01
    Endpoint: 10.0.1.11:22
    Labels: { env: prod, tier: front }
  - Id: web-02
    Endpoint: 10.0.1.12:22
    Labels: { env: prod, tier: front }
```

The inventory is named after its file (`web` for `inventories/web.yaml`)
unless it sets `Name`. Files end in `.yaml` or `.yml`; subdirectories are
ignored.

Expected states are regent-sdk expected states, named after their file
(`infra/nginx` for `expected_states/nginx.yaml` in source `infra`).

An assignment ([Assignments](assignments.md)) names an expected state, a
priority and its hosts; names without a `/` refer to this source:

```yaml
# assignments/web.yaml
ExpectedState: nginx          # or other-source/name
Priority: 100                 # default 0
Inventories: [web]            # any of Hosts, Inventories, Labels
Labels: { tier: front }       # (a host matching any of them is selected)
```

## Adding a source

**Configuration → Sources → New source**: id, repository URL, branch, path
inside the repository, poll interval, and the credentials as a secret
reference resolved by sourcesync (an access token). **Sync now** forces an
immediate synchronization.

sourcesync applies only the differences, tagged with the commit sha. A file
with errors is reported and never deletes what it defined before.

At security level 2 and above, sourcesync only accepts `https://`
repositories (or local paths).
