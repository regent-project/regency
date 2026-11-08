# regent-sdk: core principles

regency does not invent its own configuration language: it runs
[regent-sdk](https://github.com/regent-project/regent-sdk), a Rust library
("configuration management as a library"). Everything regency does to a host
is a regent-sdk operation.

## Expected states and attributes

An **expected state** describes what a host should look like. It is a list of
**attributes**, each one a building block with its own typed parameters:

| Category | Attributes (examples) |
|---|---|
| system | services, users, groups, cron, hostname, storage |
| package | apt, yum/dnf, pacman, repositories |
| utilities | line in file, templates (Tera), ping, debug |
| network | iptables, DNS, DHCP |
| shell | commands |
| containers, GPU, observability, AI | Docker/Podman/Apptainer, NVIDIA drivers, Fluent Bit, OpenTelemetry Collector, Grafana Alloy, Ollama… |

Expected states are written in YAML (or built in Rust). YAML uses tags for
variants (`!Present`, `!Raw`…) and PascalCase keys:

```yaml
Attributes:
  - Name: nginx installed
    Privilege: !WithSudo
    Detail: !Apt
      Package: nginx
      State: Present
  - Name: SSH root login disabled
    Privilege: !WithSudo
    Detail: !LineInFile
      FilePath: /etc/ssh/sshd_config
      Line: !Raw PermitRootLogin no
      State: !Present
        Position: !InsertAfter EOF
```

## Assess and Reach

regent-sdk works in two modes:

- **Assess** — a read-only compliance check. Each attribute compares the host
  with its expected state and lists the *remediations* that would be needed.
  Nothing is changed.
- **Reach** — assess, then apply the remediations, and verify them. A host
  that is already compliant is left untouched.

Operations are **idempotent** wherever possible: reaching a compliant host
twice does nothing the second time. This is what lets regency retry safely.

## Hosts and connections

A host is reached through a **connection method**:

| Method | Use |
|---|---|
| `Localhost` | the machine running regent-sdk (regops uses it) |
| `Ssh2` | SSH (password or key; host key verification with `HostKey`) |
| `Sshoq` | SSH over QUIC/HTTP3 |

Windows hosts are supported for a growing set of attributes (services, users,
local groups, hostname, commands, files and templates) with the SDK's
`windows` feature.

## Secrets are references

Credentials never appear in expected states or inventories: they are
**secret references** (`{ SecRef: ops_key, Provider: files }`) resolved by a
*secret provider* where the work runs (files, environment variables, AWS
Secrets Manager, Google Secret Manager…). regency enforces this: anything
carrying a plaintext credential is refused.

## Tasks and outcomes

A **task** (`RegentTask`) bundles one host, one expected state and a mode
(Assess or Reach). It can be serialized, sent to a worker and run there; it
always returns an **outcome** (`RegentTaskResult`): the compliance status,
the remediations or actions done, and any error. regency is, at heart, a
distributed system that moves tasks to runners and outcomes back to a
database.

## The host lock

On any host, several assessments may run at once, but a remediation runs
alone. regent-sdk enforces it with a lock taken **on the host itself**
(`flock` on Linux, a lock file on Windows), shared by every regent process —
regency runners and the regops agent alike. A busy host makes the task fail
with "host busy", which is recorded like any other outcome.
