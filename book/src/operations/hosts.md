# Hosts and labels

## Host definitions

A host definition gives regency everything it needs to reach a host:

| Field | Meaning |
|---|---|
| host id | the global identity of the host (unique across sources) |
| endpoint | address and port (`10.0.0.5:22`), or `localhost` |
| connection method | `Ssh2`, `Sshoq` or `Localhost`, with secret references |
| labels | `key=value` pairs: they decide which [assignments](assignments.md) the host gets |
| runner pool | which runners reach it (default `default`) |
| host vars | variables for templates |

Definitions come from [sources](sources.md): Git repositories (read-only in
regency), the API, or the web UI (**Hosts → Add host** for an SSH host
with the runners' key, **Administration → Host definitions**: create,
edit, delete).

## The runners' SSH key

A runner with `[runner] ssh_key` set (the all-in-one and the packages set
it) generates an ed25519 key there at its first start and publishes its
public key; **Hosts → Add host** shows it per runner pool, with the command
authorizing it on a host. Runners of one pool on several machines each
have their own key at the same path: authorize every key of the pool. The
host definition refers to the key by its path (a `files` secret reference),
never by its content.

A host defined but never run has no state yet: it shows on the map and in
the Hosts table after its first job. Its page (`/hosts/<id>`) already shows
its effective expected state, with Assess and Reach.

## Labels

Labels are what regency selects hosts by, as Loki selects streams and
Kubernetes selects pods: an [assignment](assignments.md) applies to the
hosts its label selector matches, so labelling a host is how it gets its
expected states (`role=web` → the `web` assignment). Runs, live views and
access policies select hosts the same way.

```text
{}                                   every host
{env="prod"}                         hosts labelled env=prod
{env="prod", role=~"web|api"}        ... whose role is web or api
{env="prod", role!="db"}             ... whose role is not db
{host="web-01"}                      one host
```

A host without the label reads as `""` (so `{env!="prod"}` includes hosts
without `env`). Every host also has two **built-in labels**: `host` (its
id) and `source` (the sources defining it; `=` holds if one of them
matches, `!=` if none does). Hosts cannot set these two themselves.

Nothing is stored about which assignments a host gets: change a host's
labels, or an assignment's selector, and the hosts follow at once
(effective states, runs, the map, access policies). Connection method,
variables and runner pool belong to each host.

Deleting a definition never deletes the host's history. To remove a host
and all its jobs and outcomes, use **Delete host data** on the host's page;
a host that reports again (regops still running) comes back as a new host.

## Conflicting definitions

The same host id may appear in several sources. Identical definitions merge
(their labels are combined). Different definitions mark the host
**conflicting**: it is flagged on the map and in the table, and no task is
built for it until the sources agree.

## Runner pools

A pool is a set of runners serving some hosts — typically a network zone
(`dmz`, `factory-1`). Runners of a pool only take tasks of their pool, and
hold only the credentials of their hosts. Set the pool on a host
definition (in Git, on a whole `hosts/` file), or override it in an order.

## Host selectors

Assignments (and the orders running them) select their hosts with their
label selector. "Run now" and the live views can also name host ids,
`label=value` pairs, a label selector or assignments (`<source>/<name>`).
Everything is resolved when it fires: a host whose labels now match is
picked up by the next firing.

## The live views

- **Map** (home page): one card per assignment (hosts without one in
  *(unassigned)*), with its hosts as dots colored by their state against their
  effective expected state ([Assignments](assignments.md)): grey when
  unassigned. Two displays: **graph** (hosts on rings around the card, the
  least compliant nearest; a host with several assignments around its last
  one, the most specific) and **areas** (each group a rectangle holding its
  hosts; a host with several assignments drawn in each).
  Each card has **Assess** and **Reach** buttons (each host runs its own
  effective state; an assignment's dialog can also run a hand-written
  attribute once). Once confirmed, the run opens in the right panel and
  is followed live: whether the command was accepted, LIVE then FINISHED,
  a progress bar, counts and its hosts (those needing attention first;
  click one to see it). A click on a card folds or unfolds its hosts. A host with
  a job in progress (pending, running or abort requested) turns light blue
  until it finishes. Zoom with the wheel, pan by dragging;
  hovering a host shows a tip next to the cursor; clicking it opens the
  right panel (its assignments and effective state), **Details** opens
  its page. The right panel floats over the map; drag its left edge to
  resize it. Proactive hosts
  are diamonds ([Proactive hosts](proactive-hosts.md)).
- **Hosts**: a live table with filters, all applied together: a plain
  search (words found anywhere in the host's id, state, labels,
  assignments or error; no syntax), a state, an assignment (or
  *Unassigned*) and a label selector (`{kind=~"web|api", env!="dev"}`,
  written by hand or built from the hosts' labels, as on the Assignments
  page).
- **Host page**: state, failing attributes and needed remediations,
  certificate status (proactive hosts), outcome history with full details,
  and its **effective expected state**: its assignments in order, each with
  its stack,
  Assess / Reach now, and its attributes, one folded row each, named after
  the attribute type and its name, tagged with the assignment each comes
  from.
