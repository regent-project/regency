# Changelog

One version for the whole of regency: every service, `regencyctl`, the web
client and the all-in-one package share it. This file holds **only the
release being prepared** (`# Unreleased`, renamed `# Release v<version>` when
it is released), with one `##` subsection per component (write "No change."
for a component that did not change); it becomes the GitHub release notes.
Each new release starts this file fresh: the notes of earlier releases are
the GitHub releases (and this file's git history). See RELEASING.md.

# Release v0.8.0

**A web client made for daily work on the fleet.** Host definitions and
hosts are now one Hosts page, the Add host dialog also covers SSHOQ, and the
Live view map can be organized by hand, shows hosts' full names and tells,
attribute by attribute, what the last job found on a host. Also fixes
`LineInFile` lines starting with `-` (regent-sdk 0.26.1) and a false drift
warning on proactive hosts. No configuration, message or document change.

**Upgrade the scheduler and the runners together** (the all-in-one does):
regency 0.8.0 is built on regent-sdk 0.26.1, and a runner refuses tasks
built with another regent-sdk version (they are recorded as errors), so
tasks scheduled by a 0.7.1 scheduler fail on a 0.8.0 runner and the
reverse. The other services of 0.7.1 and 0.8.0 work together.

## scheduler

- Built on regent-sdk 0.26.1 (see runner): upgrade it together with the
  runners.

## runner

- regent-sdk 0.26.1: a `LineInFile` line starting with `-` (e.g. an iptables
  rule such as `-A INPUT -m conntrack --ctstate INVALID -j DROP`) or holding
  a single quote is now found on the host. Before, such an attribute was
  always reported not compliant and each Reach appended the line again.

## dbwriter

No change.

## backend

- Proactive hosts: no more false *drift* warning when the assignment's
  expected state uses templates (e.g. `{{ host }}` in a file path). The agent
  reports the state it applied with the templates rendered, and each of
  those attributes counted as *different*. A template now matches its
  rendered value (the same variable must have the same value throughout the
  attribute).
- New RPC `RevokeApiToken`. A revoked API token is refused at once, even
  when it was verified a moment before.
- `SetSettings` checks the certificate deny lists (`revoked_hosts`,
  `revoked_serials`, `revoked_fingerprints`) and writes serials and
  fingerprints the way the endpoint compares them (lowercase hex serials,
  `AB:CD:…` fingerprints), so an entry typed differently still matches.

## endpoint

No change.

## sourcesync

No change.

## regencyctl

No change.

## frontend

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
- Live view map:
  - **Full host names** (no longer cut to 8 characters; shown from the same
    zoom level as before).
  - **Organize** mode: drag group cards and hosts where you want them.
    Several at once with Ctrl + click or a selection rectangle (drag on
    empty space; the middle button pans in this mode). The browser remembers
    the arrangement per display; **Auto-arrange** returns to the automatic
    layout.
  - Folding and unfolding a group glides (200 ms) and no longer re-zooms
    the view.
  - Host panel: *Expected state* and *Last job log* merged into **Last known
    state**. Each attribute of the host's effective state is coloured with
    its outcome in the last job (compliant or fixed, not compliant, failed,
    aborted, not checked). Unfold an attribute for its definition and what
    the last job did with it (commands, output, verdict).
  - Proactive (regops) and passive hosts apart: in a group holding both,
    proactive hosts are on the left and passive ones on the right (graph:
    two half-ring lobes either side of the card, like ∞; areas: two columns
    with a dashed line between them).
  - A run started from the map is no longer lost on a reload or a visit
    to another page: until all its jobs have ended, its panel opens again
    with the Live view, and a "Job in progress" button per run, at the
    right end of the toolbar, brings it back.
  - The graph display shows the same groups as the areas display: a host
    is drawn in every assignment selecting it, no longer only in its most
    specific one.
- The Live view icon of the menu slowly fades to red and back only while the
  Live view is shown.
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
- Live view map: hosts are *compliant* or *not compliant* (*reach success*
  and *reach failed* stay in the host's details); the hover tip shows the
  host's state once.
- The "Job in progress" button of a run disappears when its jobs end, even
  if its panel was closed.

## all-in-one

- Includes the runner, backend and web client changes above.
