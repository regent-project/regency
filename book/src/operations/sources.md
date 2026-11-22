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
  hosts/
    web.yaml
    databases.yaml
  expected_states/
    base.yaml
    nginx.yaml
  assignments/
    base.yaml
    web.yaml
```

A hosts file is a regent-sdk inventory file (the format `reg` reads), with
regency's optional `Labels` and `Pool`, for the whole file and on each
host. Values at the top of the file (connection method, variables, labels,
pool) are copied into each of its hosts; a host's own values win. The
file name means nothing.

```yaml
# hosts/web.yaml
Pool: dmz
Labels: { role: web }
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
    Labels: { env: prod }
  - Id: web-02
    Endpoint: 10.0.1.12:22
    Labels: { env: staging }
```

Expected states are regent-sdk expected states, named after their file
(`infra/nginx` for `expected_states/nginx.yaml` in source `infra`).

An assignment ([Assignments](assignments.md)) gives the hosts its label
selector matches ([Labels](hosts.md#labels)) a stack of expected states,
base first, each one overriding the previous ones; names without a `/`
refer to this source. A host gets every assignment matching it, applied by
`Priority` (lower first, default 0), then by name:

```yaml
# assignments/web.yaml
Selector: '{role="web", env=~"prod|staging"}'   # '{}' = every host
Priority: 10
ExpectedStates:               # base first, the last wins
  - nginx
  - nginx-prod                # or other-source/name
# ExpectedState: nginx        # shorthand for a single state
```

An assignment with the same `Priority` and name as another source's
cannot be ordered against it: it is reported as an error and not synced
until one of them changes.

Files end in `.yaml` or `.yml`; subdirectories are ignored. Files in a
former layout (hosts listed in `inventories/`, an `inventories/` folder of
selectors, assignments with `Inventory` or `Hosts`) are reported as
errors, and like any file error they never delete what the source defined
before: move the hosts to `hosts/`, then put each inventory's selector in
the assignments using it (`Selector:`) and delete `inventories/`.

## Adding a source

**Administration → Sources → New source**: id, repository URL, branch, path
inside the repository, poll interval, and the credentials as a secret
reference resolved by sourcesync (an access token). **Sync now** forces an
immediate synchronization.

sourcesync applies only the differences, tagged with the commit sha. A file
with errors is reported and never deletes what it defined before.

At security level 2 and above, sourcesync only accepts `https://`
repositories (or local paths).
