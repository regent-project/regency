# Glossary

| Term | Meaning |
|---|---|
| **Assess** | read-only compliance check of a host against an expected state |
| **Reach** | assess, then remediate what differs (called *enforce* in the CLIs) |
| **Attribute** | one building block of an expected state (a package, a file line, a service…) |
| **Expected state** | the desired configuration of a host: a list of attributes |
| **Inventory** | a list of hosts with their connection method and variables |
| **Host definition** | one host of the catalog: id, endpoint, connection method, inventories, labels, runner pool |
| **Catalog** | every source, inventory, host definition and expected state known to regency |
| **Source** | where catalog items come from: a Git repository, the API, or the web UI |
| **Assignment** | a host selector + an expected state + a priority: what those hosts should look like |
| **Effective expected state** | a host's expected state, layered from every assignment selecting it (lowest priority first; same `Name` = override) |
| **Task / job** | one host + its effective expected state (or a one-shot attribute) + a mode; its id is the idempotency key |
| **Outcome** | the result of a task: compliance status, remediations or actions, error |
| **Order** | a recurring rule: on a schedule, Assess or Reach the hosts of an assignment |
| **Run** | an immediate, one-off order (from the UI or the API) |
| **Host selector** | host ids, inventories and `label=value` matches, resolved when the order fires |
| **Runner pool** | a group of runners serving some hosts (e.g. a network zone) |
| **Passive host** | reached by runners over SSH/SSHOQ |
| **Proactive host** | runs regops and reports to the endpoint |
| **Security level** | 0 open, 1 authenticated, 2 encrypted, 3 hardened |
| **Central configuration** | service settings stored in CouchDB and edited from the web UI |
| **Instance** | one running process of a service (one replica) |
| **Distribution** | a tested combination of component versions (Helm chart, compose bundle) |
