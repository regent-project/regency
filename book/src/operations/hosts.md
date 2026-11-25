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
regency), the API, or the web UI: **Hosts → Add host** (tabs *Over SSH*,
*Over SSHOQ*, *With Regops*) for a host reached with the runners' key (SSHOQ
authenticates with the same key) or running the regops agent; a host's page (**Hosts →** the host) shows its
definitions and edits, adds or deletes them (any connection method).

The **Hosts** page lists every host, whether regency has seen it (it has
outcomes) or only defined it: a defined host that never ran shows as
*Never run* until its first job, and the *Defined by* column shows its
sources (`regops` for a host whose agent reports by itself, `—` for a host
known only from its outcomes). Deleting a definition never deletes the
host's history.

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

- **Map** (home page): each host is drawn once. Hosts are grouped by the
  exact set of assignments selecting them (hosts without one in
  *(unassigned)*). A host has a state for each of its assignments, and
  its dot shows the worst of them ([Assignments](assignments.md)): grey
  when unassigned, never checked, or checked against an older version of
  an assignment. The map is a graph: each assignment a card, close to the
  assignments it shares many hosts with; each
  group of hosts inside a light outline between its assignments' cards
  (point at it to see its assignments, one per line, and its counts), a small pie when folded. Links are
  *grouped*: one plain ribbon per group and assignment, as wide as its
  hosts, its arrow pointing from the assignment to the hosts it constrains (faint
  chevrons slowly flow along it). Hover or click an assignment's card to see, on each
  of its ribbons, how many hosts are in each state for it (Ctrl / Cmd +
  click selects several; Escape or a click on empty space drops them).
  Hover a host, or hover / click a ribbon, to see the hosts' own links,
  each coloured with the host's state for that assignment only; Escape
  closes them; the toolbar's **Links: per host** shows them all. In a
  group with both kinds, proactive hosts (regops) are on the left and
  passive ones (reached by runners) on the right: two half-ring lobes
  either side of the group, like ∞.
  **Assess** and **Reach** (an assignment's card) run that assignment's expected state only, on its hosts: the
  hosts' other assignments are not touched; Reach, which changes the
  hosts, stands out in orange. **Run** on the card writes one attribute
  by hand and runs it once on the assignment's hosts. Once confirmed, the run opens in the right panel and
  is followed live: whether the command was accepted, LIVE then FINISHED,
  a progress bar, counts and its hosts (those needing attention first;
  click one to see it). The browser keeps a run until all its jobs have
  ended: after a reload, or back from another page, its panel opens again,
  and a **Job in progress** button per run, at the right end of the
  toolbar, brings its panel back after closing it or looking at a host. A
  click on a group's card folds or unfolds its hosts (they glide back
  under it, or out of it). **Drag** a card, a host or a group of hosts
  (grab it anywhere inside its outline) to move it (a host then follows
  its group); drag empty space to pan. Pick several things
  with **Ctrl + click** (Cmd on macOS) or **Shift + drag** (a selection
  rectangle; with Ctrl it adds to the picked ones), then drag any of them
  to move them all; **Escape** or a click elsewhere unpicks everything.
  The browser remembers the arrangement, per display; **Auto-arrange**
  puts everything back in the automatic layout, and **Center** zooms and
  pans to fit the whole map in view without moving anything. A host with
  a job in progress (pending, running or abort requested) turns light blue
  until it finishes; so do the links (ribbons and per-host links) from the
  assignment that job runs (Assess / Reach) to that host. Zoom with the wheel, pan by dragging;
  hovering a host shows a tip next to the cursor; clicking it opens the
  right panel: its summary, what runs on it live, and its **Last known
  state**, the attributes of its effective state each coloured with its
  outcome in the host's last job (green compliant or fixed, red not
  compliant or failed, amber aborted, grey not checked); unfold one for
  its definition and what the last job did with it. **Details** opens
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

## Editing on the map

The map edits assignments and labels where they are drawn. A group of
hosts stands for the labels its hosts share; a selector is an AND of
filters, so making an assignment select more or fewer hosts can change
either the hosts or the selector, and the map asks which each time:

- **Connect:** drag from the handle at the bottom of an assignment's card
  to a group of hosts (or one host). *Label the hosts* sets on them the
  labels the selector's `key="value"` filters need (refused when another
  filter would still fail, e.g. a regex or a `!=`); *Change its selector*
  offers a selector matching the assignment's hosts and the new ones (the
  labels they all share, else a `host=~"…"` list), which you can edit.
- **Disconnect:** click a ribbon to select it, then its **×** (or press
  Delete). *Relabel the hosts* removes a label one of the selector's
  `=` / `=~` filters needs (pick which); *Change its selector* adds an
  exclusion (a label value only those hosts have, else `host!~"…"`).
- **Preview:** before anything is saved, each option lists, per
  assignment, the hosts that would join and leave it. It is computed by
  the browser from the labels it knows; the scheduler resolves selections
  again when it runs them.
- **A group's labels:** click inside a group of hosts: the labels they
  share, editable; applying changes them on every host of the group (same
  preview). **New assignment from these hosts** opens a new assignment
  selecting those labels.
- **Assignments:** **+ Assignment** in the toolbar, **Edit** and
  **Delete** above a card (or Delete on a selected card). The form is the
  Assignments page's, in the right panel; while it is open, the hosts its
  selector would select are ringed on the map.

Relabelling writes a host's definition, and only when the host has exactly
one, from an API/frontend source: hosts defined in Git, by several sources
(changing one definition would make them conflict) or only by their regops
agent are left unchanged and listed with the reason. A label reported by a
regops agent cannot be removed this way (the agent would set it again):
change it in the agent's `[Regency] labels`. Assignments from Git sources
are read-only on the map too.

