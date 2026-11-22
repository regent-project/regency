# Changelog

One version for the whole of regency: every service, `regencyctl`, the web
client and the all-in-one package share it. This file holds **only the
release being prepared** (`# Unreleased`, renamed `# Release v<version>` when
it is released), with one `##` subsection per component (write "No change."
for a component that did not change); it becomes the GitHub release notes.
Each new release starts this file fresh: the notes of earlier releases are
the GitHub releases (and this file's git history). See RELEASING.md.

# Release v0.7.1

**Every host of a large run now shows on the Live view.** During a
Reach or Assess on thousands of hosts, the web client could miss job
updates: some hosts never turned the job colour, and some jobs could stay
*pending* or *running* in it although they had finished. Fixes only: no
configuration, message or document change; services of 0.7.0 and 0.7.1
work together and the upgrade needs no action.

## scheduler

No change.

## runner

No change.

## dbwriter

No change.

## backend

- Job updates no longer get lost when a client reads more slowly than jobs
  change (e.g. a Reach on thousands of hosts with the Live view open). Before,
  some jobs could stay *pending* or *running* in the web client, or never show
  as running at all. Each live view of jobs now keeps the latest update of
  every job until the client reads it, and reads the recent jobs again should
  it still fall behind.

## endpoint

No change.

## sourcesync

No change.

## regencyctl

No change.

## frontend

- Live view: a host whose job finished before the web client saw it running
  (very quick runs, or intermediate states merged under load) now briefly
  turns the job colour too, so every host of a run visibly takes part in it.

## all-in-one

- Includes the backend and frontend fixes above.
