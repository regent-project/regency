# Configuration

Every service reads its settings from four layers, lowest first:

1. built-in **defaults**;
2. the **central configuration** — edited in the web UI (**Configuration →
   Services**), stored in CouchDB, shared by every instance of the service;
3. the local **file** — `/etc/regency/<service>.toml`
   (`%ProgramData%\regency\<service>.toml` on Windows; `--config` to change);
4. **environment variables** `REGENCY_<SECTION>_<KEY>`, e.g.
   `REGENCY_RABBITMQ_URL` sets `[rabbitmq] url`, `REGENCY_INSTANCE` sets the
   top-level `instance`.

A key set in the file or the environment is **pinned**: the central
configuration cannot change it, and the UI shows it as such. This keeps the
last word with whoever deploys the service (Helm values, Ansible, …).

## Local-only keys

What a service needs before it can read the central configuration, or what
identifies it, is never taken from it:

`[rabbitmq]`, `[couchdb]`, `[secrets]`, `[health]`, `[log]`, `[central]`,
`instance`, `security.internal`, and the runner's `runner.pool`.

## Editing the central configuration

**Configuration → Services → Central configuration**: pick a service (for
runners, optionally a pool), write TOML with the same sections as the file,
**Save**:

```toml
[scheduler]
command_prefetch = 16

[security]
external = 2
```

- backend validates the document against every live instance's own file
  and environment, and refuses it if any instance would end up invalid.
- Each instance checks it again locally (TLS files, secrets…). If valid, it
  waits a random delay (`[central] restart_jitter_sec`, default 30 s), stops
  gracefully and exits with status 3; systemd, Kubernetes, Windows or the
  all-in-one supervisor start it again with the new configuration. Replicas
  restart one after the other, not all at once.
- If invalid, the instance keeps running with its current configuration and
  shows the error in **Configuration → Services**.
- An empty document removes the central configuration.

`[central] enabled = false` in a service's file makes it ignore the central
configuration (and need no CouchDB access for it).

## Instances

Every instance reports its status every 30 seconds: version, regent-sdk
version, readiness, security levels, and its **effective configuration**
with the origin of every key (default, central, file, env). **Configuration
→ Services** lists them; an instance silent for 2 minutes is shown as gone,
and forgotten after 7 days.

## Secrets

Secret values never appear in configuration: only references, resolved by
the service's `[secrets]` providers.

```toml
[rabbitmq]
password = { SecRef = "/etc/regency/secrets/runner.rabbitmq" }        # a file
key = { SecRef = "RUNNER_TLS_KEY", Provider = "env" }                  # an environment variable
```

The default providers are `files` (the default) and `env`.

See [Service configuration keys](../reference/configuration.md) for every key.
