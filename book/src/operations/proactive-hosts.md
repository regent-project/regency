# Proactive hosts (regops)

[regops](https://github.com/regent-project/regops) is a GitOps agent: it
pulls an expected state from Git, assesses or reaches it on its own host in
a loop, and can report every pass to regency's **endpoint**. The host then
appears in regency like any other, with its history.

## On the regops host

`/etc/regops/config.toml` (`C:\ProgramData\regops\config.toml` on Windows):

```toml
Reporting = "Regency"

[Regency]
endpoint = "https://regency.example.com:8443"
ca_cert_path = "/etc/regops/regency-ca.pem"   # CA of the endpoint's server certificate
host_id = "web-01"                            # default: the hostname
tls_mode = "BuiltInCa"                        # or ExternalCa, SelfSigned
enrollment_token_path = "/etc/regops/regency-enrollment.token"
```

Outcomes wait in a local spool until the endpoint accepts them, so a
network outage loses nothing; the compliance loop never waits for the
network.

## Identities: three modes

| Mode | How a host gets its certificate | Security level |
|---|---|---|
| **External CA** | your PKI (Vault, step-ca, cert-manager…) issues it, SAN = host id | 1–3 |
| **Built-in CA** | an admin issues an **enrollment token** for the host id; regops enrolls once, then renews its short-lived certificate itself | 1–3 |
| **Self-signed** | regops generates it and logs its fingerprint; an admin **approves** it on the host page (pending until then) | 1–2 |

Enrollment tokens come from the API (`IssueEnrollmentToken`, bound to one
host id, short expiry). With the all-in-one image and the Helm chart, the
built-in CA is generated for you; give regops its certificate (`ca.pem`) as
`ca_cert_path`.

The endpoint refuses an outcome whose host id differs from the certificate.
Deleting a host's data also drops its pinned fingerprint.

## Passive and proactive at once

A host may run regops and also be in an inventory: give regops the same
`host_id`. regops and runners share the host lock, so a remediation from
one never overlaps work from the other.
