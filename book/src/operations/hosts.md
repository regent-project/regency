# Hosts and inventories

## Host definitions

A host definition gives regency everything it needs to reach a host:

| Field | Meaning |
|---|---|
| host id | the global identity of the host (unique across sources) |
| endpoint | address and port (`10.0.0.5:22`), or `localhost` |
| connection method | `Ssh2`, `Sshoq` or `Localhost`, with secret references |
| inventories | the inventories it belongs to (in its source) |
| labels | `key=value` pairs, used by host selectors |
| runner pool | which runners reach it (default: the inventory's, else `default`) |
| host vars | variables for templates |

Definitions come from [sources](sources.md): Git repositories (read-only in
regency), the API, or the web UI (**Configuration → Hosts**: create, edit,
delete).

Deleting a definition never deletes the host's history. To remove a host
and all its jobs and outcomes, use **Delete host data** on the host's page;
a host that reports again (regops still running) comes back as a new host.

## Conflicting definitions

The same host id may appear in several sources. Identical definitions merge
(the host is in several inventories). Different definitions mark the host
**conflicting**: it is flagged on the map and in the table, and no task is
built for it until the sources agree.

## Runner pools

A pool is a set of runners serving some hosts — typically a network zone
(`dmz`, `factory-1`). Runners of a pool only take tasks of their pool, and
hold only the credentials of their hosts. Set the pool on an inventory, a
host definition, or override it in an order.

## Host selectors

Assignments, orders and "run now" commands target hosts with a selector,
resolved when they fire:

- explicit host ids: `web-01, web-02`;
- whole inventories: `infra/web` (`<source>/<inventory>`);
- labels: `env=prod`.

A host added to an inventory is picked up by the next firing.

## The live views

- **Map** (home page): one card per assignment (or inventory, label value,
  or state), with its hosts as dots colored by their state against their
  effective expected state ([Assignments](assignments.md)): grey when
  unassigned. Two displays: **graph** (hosts on rings around the card, the
  least compliant nearest; a host in several groups linked to the others by
  a dashed line) and **areas** (each group a rectangle holding its hosts).
  Each card has **Assess** and **Reach** buttons (each host runs its own
  effective state; an assignment's dialog can also run a hand-written
  attribute once); a click on a card folds or unfolds its hosts. A host with
  a job in progress (pending, running or abort requested) turns light blue
  until it finishes. Zoom with the wheel, pan by dragging;
  click a host to show it in the right panel (its assignments and effective
  state), **Details** opens its page.
- **Hosts**: a live table with a filter (id, state, `label=value`,
  inventory).
- **Host page**: state, failing attributes and needed remediations,
  certificate status (proactive hosts), outcome history with full details,
  and its **effective expected state**: the assignments layered into it,
  Assess / Reach now, and its attributes, one folded row each, named after
  the attribute type and its name, tagged with the assignment each comes
  from.
