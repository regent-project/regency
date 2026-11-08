# Running jobs and recurring orders

## Run now

From the map (a group's card) or a host's panel or page: **Assess** or
**Reach**. Each host runs its own effective expected state
([Assignments](assignments.md)); hosts without an assignment are skipped.
Over the API, `RunNow` takes a host selector, so one command can target a
whole inventory or every `env=prod` host; with `one_shot_yaml` it runs a
hand-written attribute once instead (history only). Each host gets its own job; follow them
in **Jobs** (live, last 24 hours), where queued and running jobs can be
**aborted**.

## Recurring orders

An order runs the hosts of an assignment on a schedule, each against its
effective expected state:

| Field | Example |
|---|---|
| schedule | every 900 seconds, or cron `0 3 * * *` in `Europe/Paris` |
| assignment | `infra/web` |
| mode | Assess or Reach |
| pool | optional override of the hosts' runner pool |

Orders are managed through the API (`UpsertOrder`, `DeleteOrder`,
`ListOrders`); an orders page in the web UI is planned.

Rules worth knowing:

- Firings are aligned to the clock (`every 15 min` fires at :00, :15, :30,
  :45) and **missed firings are not replayed**: after an outage, only the
  current slot runs.
- A recurring task that cannot start before the next firing expires
  (`EXPIRED`) instead of piling up.
- The assignment's hosts and their effective states are resolved at each
  firing (latest revisions), and the order's **author is re-checked** against the current
  policies: if they may no longer run it, the firing is skipped and recorded.
- Editing an order changes its revision; jobs of the new revision get new
  keys.

## Reading outcomes

A job's outcome tells you:

| State | Meaning |
|---|---|
| compliant | Assess: nothing to change |
| not compliant | Assess: the listed remediations are needed |
| reach success | Reach: remediations applied and verified |
| reach failed | Reach: some remediation failed (see the actions) |
| aborted | stopped by an abort; the actions done so far are listed |
| error | the task could not run: connection, host busy, refused by a security level, inline credentials… |

The full outcome (every attribute, remediation and action) is on the host
page, under **History → Details**.
