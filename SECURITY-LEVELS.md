# Security levels

Regency lets you start fully open, to try it and look inside, then raise
security link by link until production (decision D19). There are four levels,
with the same meaning everywhere:

| Level | Name | Meaning |
|---|---|---|
| 0 | open | nothing required: anonymous, clear text, nothing verified |
| 1 | authenticated | every principal (user, service, host) proves who it is; clear text tolerated; trust on first use allowed |
| 2 | encrypted | + every network link encrypted, every server identity verified (explicit CA, pins); no trust on first use, no insecure modes |
| 3 | hardened | + mutual authentication with keys/certificates instead of passwords where the protocol allows, TLS 1.3, short-lived credentials |

Every service takes **two levels at startup**:

```toml
[security]
internal = 1   # links to RabbitMQ (AMQP + stream) and CouchDB
external = 3   # what the service exposes or reaches outside
```

or `REGENCY_SECURITY_INTERNAL` / `REGENCY_SECURITY_EXTERNAL` (numbers or names:
`open`, `authenticated`, `encrypted`, `hardened`). So an all-in-one container
can be open inside and hardened outside.

**Both default to 3.** A service whose configuration is weaker than its
levels refuses to start (exit status 2) and lists every missing piece, e.g.:

```
refusing to start:
  configuration too weak for internal security level 2 (encrypted): rabbitmq.url must be amqps:// (TLS; the stream port then uses TLS too); rabbitmq.ca_file is required (explicit trust anchor; …)
```

Secrets are always secret references, at every level (D9): inventories and
tasks never carry plaintext passwords.

## Internal level (every service, and `regencyctl`)

| Level | Requires |
|---|---|
| 0 | nothing |
| 1 | `[rabbitmq] username` + `password`, not `guest`; `[couchdb] username` + `password` |
| 2 | + `[rabbitmq] url = "amqps://…"` (the stream port then uses TLS too) and `ca_file`; `[couchdb] url = "https://…"` and `ca_file`; for `regencyctl`, `rabbitmq_management_url = "https://…"` |
| 3 | + `cert_file` and `key` (client certificate, mutual TLS) in both sections; CouchDB is reached in TLS 1.3 only |

```toml
# level 3
[rabbitmq]
url = "amqps://rabbitmq.internal:5671"
stream_host = "rabbitmq.internal"
stream_port = 5551
username = "regency-runner"
password = { SecRef = "/run/secrets/rabbitmq-password" }
ca_file = "/etc/regency/tls/ca.pem"
cert_file = "/etc/regency/tls/runner.pem"
key = { SecRef = "/run/secrets/runner.key" }

[couchdb]
url = "https://couchdb.internal:6984"
username = "regency-backend"
password = { SecRef = "/run/secrets/couchdb-password" }
ca_file = "/etc/regency/tls/ca.pem"
cert_file = "/etc/regency/tls/backend.pem"
key = { SecRef = "/run/secrets/backend.key" }
```

On the broker side, for level 3: RabbitMQ `ssl_options.verify = verify_peer`,
`fail_if_no_peer_cert = true`, `ssl_options.versions.1 = tlsv1.3`, and the
TLS stream listener; CouchDB `[ssl] verify_ssl_certificates = true`,
`fail_if_no_peer_cert = true`. (The AMQP client cannot restrict TLS versions
itself: the broker enforces TLS 1.3.)

## External level

### backend (users, frontend, API clients)

| Level | Behaviour / requires |
|---|---|
| 0 | callers without credentials act as the bootstrap admin; plain HTTP/2 |
| 1 | authentication required (login sessions, API tokens, OIDC); Cedar authorization |
| 2 | + TLS: `[backend] tls_cert` + `tls_key`, or `tls_offloaded = true` behind a TLS-terminating proxy; OIDC issuers `https://` |
| 3 | + TLS 1.3 only (when backend terminates TLS); password login only for the bootstrap admin (break-glass), everyone else uses OIDC or API tokens; `session_ttl_sec <= 3600` |

### endpoint (proactive hosts running regops)

TLS is always on (regops requires it).

| Level | Behaviour / requires |
|---|---|
| 0 | any client certificate, unverified, no pinning (identity = certificate name) |
| 1 | a trust mode: external CA bundle, built-in CA, or pinned self-signed certificates; trust on first use allowed |
| 2 | + `pinning.trust_on_first_use = false`: an admin approves each self-signed fingerprint |
| 3 | CA-issued certificates only (external CA or built-in CA with `cert_lifetime_days <= 30`), self-signed pinning disabled; TLS 1.3 only |

### runner (passive hosts, checked for every task)

| Level | Allows |
|---|---|
| 0, 1 | any connection method |
| 2 | only verified host identities: SSH with a `HostKey` (known_hosts file or SHA-256 fingerprints), SSHOQ without `Insecure` TLS |
| 3 | + no password authentication: SSH keys, SSHOQ keys or bearer tokens |

SSH host key verification, in a host definition or an inventory's
connection method:

```yaml
DefaultConnectionMethod: !Ssh2
  AuthMethod: !Key
    Username: ops
    Key: { SecRef: ops_ed25519, Provider: files }
  HostKey: !KnownHostsFile /etc/regency/known_hosts   # or: !Fingerprints [SHA256:…]
```

A task refused by the runner's level is not run; its outcome records the
reason (`security level 2 (encrypted) forbids this host connection: …`).
Different runner pools can run at different levels (e.g. a lab pool at 1, a
production pool at 3).

### sourcesync (Git servers)

| Level | Allows |
|---|---|
| 0, 1 | any repository URL |
| 2, 3 | `https://` repositories (or local paths) |

scheduler and dbwriter have no external surface.

## Typical setups

- **Trying regency** (all-in-one image, laptop): `internal = 0`, `external = 0`.
- **All-in-one exposed to a network:** `internal = 0` (everything inside one
  container), `external = 3` for backend and endpoint.
- **Lab cluster:** `internal = 1`, `external = 1` (the `dev/` configurations).
- **Production:** `internal = 3`, `external = 3`.

## Limits

- The health/metrics port (`[health] listen`) is never authenticated: keep it
  on a private network.
- Multi-factor authentication is delegated to the OIDC provider.
- Git commit signature verification is not implemented.
