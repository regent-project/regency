# Changelog

One version for the whole of regency: every service, `regencyctl`, the web
client and the all-in-one package share it. Each release has a section
`# Release v<version>` with one `##` subsection per component (write "No
change." for a component that did not change); it becomes the GitHub release
notes. Changes waiting for the next release go under `# Unreleased` (see
RELEASING.md).

# Release v0.3.0

A one-line installer turns a Linux machine into a complete regency, RabbitMQ
and CouchDB included; and a fix for the all-in-one's initial admin password.

## scheduler

No change.

## runner

No change.

## dbwriter

No change.

## backend

No change.

## endpoint

No change.

## sourcesync

No change.

## regencyctl

No change.

## frontend

No change.

## all-in-one

- One-line installer for Linux: `curl -fsSL
  https://github.com/regent-project/regency/releases/latest/download/install.sh | sh`
  installs RabbitMQ and CouchDB (from their projects' repositories, localhost
  only), the `regency-all-in-one` package of the release, fills
  `/etc/regency/all-in-one.env` and starts regency. Debian 13+, Ubuntu
  24.04+, RHEL 10 and compatibles, Fedora 40+ (best effort). Releases now
  also publish `install.sh` and `SHA256SUMS` (checked by the installer).
- Fix: `REGENCY_ADMIN_PASSWORD` (initial password of `admin`) stopped every
  service from starting (it reads as a configuration override of an unknown
  `[admin]` section); regency-aio no longer passes the variables it consumes
  to the services. Also affected the Docker quick start, which sets it
  (empty) by default.

# Release v0.2.0

Web client improvements: a chart of jobs in progress, a steadier map, and outcome
details right in the host history.

## scheduler

No change.

## runner

No change.

## dbwriter

No change.

## backend

No change.

## endpoint

No change.

## sourcesync

No change.

## regencyctl

No change.

## frontend

- Map, "Jobs in progress" panel: chart of pending and running jobs over the
  last 2 min, 10 min, 30 min or 1 h (time-weighted averages, so short jobs
  are never missed), built from the jobs the page already receives; the
  whole last hour shows as soon as the page has loaded. Its scale follows
  the data shown (gridlines at round values); hovering it shows the time
  slot and its values.
- Map, graph and areas displays: hosts in name order, so they no longer
  move when a host's state changes (the graph display used to put the worst
  hosts on the inner rings).
- Map: host names zoom with the hosts (their size is in map units, no
  longer a fixed 9 px), shown once they are readable.
- Host page, history: "Details" expands the outcome right under its row
  ("Hide" folds it back; several can be open), instead of showing it below
  the whole list.

## all-in-one

The web client changes above.

# Release v0.1.0

First release of regency, the fleet-scale control plane of regent-sdk:
scheduling of assessments and remediations, outcome history, live view.
Built on regent-sdk 0.23.0.

## scheduler

Fires recurring orders (interval or cron, sharded leadership) and expands
`run` commands into tasks.

- Assignments (D23): each host's task carries its effective expected state
  (layered assignments); unassigned or conflicting hosts get no task; orders
  reference an assignment; one-shot attribute runs.
- Fleet scale: firing a large order ticks the shard watchdog per published
  task (it dropped the connection mid-firing); tasks are published with up to
  64 confirms in flight (about 6× faster); authorization indexes host
  definitions once per firing.
- Pools are declared on a disposable channel (a refused declaration closed
  the publishing channel, and the scheduler could then never publish again);
  a closed publishing channel renews the connection.

## runner

Runs tasks against passive hosts (shared Assess queue, owned Reach
partitions, aborts, D9/D19 refusals).

- Assignments (D23): the outcome records the effective-state revision its
  task checked, or that it was a one-shot run.
- Fleet scale: a refused partition lock (`RESOURCE_LOCKED`) no longer counts
  as a lost connection; lock probing backs off after a fruitless sweep;
  closing a broken connection is bounded (a runner could hang there forever,
  ignoring SIGTERM).
- A closed publishing channel renews the connection.

## dbwriter

Records outcomes, job lifecycles, dead-lettered tasks and commands in
CouchDB; runs retention.

- Assignments (D23): `assignment:` catalog items; host docs keep the
  effective-state revision of their state.
- Closing a broken RabbitMQ connection is bounded (5 s).

## backend

gRPC and gRPC-Web gateway: authentication, Cedar authorization, live views,
history and commands; serves the web client.

- Assignments (D23): `GetEffectiveState`, assignment fields on hosts and
  orders; `RunNow` runs effective states or a one-shot attribute
  (`one_shot_yaml`).
- Attribute builder: `GetAttributeSchema` (regent-sdk's attribute JSON
  Schema) and `ConvertAttributes` (YAML ⇄ JSON through the SDK, with the D9
  check); one-shot YAML errors describe the form the YAML has (list,
  expected state, attribute).
- Fleet scale: the catalog's derived indexes are rebuilt once per `_changes`
  batch, not per change (startup with thousands of hosts never became
  ready); `RunNow` authorization indexes host definitions once.
- Live views (`WatchHosts`, `WatchJobs`) end when backend stops (its
  graceful shutdown waited for every open browser); a closed publishing
  channel renews the connection.

## endpoint

gRPC endpoint for proactive hosts (regops): mTLS in the three D13 modes,
enrollment, outcome forwarding.

## sourcesync

Syncs Git sources into the catalog.

- Assignments (D23): `assignments/*.yaml` in Git sources.
- Shard locks: a refused lock no longer counts as a lost connection; closing
  a broken connection is bounded (5 s); a closed publishing channel renews
  the connection.

## regencyctl

`init`: RabbitMQ and CouchDB setup, default settings, bootstrap admin.

- Closing the RabbitMQ connection is bounded (5 s).

## frontend

Leptos web client: live map, hosts, host and jobs pages, configuration pages.

- Map: groups as cards with their hosts around them (graph display) or as
  rectangles holding them (areas display), folding, zoom around the cursor,
  host names only when they fit. Hosts with a job in progress are drawn in
  the job colour, without animation.
- Map at fleet scale: hosts and links drawn on a canvas, only what is in
  view; live updates applied in batches; job events do not recompute the
  layout (3000 hosts: 60 fps idle with live updates, ~45 fps while zooming
  and panning).
- Map page in three columns: jobs in progress (with Abort) and recent
  activity on the left; fleet counts, groups and the selected host (summary
  and expected state explorer, `/?host=<id>`) on the right; panels can be
  shrunk to a rail.
- Assignments (D23): host colours against the effective expected state
  (outdated, invalid, unassigned), grouping by assignment (default),
  Assess/Reach without choosing a state, effective state explorer with each
  attribute's assignment.
- One-shot attribute dialog from an assignment: **Builder** (forms generated
  from regent-sdk's attribute schema, natures grouped by category, checked
  live) and **Code** (YAML); starts empty, Run enabled once the attributes
  are valid.
- Dark theme in greys and blues from the logo; logo in the header, login
  page and favicon.

## all-in-one

`regency-all-in-one` package: every regency service, `regencyctl` and the
web client on one host, under one systemd unit (`regency-all-in-one`,
supervised by `regency-aio`). Secrets, the built-in CA and the endpoint's
certificate are generated at first start. RabbitMQ and CouchDB are not
included: their addresses and admin credentials go in
`/etc/regency/all-in-one.env`.
