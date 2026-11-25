# Changelog

One version for the whole of regency: every service, `regencyctl`, the web
client and the all-in-one package share it. This file holds **only the
release being prepared** (`# Unreleased`, renamed `# Release v<version>` when
it is released), with one `##` subsection per component (write "No change."
for a component that did not change); it becomes the GitHub release notes.
Each new release starts this file fresh: the notes of earlier releases are
the GitHub releases (and this file's git history). See RELEASING.md.

# Release v0.8.0

**Runs and colours per assignment, and a map that shows each host once.**
Assess / Reach of an assignment now runs that assignment only, never the
hosts' other assignments, and a host has a state per assignment: on the
map's graph, assignments are nodes and each host is drawn once, linked to
each of its assignments in its state for it (its own colour is the worst
link). The web client also gains pages for recurring jobs (CronJob), users
and API tokens, Cedar policies and certificate revocation; host
definitions and hosts are one Hosts page; on the map, assignments and
labels are edited where they are drawn (connect an assignment to hosts,
disconnect them, edit a group's labels, with a preview of the hosts
joining and leaving), and cards and hosts are moved by dragging them. Also fixes `LineInFile` lines starting with `-`
(regent-sdk 0.26.1) and a false drift warning on proactive hosts.

**Upgrading** (the all-in-one does all of this by itself):

- **Upgrade the scheduler and the runners together**: regency 0.8.0 is
  built on regent-sdk 0.26.1, and a runner refuses tasks built with another
  regent-sdk version (they are recorded as errors), so tasks scheduled by a
  0.7.1 scheduler fail on a 0.8.0 runner and the reverse.
- **Run `regencyctl init`** once with the new version: it adds the view the
  dbwriter's monthly cleanup now uses (`by_month_v2` in `regency_state`).
  Until then the cleanup stops at that step (logged) and drops nothing.
- Messages and documents only gain fields (task header and outcome
  `assignment`, hosts' `assignments`): 0.7.1 and 0.8.0 services read each
  other's. A 0.7.1 dbwriter ignores the new field, so states per
  assignment start with the first jobs recorded by a 0.8.0 dbwriter.
- Until then, a host's last whole-state check counts for each of its
  assignments not checked alone since, as long as none of its assignments
  changed; otherwise the map shows it grey (not checked) for that
  assignment until its next Assess.
- Assess / Reach of a host (its page) now gives one job per assignment of
  the host.
- Map arrangements saved in browsers are reset once (the map's groups
  changed).

## scheduler

- Built on regent-sdk 0.26.1 (see runner): upgrade it together with the
  runners.
- **One assignment per job.** Assess / Reach of an assignment (the map's
  buttons, the Assignments page, CronJobs) runs that assignment's expected
  state only, on the hosts it selects; the hosts' other assignments are not
  touched. Assess / Reach of hosts (a host's page, a label selector) runs
  each of their assignments, one job each. A host with conflicting
  assignments still gets no job at all.

## runner

- regent-sdk 0.26.1: a `LineInFile` line starting with `-` (e.g. an iptables
  rule such as `-A INPUT -m conntrack --ctstate INVALID -j DROP`) or holding
  a single quote is now found on the host. Before, such an attribute was
  always reported not compliant and each Reach appended the line again.
- Outcomes say which assignment the job checked.

## dbwriter

- Hosts keep their state **per assignment** (from the newest job of each),
  next to their latest state, and their newest check of the whole
  effective state (jobs from before 0.8.0), which keeps counting for the
  assignments not checked alone yet; retention clears them with the month
  they refer to (needs `regencyctl init`, see Upgrading).

## backend

- Host summaries carry the host's state per assignment.
- New RPC `RevokeApiToken`. A revoked API token is refused at once, even
  when it was verified a moment before.
- `SetSettings` checks the certificate deny lists (`revoked_hosts`,
  `revoked_serials`, `revoked_fingerprints`) and writes serials and
  fingerprints the way the endpoint compares them (lowercase hex serials,
  `AB:CD:…` fingerprints), so an entry typed differently still matches.
- Proactive hosts: no more false *drift* warning when the assignment's
  expected state uses templates (e.g. `{{ host }}` in a file path). The agent
  reports the state it applied with the templates rendered, and each of
  those attributes counted as *different*. A template now matches its
  rendered value (the same variable must have the same value throughout the
  attribute).

## endpoint

No change.

## sourcesync

No change.

## regencyctl

- `init` adds the view `by_month_v2` to `regency_state` (retention of the
  hosts' states per assignment).

## frontend

- **Live view map: each host appears once.**
  - Graph display: each assignment is a node (its card, with its hosts'
    states for it and its Assess / Reach); hosts are grouped by the exact
    set of assignments selecting them, inside a light outline between
    those assignments' nodes (captioned with them, one per line, while the
    pointer is on the group; a small pie of their
    states when folded). A host's state for an assignment is the colour of
    its link to it; a host's colour is its worst link.
  - Assignment card: more compact (the counts on the left, the donut on
    the right), **Assess** quiet, **Reach** in orange (it changes the
    hosts), and a **Run** button for a hand-written attribute (formerly
    *Run an attribute…* in the Assess / Reach dialogs).
  - **Assignments sharing many hosts are placed close together**
    (unrelated ones apart), and links point from the assignment to the
    hosts it constrains (arrowheads, with faint chevrons slowly flowing
    along them; none when the system asks for reduced motion).
  - **Links grouped** by default: one plain ribbon per group and
    assignment, as wide as its hosts. Hovering or clicking an
    assignment's card shows its ribbons' statistics (hosts per state for
    it). Hovering a host, or hovering / clicking a ribbon, shows the hosts'
    own links (Escape closes the clicked ones); **Links: per host** in the
    toolbar shows every link.
  - The areas display is gone: the map is the graph only.
  - **Edit assignments and labels on the graph** (like a node editor):
    - drag from the handle at the bottom of an assignment's card to a
      group of hosts (or one host) to make the assignment select them;
    - select a ribbon (click it), then its **×** or the Delete key, to
      make it stop selecting them;
    - each time, choose between **relabelling the hosts** (their host
      definitions get or lose the labels the selector needs) and
      **changing the assignment's selector** (a suggestion is prefilled
      and editable), with a preview of the hosts joining and leaving every
      assignment before anything is saved;
    - **+ Assignment** (toolbar), **Edit** and **Delete** above a card
      (or the Delete key on a selected card): the Assignments page's form
      opens in the right panel, and the hosts its selector would select
      are ringed on the map;
    - click inside a group of hosts: the labels they share, editable for
      all of them at once, and **New assignment from these hosts**.
    Items from Git sources stay read-only, and so do hosts defined in Git,
    by several sources or only by their regops agent (listed with the
    reason).
  - **Move things by dragging them**: drag a card, a host or a group of
    hosts (anywhere inside its outline) where you want it; drag empty space
    to pan. Pick several with Ctrl / Cmd + click or
    Shift + drag (a selection rectangle) and drag them together; Escape or
    a click elsewhere drops the picks. The browser remembers the
    arrangement; **Auto-arrange** returns to the automatic
    layout.
  - While an Assess / Reach of an assignment runs, its links to the hosts
    with a job in progress turn light blue, the job colour, like those
    hosts, and go back to normal as each job ends.
  - **Center** (toolbar): zooms and pans so that the whole map fits in view,
    leaving moved cards and hosts where they are.
  - **Full host names** (no longer cut to 8 characters; shown from the same
    zoom level as before).
  - Hosts are *compliant* or *not compliant* (*reach success* and *reach
    failed* stay in the host's details).
  - Folding and unfolding a group glides (200 ms) and no longer re-zooms
    the view.
  - Proactive (regops) and passive hosts apart: in a group holding both,
    proactive hosts are on the left and passive ones on the right (two
    half-ring lobes, like ∞).
  - Host panel: *Expected state* and *Last job log* merged into **Last known
    state**. Each attribute of the host's effective state is coloured with
    its outcome in the last job (compliant or fixed, not compliant, failed,
    aborted, not checked). Unfold an attribute for its definition and what
    the last job did with it (commands, output, verdict).
  - A run started from the map is no longer lost on a reload or a visit
    to another page: until all its jobs have ended, its panel opens again
    with the Live view, and a "Job in progress" button per run, at the
    right end of the toolbar, brings it back (and disappears when its jobs
    end, panel open or not).
- A host's **state per assignment** in the map's hover tip and host panel
  and on the host page; the fleet counters and the Hosts page use the worst
  of them.
- **Hosts and host definitions merged.** The Hosts page lists every host,
  including the defined ones that never ran (*Never run*), with new
  *Address* and *Defined by* columns (`regops` for a proactive host, `—` for
  a host known only from its outcomes). A host's page shows its definitions
  and edits, adds or deletes them. The *Host definitions* page is gone
  (`/config/hosts` opens the Hosts page).
- **Add host**: tabs *Over SSH*, *Over SSHOQ* (the runners' key, port 443,
  optional path, server certificate checked against the system CAs, a
  known_hosts file, or not verified) and *With Regops*; shorter, with the
  explanations left to the documentation.
- **CronJob** page (Activity): create, edit, enable/disable and delete
  recurring Assess / Reach of an assignment (fixed interval such as `15m`
  or `1h30m`, or cron expression and timezone), with each one's next run
  and jobs of the last hour.
- **Users** page (Administration): local users and service accounts, their
  groups (shipped roles in one click), password, enable/disable; API tokens
  created (shown once) and revoked.
- **Policies** page (Administration): the shipped roles, and the Cedar
  policies you add, edit or delete (checked against the schema, shown on
  the page, when saved).
- Security page: **certificate revocation** of proactive hosts (host ids,
  certificate serials, fingerprints).
- The Live view icon of the menu slowly fades to red and back only while the
  Live view is shown.

## all-in-one

- Includes every change above; its start runs `regencyctl init`, so nothing
  to do by hand.
