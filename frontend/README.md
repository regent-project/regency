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

Theme: dark greys and blues around the logo's blue (`#1f4559`, variables
at the top of `style.css`).

Pages:

- **Map** (`/`, main page): live infrastructure graph, laid out like the
  former proof of concept: one card per group (assignment by default,
  inventory, label value or state) with its hosts as dots on rings around
  it, linked to it (no root node, no circle around a group). Dots are
  coloured by the host's compliance with its effective expected state
  (D23, `pages::host_state`): outdated (checked against an older one) like
  unknown, invalid (assignments in conflict) amber, unassigned dark grey.
  Grouped by assignment, the graph puts a host in its most specific
  assignment (last layer), the areas display in each of them; a host in
  several inventories gets dashed links to the others; a host with a job
  in progress (pending, running or abort requested) is drawn in the job
  colour instead, without animation.
  Cards have Assess/Reach buttons (a plain confirmation: each host runs its
  own effective state); an assignment's dialog also offers "Run an
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
  SDK pin moves). A click on a card folds it. Zoom (wheel, around the cursor), pan (drag),
  hover a host for a glance. Two panels around the map: on the left, jobs
  in progress (with Abort) and the activity of the last hour; on the right,
  fleet counts, the groups (a click centres the map on one) and the
  selected host. Clicking a host selects it (`/?host=<id>`, shareable) and
  shows its summary (state, assignments) and its effective expected state
  (`expected::HostStateExplorer`: the layers, lowest priority first, then
  the attributes tagged with their assignment, from `GetEffectiveState`) in
  the right panel, with Assess/Reach and a Details button to the host page; clicking empty space
  deselects it. Each panel shrinks to a thin rail (its ‹/› button; click the
  rail to bring it back), and "Hide panels" in the toolbar leaves only the
  map; the browser remembers the choice (local storage).
  A second display ("Display: areas", also remembered): each group is a
  rectangle holding its hosts in a grid, with its card (same
  counts, Assess/Reach, fold on click) in the top right corner; a host in
  several inventories is drawn in each of them (no links). In both
  displays hosts are in name order (they never move when a state changes),
  and host names zoom with the hosts (size in map units) once readable.
  Rendering (`src/map.rs`, `src/canvas.rs`), for thousands of hosts: group
  cards are SVG; hosts and links are drawn on a canvas below them (redrawn
  only on change, only what is in view; no animation, so running jobs
  cost nothing between changes). Live streams are applied in
  batches (`pages::next_batch`, 150 ms). Measured with 3000 hosts: what costs
  is pixels, not canvas calls: keep lines ≥ 1 px and tiny dots as squares.
- **Hosts** (`/hosts`): live table (filter by id, state, `label=value`,
  inventory); host detail (state, assignments, remediations, certificate
  approval, effective expected state with Assess/Reach now, outcome history,
  deletion of the host's data).
- **Jobs** (`/jobs`): live jobs of the last 24 h (abort).
- **Configuration** (`/config/...`):
  - *Hosts*: create, edit and delete host definitions of API/frontend
    sources (Git-sourced ones are read-only);
  - *Sources*: Git sources (repository, branch, path, poll interval,
    credentials reference), sync now;
  - *Services*: every instance (version, regent-sdk version, readiness,
    levels, last seen), its effective configuration with the origin of each
    key, and the TOML editor of each service's central configuration (D21);
  - *Security*: levels of every service, warnings, central external level;
  - *Settings*: retention.
