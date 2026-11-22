# Changelog

One version for the whole of regency: every service, `regencyctl`, the web
client and the all-in-one package share it. This file holds **only the
release being prepared** (`# Unreleased`, renamed `# Release v<version>` when
it is released), with one `##` subsection per component (write "No change."
for a component that did not change); it becomes the GitHub release notes.
Each new release starts this file fresh: the notes of earlier releases are
the GitHub releases (and this file's git history). See RELEASING.md.

# Release v0.6.0

**Assignments select hosts by labels and add up** (D26, D27, D29): label a
host and it gets the expected states of every assignment selecting it,
like a Salt top file targeting grains. An assignment is a label selector in
the style of Loki or Kubernetes (`{env="prod", role=~"web|api"}`, `{}`
being every host), a priority, and a stack of expected states like Helm
values files: the first is the base, each next one overrides the previous
ones (same attribute `Name`). A host's assignments are applied by priority
(lower first), then by name; an attribute two of them declare identically
is applied once (whatever its `Name`), one they set differently (same
`Name`, or the same target, e.g. a service Started vs Stopped) is a
conflict and the host gets no task until it is fixed. The final state
shows where each attribute comes from (state, assignment) and which states
it overrides; suspected conflicts inside a stack are pointed out.
Inventories are gone: an assignment's selector replaces them.

**Proactive hosts join the fleet model** (D28): a regops host reports its
labels (`[Regency] labels`, regops 0.17.0), so it gets assignments like
any host; its colour is its own pass's compliance, it is
drawn as a diamond on the map, and regency shows whether it runs the latest
commit of its repository (when that repository is a regency Git source)
and whether its Git state differs from its effective state.

Ready to manage hosts right after installation, both ways: the runner
generates its own SSH key and the new **Add host** button of the Hosts
page shows it with the
command authorizing it on a host, and gives regops agents everything they
need (enrollment token, the endpoint's CA certificate, their `[Regency]`
configuration). The whole catalog can also be managed from the web client:
expected states and assignments get their own pages, so a host can be defined, given an expected state and assessed
without Git or the API.

Upgrading:

- **Catalog (D26, D29), action needed.** Git sources: move the host lists
  from `inventories/` to `hosts/` (same format), delete `inventories/`, and
  give each assignment its hosts as `Selector: '{...}'` (see the book,
  *Sources*). Until then the sync reports the old files as errors and
  keeps what it had. Inventories and assignments created through the API
  or the web UI in the former shape are ignored (logged as unreadable):
  recreate the assignments with a selector. Host definitions keep
  working; their former `inventories` list is ignored, so give the hosts
  labels. Inventory defaults (connection method, variables, pool) no
  longer exist: they belong to each host (in Git, top-level values of a
  `hosts/` file are copied into its hosts). Access policies naming an
  `Inventory` must name an `Assignment` (or a `Source`) instead.
- **Assignments (D27, D29), check your hosts.** `Priority` now orders a
  host's assignments (lower first), whose final states add up; an
  assignment no longer overrides another: put states meant to override
  each other in one stack (`ExpectedStates: [base, web]`, the last wins;
  `ExpectedState:` still works for one). A host whose assignments set an
  attribute differently gets no task (flagged) until fixed. Two
  assignments with the same priority and name (from two sources) are
  refused.
- **regent-sdk 0.26.0** (D28: the agent's report gains labels and the
  state's source). Proactive hosts need regops 0.17.0 to report them;
  older ones keep working, without labels (so unassigned).
- The all-in-one's runner generates its SSH key at
its next start; an existing endpoint certificate is kept (agents reaching
it by another name get `server_name = "regency"` in the configuration the
dialog gives). For per-component packages, `ssh_key` is in the new default
`/etc/regency/runner.toml`; an edited one keeps its content (add the key to
enable it).

## scheduler

- Orders run the hosts their assignment's label selector matches (D29); no
  inventory defaults are applied any more (a host's connection method,
  variables and pool are its own).
- A host's expected state is the sum of the final states of every
  assignment selecting it, by priority then name (D27, D29); hosts with
  conflicting or ambiguous assignments are skipped.

## runner

- New `[runner] ssh_key`: the runner's SSH key file, generated (ed25519,
  OpenSSH format, `<file>.pub` beside it) at startup when missing. Its
  public key is published in the instance status, for the web client's
  **Add host** dialog. Set by the all-in-one
  (`/var/lib/regency/secrets/runner-ssh-key`) and the packages' default
  configuration (`/var/lib/regency/runner/ssh-key`).

## dbwriter

- Checks catalog items before writing them: names, label keys (the
  built-in `host` and `source` cannot be set), references; inventories are
  refused (D29).
- Keeps a proactive host's newest agent report on its host doc (D28:
  labels, repository, branch, file, commit, applied state; label keys that
  are invalid or built-in are dropped and logged), and each Git source's
  synchronized commit (`source_head:` docs).

## backend

- Assignments select hosts by labels and add up (D26, D29): host
  summaries list every assignment of a host in order (their `inventories`
  field is gone); host selectors (live views, runs) take assignments and a
  label selector (`selector`, Loki style) instead of inventories; Cedar's
  hierarchy is `Source` ⊃ `Assignment` ⊃ `Host`. Catalog items are checked
  before they are accepted; an assignment with the priority and name of
  another source's is refused.
- Assignments stack expected states (D27): new RPC `ComposeStates` (the
  final state of a stack, for the UI); `GetEffectiveState` gives the layers
  with their assignment's priority, the blame of each attribute (its
  state, the states it overrides, its assignment, the later assignments
  declaring it too) and the suspected conflicts.
- New setting `endpoint_url` (`https://…`, empty = derived): the endpoint
  address shown to regops agents.
- Proactive hosts (D28): their agent's labels join their definitions'
  (which win on the same key; `source` gains `regops`), for assignments,
  live-view selectors and Cedar. Host summaries gain the
  agent, its state's source and commit, the repository status
  (`up_to_date` / `behind` / `unknown`, against the regency Git source on
  the same repository and branch) and the drift of the applied state from
  the host's effective state.

## endpoint

- Publishes in its instance status what regops agents need: its port, the
  names of its server certificate and the built-in CA's certificate.

## sourcesync

- New Git layout (D26, D29): `hosts/*.yaml` (regent-sdk inventory files
  plus `Labels` and `Pool`; top-level values copied into each host),
  assignments with `Selector`, `Priority` and `ExpectedStates` (base first)
  or `ExpectedState`. Files in a former layout (`inventories/`, `Inventory`
  or `Hosts` in an assignment) are reported as errors (nothing deleted);
  so is an assignment with the priority and name of another source's.
- Reports each source's synchronized commit to dbwriter (D28), the head
  proactive hosts on the same repository are compared with.

## regencyctl

No change.

## frontend

- The menu follows regency's model: **Live view, Hosts, States,
  Assignments** first (new addresses `/states`, `/assignments`; the former
  `/config/...` ones still work, `/inventories` opens Assignments), then
  *Activity* (Jobs) and *Administration* (Host definitions, Sources,
  Services, Security, Settings).
- Hosts page: new **Add host** button, opening a dialog with two tabs:
  - **Over SSH**: the public key of each runner pool's runners with a ready
    command authorizing it on a host, and a short form defining the host
    (id, address, user, pool, labels, host key fingerprints)
    with the runners' key as its credentials;
  - **With regops**: an enrollment token for a host id, the commands writing
    the endpoint's CA certificate and the token on the host, and the
    `[Regency]` lines of its configuration (endpoint URL, `server_name`
    when the address is not in the endpoint's certificate).
- Settings: **Endpoint URL** for regops agents.
- Host definitions: no more inventory list on a host (its labels decide
  its assignments); the labels `host` and `source` are refused.
- Host definitions: a new host's SSH template uses the runners' key when
  the default pool has one.
- New pages for the catalog items of API/frontend sources (items of Git
  sources are listed read-only, edited in Git):
  - **Expected states**: create, edit, delete; the attributes are written
    as regent-sdk YAML, checked as you type by backend (the SDK's serde,
    inline credentials refused) before saving; the assignments using each
    state are listed (and named before deleting one).
  - **Assignments**: create, edit, delete, listed in the order they apply
    (priority, then name). The hosts are a label selector (D29) built like
    Grafana's label filters on Loki: the filters are chips on one line
    (`env="prod"`), each edited with a label, an operator and a value
    picked from lists of the labels and values in use (opened on a click,
    narrowed as you type; any value can be typed), or written as text
    (**Code**); the stack of expected states is a line of chips too
    (◀ ▶ to reorder); the form is laid out like a sheet (source, name,
    priority on top; Hosts and Expected states side by side, rows aligned); **Matching hosts** counts the hosts it
    selects and lists them with their labels (those filtered on
    highlighted), with a search: a searched host that does not match is
    shown with the filter excluding it and its actual value. A priority
    orders it among a host's assignments. The expected states are picked
    from the catalog (create them first) and form an ordered stack
    (reorder, remove, add), the last winning (D27). Next to the form, and
    when an assignment's name is clicked, **Final expected state**: the
    hosts it selects, suspected conflicts (⚠), and each attribute with the state it
    comes from and the states it overrides. Saving an assignment with the
    priority and name of another source's is refused. **Assess** /
    **Reach** now on an assignment's hosts.
- Host page and Live view panel: the host's assignments in order, each
  with its stack, its attributes with their state, assignment, overrides
  and the assignments declaring them too, and the suspected conflicts.
- Live view: grouping by inventory is gone; grouped by assignment, a host
  with several assignments is drawn around its last one (graph) or in each
  (areas). The Hosts table shows assignments instead of inventories.
- Pages no longer open with explanation paragraphs (the book has them).
- Live view: the right panel appears only for a host selected by a click
  (hovering shows the floating tip only; the panel's group list is gone,
  the map takes the whole width otherwise); the menu's Live view icon is a
  red "recording" mark.
- Proactive hosts (D28): coloured by their last pass, assigned or not;
  drawn as diamonds, with an amber outline when behind their repository;
  their panel and page show the agent, the source of their state, the
  commit (up to date / behind / unknown) and the drift from their
  effective state. Legend updated.
- Lists reload by themselves once dbwriter has applied a change.
- Host page: a host that is defined but has never run (no outcome yet) shows
  its effective expected state and can be assessed or reached from there;
  until now it showed nothing and its buttons stayed disabled.

## all-in-one

- The runner gets its SSH key at first start (see runner).
- The endpoint's certificate, generated at first start, also names the
  machine's hostname, FQDN and IP addresses (`[prepare.tls] local_names`),
  so regops agents can reach it by any of them.
- `install.sh`: its final summary points to **Hosts → Add host** (the
  runner's SSH key, regops enrollment) instead of the bare endpoint address.
