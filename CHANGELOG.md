# Changelog

One version for the whole of regency: every service, `regencyctl`, the web
client and the all-in-one package share it. This file holds **only the
release being prepared** (`# Unreleased`, renamed `# Release v<version>` when
it is released), with one `##` subsection per component (write "No change."
for a component that did not change); it becomes the GitHub release notes.
Each new release starts this file fresh: the notes of earlier releases are
the GitHub releases (and this file's git history). See RELEASING.md.

# Release v0.7.0

**A plainer, classic web client.** The console takes the look of
common admin dashboards (sidebar, header, cards, soft status pills; a
grey-blue light theme and a navy dark theme). The Hosts page gets
filters that need no syntax, and runs started from the Live view are
followed live in a panel. The runner gets a cap on the Reaches it runs
at once (`reach_concurrency`, a new optional setting): large Reaches no
longer open a thousand SSH connections together. No message or document
change: services of 0.6.0 and 0.7.0 work together and the upgrade needs
no action.

## scheduler

No change.

## runner

- New setting `[runner] reach_concurrency` (default 16, like
  `assess_concurrency`): the Reaches a runner runs at once, all its Reach
  partitions together; the others wait their turn (still pending). Until
  now a runner ran up to `reach_prefetch` × owned partitions Reaches at
  once (1024 with the defaults), so a Reach of a large assignment opened
  about a thousand SSH connections together: SSH servers (`MaxStartups`),
  bastions and the runner itself refused or failed many of them ("Failure
  to establish connection"). Raise it, or add runners, for faster large
  Reaches.

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

- **New visual identity**, plainer and closer to classic admin dashboards
  (TailAdmin-style): full-height sidebar with line icons and grouped
  sections, header (menu toggle, fleet counts as soft pills, jobs chart,
  theme switch, user menu with *Log out*), bordered cards and tables, one
  blue accent, sentence-case status pills, and a two-panel sign-in page.
  The light theme is easy on the eye (grey-blue page and cards, no white,
  header and sidebar in a darker slate blue that sets them apart from the
  content); the dark theme matches it. No more blinking marks (Live view
  entry, LIVE label).
- Hosts page: the filter no longer needs a syntax. A filter card combines
  (all must hold) a plain **search** (words anywhere in the host's name,
  state, labels, assignments or error), a **state** and an **assignment**
  choice (or *Unassigned*), and a **label selector** built like the
  Assignments page's (Loki-style matchers, Builder or Code; built-in
  `host` and `source` labels included); *Clear filters* resets them. The
  per-state counts above the table are gone (they are in the header).
- Live view: Assess / Reach on an assignment card opens the right panel
  at once with the run, followed live: command status (sending,
  accepted, refused with the reason), LIVE / FINISHED, a progress bar and
  counts (running, pending, compliant or success, not compliant or
  failed, other), and its hosts, those needing attention first; a click
  on a host shows it, "Back to the run" returns.
- Live view: the right panel floats over the map (resizable by its left
  edge): opening it no longer narrows the map.
- Live view: hosts are always grouped by assignment (the *Group by*
  choice, by label or state, is gone; hosts without assignment are in
  *(unassigned)*).
- Live view: the Assess / Reach buttons of the assignment cards show at
  once (they used to appear only after folding or unfolding when the
  assignments loaded after the hosts).
- Header: the fleet counts are no longer cut off when there are many
  states or on 1280 px screens (the jobs chart narrows first; below
  1440 px the "Jobs in progress" label, the chart's time window choice
  and the user name give way).
- Expected state lists (host page, Live view panel): attribute types are
  no longer broken mid-word in the narrow panel ("LineInFil e").

## all-in-one

- The new web client (see frontend) and the runner's Reach cap (see runner).
