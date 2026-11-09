# Packages (deb, rpm)

Every GitHub release (`v<version>`) has a `.deb` and an `.rpm` (amd64) of:

- each service: `regency-scheduler`, `regency-runner`, `regency-dbwriter`,
  `regency-backend`, `regency-endpoint`, `regency-sourcesync`;
- `regencyctl` and `regency-frontend` (the web client's files);
- `regency-all-in-one`: all of them on one host (see below).

They install on Debian 13, Ubuntu 24.04, RHEL 10, Fedora 40 and newer (glibc
2.39 or newer). RabbitMQ and CouchDB are not packaged: install them
first ([RabbitMQ and CouchDB](infrastructure.md)).

## All-in-one: regency on one host

The [one-line installer](../getting-started/quick-start.md#on-a-linux-machine-one-line-install)
does all of this, RabbitMQ and CouchDB included. By hand:

```bash
sudo apt install ./regency-all-in-one_0.3.0-1_amd64.deb    # or:
sudo dnf install ./regency-all-in-one-0.3.0-1.x86_64.rpm
```

It installs every binary, the web client and a ready configuration, run by
one systemd unit, `regency-all-in-one` (not highly available). Tell it
where RabbitMQ and CouchDB are, with admin credentials (used once, by
`regencyctl init`, to create regency's own accounts), in
`/etc/regency/all-in-one.env`, then:

```bash
sudo systemctl enable --now regency-all-in-one
journalctl -u regency-all-in-one -f
```

At first start it generates the services' passwords, keys, a built-in CA and
the endpoint's certificate in `/var/lib/regency`, runs `regencyctl init`
and starts every service. The web UI is on port 8080 (user `admin`; the
initial password is `REGENCY_ADMIN_PASSWORD` from the env file, or generated
and logged once). Proactive hosts reach the endpoint on port 8443. It runs at
security level 1; the configuration of each service is in
`/etc/regency/<service>.toml`.

`regency-all-in-one` conflicts with the per-service packages (same files).

## One package per service

```bash
sudo apt install ./regency-backend_0.3.0-1_amd64.deb ./regency-frontend_0.3.0-1_all.deb
sudo dnf install ./regency-runner-0.3.0-1.x86_64.rpm
```

A package installs:

| File | Purpose |
|---|---|
| `/usr/bin/regency-<service>` | the binary |
| `/lib/systemd/system/regency-<service>.service` | systemd unit (user `regency`, hardened) |
| `/etc/regency/<service>.toml` | configuration template (kept on upgrade) |

Services are **not started** at installation: their template is at security
level 3 and lists every key to fill (broker and database URLs, credentials,
TLS material). Edit it, then:

```bash
sudo systemctl enable --now regency-backend
journalctl -u regency-backend -f
```

A service refuses to start, listing what is missing, when its configuration
is weaker than its security levels; systemd does not restart it in a loop
for that (exit status 2). It **is** restarted after any other exit,
including the planned exit that applies a new central configuration.

### Secrets and TLS files

Keep secrets in files readable by the `regency` user only, for example:

```bash
sudo install -d -o regency -g regency -m 0700 /etc/regency/secrets /etc/regency/tls
```

and reference them in the configuration:
`password = { SecRef = "/etc/regency/secrets/backend.rabbitmq" }`.

### Initial setup

On one machine, with `regencyctl` installed and
`/etc/regency/regencyctl.toml` filled with **admin** credentials (RabbitMQ
and CouchDB) and the service accounts' passwords:

```bash
sudo regencyctl init
```

Services never get admin credentials; they use the accounts `init` creates.

### Web client

`regency-frontend` installs the web client in `/usr/share/regency/web`.
With `frontend_dir = "/usr/share/regency/web"` in `[backend]`, backend
serves it on its own port (no extra web server, no CORS).
