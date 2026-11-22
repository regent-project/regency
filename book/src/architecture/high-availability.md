# High availability

regency has no single point of failure when every service runs at least two
replicas and RabbitMQ and CouchDB run as three-node clusters.

| Failure | What happens |
|---|---|
| a runner dies mid-task | the task is redelivered to another runner; the host lock is released with the dead SSH connection |
| a runner loses RabbitMQ but not the host | it stops its tasks; the host lock keeps any other runner from acting meanwhile |
| a scheduler dies | its shards' locks vanish with its connection; another scheduler takes them within seconds |
| two schedulers lead a shard briefly | they produce the same task keys; duplicates are recorded once |
| dbwriter is down | messages wait in their queues; nothing is lost |
| a backend replica dies | clients reconnect to another one (backend is stateless) |
| a RabbitMQ node fails | quorum queues keep working with a majority; a minority partition pauses (`pause_minority`) |
| a CouchDB node fails | every document has three copies; concurrent writes are merged deterministically |

## Sizing hints

- **Runners** scale with the number of hosts and the length of tasks;
  `assess_concurrency` and `reach_concurrency` set the assessments and
  Reaches each runner runs at once (16 each by default: every one holds a
  connection to its host). Raise them with the runner's resources and what
  the hosts' SSH servers or bastions accept; add runners to go further.
- **Schedulers** and **sourcesync** need two or three replicas for
  availability; work is spread by shards.
- **dbwriter** scales with the outcome rate; two to four replicas handle
  large fleets.
- **backend** scales with the number of UI users and API clients.
