# Proactive hosts (regops)

[regops](https://github.com/regent-project/regops) is a GitOps agent: it
pulls an expected state from Git, assesses or reaches it on its own host in
a loop, and can report every pass to regency's **endpoint**. The host then
appears in regency like any other, with its history.

## On the regops host

**Hosts → Add host → With regops** issues an enrollment
token for a host id and gives the commands writing the endpoint's CA
certificate and the token, and the configuration below, filled in (with
`server_name` when the endpoint's address is not in its certificate).

`/etc/regops/config.toml` (`C:\ProgramData\regops\config.toml` on Windows):

```toml
Reporting = "Regency"

[Regency]
endpoint = "https://regency.example.com:8443"
ca_cert_path = "/etc/regops/regency-ca.pem"   # CA of the endpoint's server certificate
host_id = "web-01"                            # default: the hostname
tls_mode = "BuiltInCa"                        # or ExternalCa, SelfSigned
enrollment_token_path = "/etc/regops/regency-enrollment.token"
labels = { env = "prod", kind = "web" }       # regops 0.17.0+
```

Outcomes wait in a local spool until the endpoint accepts them, so a
network outage loses nothing; the compliance loop never waits for the
network.

## In the fleet: labels, assignments, freshness

With each pass (regops 0.17.0 and later), the agent also reports:

- its **labels** (`labels` above): like a host definition's, they give the
  host the [assignments](assignments.md) whose selectors match them
  ([Labels](hosts.md#labels)). Change them in the configuration and the
  host moves at its next pass. If the host also has a definition in the
  catalog, both sets of labels apply (the definition's win on the same
  key), and its built-in `source` label includes `regops`;
- where its expected state comes from: its `[Git]` repository (without
  credentials), branch and file, the commit it applied, and the state
  itself.

On the map, a proactive host is a **diamond**, coloured by its own last
pass (compliant or not), whether or not an assignment selects it: it
checks the state of its own repository. Without an assignment it stays in
*Unassigned*.

**Repository freshness.** When its repository and branch are those of one
of regency's Git sources (see [Sources](sources.md); URLs compared without
scheme, credentials or `.git`), regency knows the latest commit and shows
the host as **up to date** or **behind** (an amber outline on the map): a
host that stays behind has trouble pulling (credentials, network, a broken
local copy). Otherwise its freshness is **unknown**: regency polls no other
repository.

**Drift.** When an assignment selects the host, regency compares the state
it applied with its [effective expected state](assignments.md) and
shows what differs (missing, different and extra attributes) on its panel
and page: a warning, not a colour. Until regops can pull its state from
regency, keep its repository in line with its assignments.

## Identities: three modes

| Mode | How a host gets its certificate | Security level |
|---|---|---|
| **External CA** | your PKI (Vault, step-ca, cert-manager…) issues it, SAN = host id | 1–3 |
| **Built-in CA** | an admin issues an **enrollment token** for the host id; regops enrolls once, then renews its short-lived certificate itself | 1–3 |
| **Self-signed** | regops generates it and logs its fingerprint; an admin **approves** it on the host page (pending until then) | 1–2 |

Enrollment tokens come from **Hosts → Add host** or the API
(`IssueEnrollmentToken`, bound to one host id, short expiry). With the
all-in-one image and the Helm chart, the built-in CA is generated for you;
the endpoint publishes its certificate (shown by **Add host**), to give
regops as `ca_cert_path`. The all-in-one's endpoint certificate names
`localhost`, `regency` and the machine's hostname, FQDN and IP addresses;
the agents' endpoint URL is the web UI's host unless **Settings → Endpoint
URL** says otherwise.

The endpoint refuses an outcome whose host id differs from the certificate.
Deleting a host's data also drops its pinned fingerprint.

## Passive and proactive at once

A host may run regops and also be defined in the catalog (reached by
runners): give regops the same `host_id`. regops and runners share the host lock, so a remediation from
one never overlaps work from the other.
