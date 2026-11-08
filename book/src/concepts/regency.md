# regency: configuration management at scale

regent-sdk manages one host at a time. regency turns it into a service for
a whole fleet.

## What regency adds

| Need | regency's answer |
|---|---|
| Thousands of hosts | tasks are distributed through RabbitMQ to any number of runners |
| Regular checks | recurring **orders** (every N minutes, or cron) fire tasks automatically |
| History and live state | every outcome is stored in CouchDB; the web UI shows the fleet live |
| Several sources of truth | inventories and expected states from Git, the API or the UI, in one catalog |
| Hosts that pull their own configuration | regops agents report to regency's endpoint |
| Access control | users, groups, API tokens, OIDC, and Cedar policies |
| Production operations | replicated services, clustered infrastructure, packages, images, Helm |

## Distributed by design

regency is a set of small, stateless services. None of them is special:
every one can run several replicas, and losing one never loses work.

- Work is **queued**, not called: a task waits in a replicated queue until a
  runner takes it; if the runner dies, another one picks the task up.
- Delivery is **at least once** and every task has an **idempotency key**:
  a task run twice is recorded once.
- Where exactly one actor must do something (firing a recurring order,
  running a remediation on a host), regency uses **ownership through
  RabbitMQ**: exclusive queues act as locks that vanish when their holder
  disconnects, so there is no lease to renew and no clock to trust.
- The host lock (in regent-sdk) protects hosts even from a runner that lost
  its connection to RabbitMQ but still talks to the host.

## Passive and proactive hosts

```text
passive:    scheduler ─▶ RabbitMQ ─▶ runner ──SSH/SSHOQ──▶ host
proactive:  host (regops, pulls Git) ──gRPC/mTLS──▶ endpoint ─▶ RabbitMQ
```

Both produce exactly the same outcome format, so the rest of regency never
needs to know how a host is managed. A host can be both: regops keeps it
compliant between runs, and regency runners can still assess it on demand.

## One host, one identity

A host has one global `host_id`, whatever the inventories it appears in.
Its history, its live state and its remediation queue are tied to that id.
If two sources describe the same host differently, regency flags the host as
*conflicting* and refuses to touch it until the sources agree — it never
connects with credentials that might be wrong.
