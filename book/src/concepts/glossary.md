# Glossary

| Term | Meaning |
|---|---|
| **Assess** | read-only compliance check of a host against an expected state |
| **Reach** | assess, then remediate what differs (called *enforce* in the CLIs) |
| **Attribute** | one building block of an expected state (a package, a file line, a service…) |
| **Expected state** | the desired configuration of a host: a list of attributes |
| **Host definition** | one host of the catalog: id, endpoint, connection method, labels, runner pool, variables |
| **Label** | a `key=value` pair on a host; built in: `host` and `source`. Hosts are selected by labels, like Loki streams or Kubernetes pods |
| **Label selector** | which hosts something applies to: `{env="prod", role=~"web\|api"}`; `{}` = every host |
| **Catalog** | every source, host definition, expected state and assignment known to regency |
| **Source** | where catalog items come from: a Git repository, the API, or the web UI |
| **Assignment** | a label selector + a priority + a stack of expected states (base first, the last wins): what the hosts it selects should look like |
| **Effective expected state** | a host's expected state: the final states of every assignment selecting it, added up by priority (an attribute two of them set differently is a conflict) |
| **Task / job** | one host + its effective expected state (or a one-shot attribute) + a mode; its id is the idempotency key |
| **Outcome** | the result of a task: compliance status, remediations or actions, error |
| **Order** | a recurring rule: on a schedule, Assess or Reach the hosts of an assignment |
| **Run** | an immediate, one-off order (from the UI or the API) |
| **Host selector** | host ids, assignments, `label=value` matches and a label selector, resolved when a run fires (orders run an assignment's hosts) |
| **Runner pool** | a group of runners serving some hosts (e.g. a network zone) |
| **Passive host** | reached by runners over SSH/SSHOQ |
| **Proactive host** | runs regops and reports to the endpoint |
| **Security level** | 0 open, 1 authenticated, 2 encrypted, 3 hardened |
| **Central configuration** | service settings stored in CouchDB and edited from the web UI |
| **Instance** | one running process of a service (one replica) |
| **Distribution** | a tested combination of component versions (Helm chart, compose bundle) |
