# Changelog

One version for the whole of regency: every service, `regencyctl`, the web
client and the all-in-one package share it. This file holds **only the
release being prepared** (`# Unreleased`, renamed `# Release v<version>` when
it is released), with one `##` subsection per component (write "No change."
for a component that did not change); it becomes the GitHub release notes.
Each new release starts this file fresh: the notes of earlier releases are
the GitHub releases (and this file's git history). See RELEASING.md.

# Release v0.5.0

A new look for the web client: a classic console (in the spirit of
OpenShift / TailAdmin) with light and dark themes, fewer controls on the
Live view (formerly Map), and the fleet state and jobs in progress always
in sight in the masthead. Web client only: services, messages, documents
and configuration are unchanged; no action needed when upgrading.

## scheduler

No change.

## runner

No change.

## dbwriter

No change.

## backend

No change (it serves the new web client when `[backend] frontend_dir` is
set).

## endpoint

No change.

## sourcesync

No change.

## regencyctl

No change.

## frontend

- New console look, in the spirit of OpenShift / TailAdmin: dark masthead
  and left sidebar (Live view, Hosts, Jobs, then the Configuration pages, which
  replace the configuration tabs), page titles in a band, white cards and
  tables with soft shadows, tinted status labels.
- Light and dark themes: the masthead button switches them; the first
  choice follows the system's, then the browser remembers it. The map
  follows the theme too.
- The sidebar folds to icons (☰ button, remembered; always folded on narrow
  screens).
- Map: the fleet counts (hosts per state) and the jobs-in-progress chart
  (pending / running now, curve over 2 min to 1 h, values on hover) moved
  from the side panels to the masthead, shown on every page. They come
  from app-wide live feeds (every host, last hour's jobs), shared with the
  Live view and the Hosts page (one stream each instead of one per page),
  and restarted automatically 5 s after they end ("⚠ reconnecting…").
  The fleet states are coloured buttons (the legend's dot colours, in
  legend order, with the count inside; dot and count only below 1500 px),
  each opening the Hosts page
  filtered on that state (`/hosts?state=…`, shown as a removable chip; the
  state shown is highlighted in the masthead).
- Map, now **Live view** in the menu, with fewer controls: the left panel
  is gone (its jobs in progress, Abort and activity are on the Jobs page),
  the legend is a folded layer at the bottom left of the map, "Fold all" /
  "Unfold all" became one Fold / Unfold button, and the "Hide panels" and
  "Fit" buttons are gone (changing the display still recentres the map).

## all-in-one

- New web client, see frontend.
