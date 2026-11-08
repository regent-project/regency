# Troubleshooting

| Symptom | Look at |
|---|---|
| a service exits at startup with "refusing to start" | the listed violations: its configuration is weaker than its [security levels](security.md) |
| a service is not ready | `/readyz` on its health port, or **Configuration → Services** |
| jobs stay `PENDING` | runners of the job's pool (`regency.tasks.<pool>.*` queues have consumers?), the runner's logs |
| a job ends in error "refused" | the runner's security level vs the host's connection method (e.g. SSH without `HostKey` at level 2) |
| a job ends in error "inline credentials" | the inventory holds a password instead of a secret reference |
| "host busy" | another remediation (or regops) held the host lock longer than the timeout |
| "task built with regent-sdk X" | scheduler and runners run different regent-sdk versions: finish the upgrade |
| a host is "conflicting" | two sources define it differently: align them |
| a regops host does not appear | the endpoint's logs (certificate identity, pending fingerprint), regops' spool |
| a central configuration is not applied | **Configuration → Services**: "config refused" shows the reason; keys set in files are pinned |
| messages in `regency.dead.parked` | messages that kept failing; the dbwriter logs say why |

Every log line of a job carries its `job_id`: search for it across all
services to follow the job end to end.
