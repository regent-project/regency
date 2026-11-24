# regency frontend

Leptos, client-side rendering only (D14): static files served by any static
server, talking gRPC-Web to backend. The client is generated with prost from
`../regencylib/proto/regency/v1/api.proto` (needs `protoc`).

```bash
# from regency/frontend; trunk must use a toolchain with the wasm32 std
# (rustup's stable here; the system cargo has none)
PATH=~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH trunk build --release   # -> dist/
PATH=~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH trunk serve              # dev server
```

- Backend URL: `<meta name="regency-backend">` in `index.html` (empty = same
  origin); rewrite it at deployment.
- The static server must answer `index.html` for unknown paths (client-side
  routes such as `/hosts/<id>`).
- Not a default workspace member: `cargo build` at the workspace root skips it.

Look: a console like OpenShift / TailAdmin (masthead, left sidebar, page
title band, white cards). Two themes, light and dark, as CSS variables at the
top of `style.css` (`[data-theme]` on `<html>`: set before the first paint by
`index.html` from the saved choice or the system's, switched by the masthead
button, `main.rs::Theme`). The map canvas reads its colours from the same
variables (`canvas::Palette`, `--map-*`) and re-reads them when the theme
changes.

Pages:

- **Live view** (`/`, main page; "the map" in the code): live infrastructure graph, laid out like the
  former proof of concept: one card per assignment (hosts without one in
  "(unassigned)") with its hosts as dots on rings around
  it, linked to it (no root node, no circle around a group). Dots are
  coloured by the host's compliance with its effective expected state
  (D23, `pages::host_state`): outdated (checked against an older one) like
  unknown, invalid (assignments in conflict) amber, unassigned dark grey.
  Both displays show the same groups (`members`): a host in every
  assignment selecting it (D29), drawn in each;
  a host with a job
  in progress (pending, running or abort requested) is drawn in the job
  colour instead, without animation.
  Cards have Assess/Reach buttons (a plain confirmation: each host runs its
  own effective state; the run then opens in the right panel, followed
  live through `WatchJobs` on its command id, `src/runs.rs`; kept in localStorage until its jobs end, `KeptRun`, so a reload or another page does not lose it); an assignment's dialog also offers "Run an
  attribute…", the one-shot dialog (`src/oneshot.rs`: YAML attribute, Assess
  or Reach once on the assignment's hosts, live per-host results and
  outcomes, the last 20 runs remembered by the browser; history only, never
  a host's colour). Like Grafana's query editor it has two modes: **Builder**
  (`src/builder.rs`: forms generated from regent-sdk's attribute JSON Schema,
  backend `GetAttributeSchema`; pickers for `oneOf`/`enum`, optional fields
  on demand, flattened enums, lists, maps, raw JSON as a fallback; checked
  live through `ConvertAttributes`) and **Code** (the YAML); switching
  converts through backend. The attribute's `Detail` is shown as **Nature**, its picker
  grouped by category like regent-sdk's modules (`builder::CATEGORIES`; a
  test fails if a nature of the schema fixture has none). It starts empty
  (user's request): a blank
  attribute, choices not made ("choose…"), blank fields
  (`Schema::blank_for`); "To fill in: …" (`Schema::missing`) until valid. Builder tests use `src/testdata/attribute.schema.json`
  (regent-sdk 0.23.0's schema, as backend serves it; regenerate it when the
  SDK pin moves). A click on a card folds it; the toolbar's Fold / Unfold button folds
  every group, or unfolds them all once all are folded. Zoom (wheel, around
  the cursor), pan (drag), hover a host for a glance. The legend is a
  folded layer at the bottom left of the map. One panel, on the right: the
  groups (a click centres the map on one) and the selected host (jobs in
  progress and their Abort are on the Jobs page);
  on every page, the masthead shows the fleet counts (hosts per state) and
  the jobs-in-progress chart (`src/jobchart.rs`, compact form: pending /
  running now, the curve over the chosen range, hover for a time slot's
  values), from app-wide live feeds (`src/feeds.rs`: every host and the
  last hour's jobs, restarted 5 s after they end, finished jobs pruned
  after an hour) that the Live view and the Hosts page reuse. Clicking a host selects it (`/?host=<id>`, shareable) and
  shows its summary (state, assignments) and its effective expected state
  (`expected::HostStateExplorer`: its assignment and stack of expected
  states, base first, then the attributes tagged with their state and the
  states they override, and the suspected conflicts, from `GetEffectiveState`) in
  the right panel's "Last known state", each attribute coloured with its
  outcome in the host's last job and showing that job's lines for it when
  unfolded (`live::LastRun`, from the job's `GetRunLog`, matched by index and
  name), with Assess/Reach and a Details button to the host page; clicking empty space
  deselects it. The panel shrinks to a thin rail (its › button; click the
  rail to bring it back) and is resized by dragging its edge; the browser
  remembers both (local storage).
  A second display ("Display: areas", also remembered): each group is a
  rectangle holding its hosts in a grid, with its card (same
  counts, Assess/Reach, fold on click) in the top right corner; a host in
  several assignments is drawn in each of them (no links). A group with
  proactive and passive hosts splits them, proactive left, passive right
  (`is_active`; graph: `half_ring_offsets` either side of the card, areas:
  `split_cells` and `Area::split`, the dashed line). In both
  displays hosts are in name order (they never move when a state changes),
  and host names zoom with the hosts (size in map units), in full, once
  readable. Organize (toolbar): cards and hosts are picked (click, Ctrl +
  click, selection rectangle on empty space: `map::Picks`, `Layout::in_box`)
  and dragged together where the user wants (`map::Arrangement`: a moved card's position, a moved host's
  offset from its card, per display, in local storage); Auto-arrange
  forgets them. Folding / unfolding glides for 200 ms (`map::Motion`, the
  only canvas animation: frames are drawn only while it lasts).
  Rendering (`src/map.rs`, `src/canvas.rs`), for thousands of hosts: group
  cards are SVG; hosts and links are drawn on a canvas below them (redrawn
  only on change, only what is in view; no animation, so running jobs
  cost nothing between changes). Live streams are applied in
  batches (`pages::next_batch`, 150 ms). Measured with 3000 hosts: what costs
  is pixels, not canvas calls: keep lines ≥ 1 px and tiny dots as squares.
- **Hosts** (`/hosts`): one page for hosts, observed or defined: live
  table of the host docs plus the defined hosts that never ran
  (`src/hostdefs.rs`: state *never run*, labels and assignments computed
  from the catalog; *Address* and *Defined by* columns) (filter by id, state, `label=value`,
  assignment); `/hosts?state=<state>` shows only the hosts in that
  state (exactly, removable chip), the target of the masthead's fleet counts; host detail (state, assignments, remediations, certificate
  approval, its definitions: view, edit, add, delete (`hostdefs::HostDefinitions`;
  Git-sourced ones read-only), effective expected state with Assess/Reach
  now, outcome history, deletion of the host's data).
- **Jobs** (`/jobs`): live jobs of the last 24 h (abort).
- **Configuration** (`/config/...`):
  - *Expected states*, *Assignments* (`src/catalog.rs`): the same for the
    other catalog items (D11, D23, D29); expected states as YAML checked
    live through `ConvertAttributes`; an assignment's hosts are a label
    selector (D26/D29) evaluated in the browser by the same code as the
    services (`regencylib/src/labels.rs`, compiled in as `crate::labels`),
    edited with a Grafana-like filter builder (`src/labelpicker.rs`:
    label / operator / value comboboxes fed by the labels in use) or as
    text, next to a "Matching hosts" panel (count, searchable list of the
    hosts with their labels, and for a searched host that does not match,
    the filter excluding it); its priority orders it among a host's
    assignments;
    Assess / Reach on its hosts (`/inventories` now opens this page);
  - *Sources*: Git sources (repository, branch, path, poll interval,
    credentials reference), sync now;
  - *Services*: every instance (version, regent-sdk version, readiness,
    levels, last seen), its effective configuration with the origin of each
    key, and the TOML editor of each service's central configuration (D21);
  - *Security*: levels of every service, warnings, central external level;
  - *Settings*: retention.
