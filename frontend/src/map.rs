//! Main page: live infrastructure map, in two displays. Graph: each
//! assignment (or label value, state) is a node (a card with its
//! compliance and Assess/Reach buttons); its hosts are dots on rings around
//! it, linked to it, colored by their compliance against their effective
//! expected state (D23; grey: unassigned or outdated), in name order from
//! the inner ring outwards; a host in several assignments (D29) is drawn
//! around its last one (the most specific). Areas: each group is a
//! rectangle holding its hosts in a grid, in name order (they never move
//! when a state changes), its card in the top right corner; a host in
//! several assignments is drawn in each of them. A host with a job
//! in progress (PENDING, STARTED or ABORT_REQUIRED) is drawn in the job
//! colour, without animation (redrawing rings every frame cost too much at
//! scale). Zoom with the wheel,
//! pan by dragging, click a card to fold/unfold its hosts.
//!
//! Shown as "Live view". The legend is a layer on top of the map; one panel,
//! on the right: groups and the selected host (the fleet counts and the
//! jobs-in-progress chart are in the masthead, `crate::feeds`; jobs in
//! progress and their Abort on the Jobs page). Clicking a host
//! selects it (`/?host=<id>`): its summary and expected state explorer show
//! in the right panel, "Details" opens the host page.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use js_sys::wasm_bindgen::JsCast;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};

use crate::canvas::{self, Frame, Palette, View};
use crate::client::{api, authed, client, command_id, format_ms};
use crate::expected::{
    AssignmentEntry, GroupTarget, HostStateExplorer, describe_selector, job_label, load_assignments,
};
use crate::feeds::Feeds;
use crate::live::{HostLive, HostTip, RunLogView, running_jobs};
use crate::oneshot::OneShotDialog;
use crate::pages::{assignments_line, badge, host_state};
use crate::{Session, Theme};

pub const DOT: f64 = 7.0;
/// Distance between two hosts on a ring and between two rings.
const RING_SPACING: f64 = 24.0;
/// Radius of the first ring of hosts around a group card.
const FIRST_RING: f64 = 150.0;
const CARD_W: f64 = 236.0;
const CARD_H: f64 = 124.0;
/// Free space between two groups.
const GAP: f64 = 70.0;
/// Areas display: grid step of the hosts, inner margin and free space
/// between two areas.
const CELL: f64 = 24.0;
const AREA_PAD: f64 = 16.0;
const AREA_GAP: f64 = 40.0;

fn is_ok(state: &str) -> bool {
    matches!(state, "compliant" | "reach_success")
}

fn is_bad(state: &str) -> bool {
    matches!(state, "not_compliant" | "reach_failed" | "error")
}

/// How the map shows its groups.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Display {
    /// Cards with their hosts on rings around them, linked to them.
    Graph,
    /// Rectangles holding their hosts.
    Areas,
}

impl Display {
    const KEY: &str = "regency.map.display";

    fn saved() -> Self {
        match crate::client::storage().and_then(|s| s.get_item(Self::KEY).ok().flatten()) {
            Some(v) if v == "areas" => Display::Areas,
            _ => Display::Graph,
        }
    }

    fn save(self) {
        if let Some(storage) = crate::client::storage() {
            let _ = storage.set_item(Self::KEY, self.name());
        }
    }

    fn name(self) -> &'static str {
        match self {
            Display::Graph => "graph",
            Display::Areas => "areas",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GroupBy {
    Assignment,
    Label,
    State,
}

/// Colour of a host's dot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Ok,
    Bad,
    Warn,
    Unknown,
    /// No assignment: nothing is checked (D23).
    Unassigned,
}

impl Tone {
    /// The CSS colour of the legend and the canvas (`canvas::Palette`).
    pub fn css(self) -> &'static str {
        match self {
            Tone::Ok => "var(--ok)",
            Tone::Bad => "var(--bad)",
            Tone::Warn => "var(--warn)",
            Tone::Unknown => "var(--unknown)",
            Tone::Unassigned => "var(--unassigned)",
        }
    }

    /// Legend order: compliant first, unassigned last.
    pub fn rank(self) -> u8 {
        match self {
            Tone::Ok => 0,
            Tone::Bad => 1,
            Tone::Warn => 2,
            Tone::Unknown => 3,
            Tone::Unassigned => 4,
        }
    }
}

/// Colour of a host state (`pages::host_state`), as on the map.
pub fn tone(state: &str) -> Tone {
    if is_ok(state) {
        Tone::Ok
    } else if is_bad(state) {
        Tone::Bad
    } else if matches!(state, "aborted" | "invalid") {
        Tone::Warn
    } else if state == "unassigned" {
        Tone::Unassigned
    } else {
        Tone::Unknown
    }
}

/// Ring of a host with a job in progress.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ring {
    Pending,
    Started,
    AbortRequired,
}

impl Ring {
    fn of(state: &str) -> Option<Ring> {
        match state {
            "PENDING" => Some(Ring::Pending),
            "STARTED" => Some(Ring::Started),
            "ABORT_REQUIRED" => Some(Ring::AbortRequired),
            _ => None,
        }
    }

    fn state(self) -> &'static str {
        match self {
            Ring::Pending => "PENDING",
            Ring::Started => "STARTED",
            Ring::AbortRequired => "ABORT_REQUIRED",
        }
    }
}

/// Hosts with a job in progress (their newest job), drawn in the job colour.
pub type Rings = HashMap<String, Ring>;

fn rings(jobs: &BTreeMap<String, api::JobSummary>) -> Rings {
    let mut newest: HashMap<&str, &api::JobSummary> = HashMap::with_capacity(jobs.len());
    for job in jobs.values() {
        let entry = newest.entry(job.host_id.as_str()).or_insert(job);
        if job.created_at > entry.created_at {
            *entry = job;
        }
    }
    newest
        .into_iter()
        .filter_map(|(host, job)| Some((host.to_string(), Ring::of(&job.state)?)))
        .collect()
}

/// A host on the map: what the canvas draws, no copy of its summary. Job
/// rings are not part of the layout ([`Rings`]): job events, the most
/// frequent ones, do not move anything.
#[derive(Clone, PartialEq)]
pub struct Placed {
    pub host_id: String,
    pub tone: Tone,
    /// Conflicting definitions or certificate pending.
    pub flag: bool,
    /// D28: a proactive host (its agent reports itself): drawn as a diamond.
    pub proactive: bool,
    /// D28: its agent's repository is behind the regency Git source's head.
    pub behind: bool,
    pub x: f64,
    pub y: f64,
    /// Center of its group (the end of its link).
    pub gx: f64,
    pub gy: f64,
}

#[derive(Clone, PartialEq)]
struct Group {
    name: String,
    /// What the Assess/Reach buttons select; `None` = no buttons.
    target: Option<GroupTarget>,
    /// Center of its card.
    x: f64,
    y: f64,
    radius: f64,
    /// Its rectangle (areas display).
    area: Option<Area>,
    total: usize,
    ok: usize,
    bad: usize,
    collapsed: bool,
}

/// A group's rectangle in the areas display (top left corner and size).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub folded: bool,
}

#[derive(Clone, PartialEq, Default)]
pub struct Layout {
    groups: Vec<Group>,
    /// Hosts of the unfolded groups (areas display: one per group of the
    /// host, so a host may be placed more than once).
    pub hosts: Vec<Placed>,
    /// Draw the links of hosts to their group (graph display).
    pub links: bool,
    /// Rectangles of the groups (areas display), drawn under the hosts.
    pub areas: Vec<Area>,
    /// Every host (folded ones too): its groups, and its index in `hosts`
    /// in each of them.
    of_host: HashMap<String, Vec<(usize, Option<usize>)>>,
    /// min x, min y, max x, max y
    bounds: (f64, f64, f64, f64),
}

impl Layout {
    /// Every place where host `host_id` is drawn.
    pub fn placements<'a>(&'a self, host_id: &str) -> impl Iterator<Item = &'a Placed> + 'a {
        self.of_host
            .get(host_id)
            .into_iter()
            .flatten()
            .filter_map(|(_, index)| self.hosts.get((*index)?))
    }

    /// Hosts with a job in progress, per group.
    fn running(&self, rings: &Rings) -> Vec<usize> {
        let mut counts = vec![0; self.groups.len()];
        for host in rings.keys() {
            for (group, _) in self.of_host.get(host).into_iter().flatten() {
                counts[*group] += 1;
            }
        }
        counts
    }

    /// The host whose dot is nearest to (`x`, `y`), within `radius` (map units).
    pub fn host_at(&self, x: f64, y: f64, radius: f64) -> Option<&Placed> {
        let reach = radius * radius;
        self.hosts
            .iter()
            .map(|p| (p, (p.x - x).powi(2) + (p.y - y).powi(2)))
            .filter(|(_, d)| *d <= reach)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(p, _)| p)
    }

    fn grow(&mut self, x0: f64, y0: f64, x1: f64, y1: f64) {
        let b = &mut self.bounds;
        b.0 = b.0.min(x0);
        b.1 = b.1.min(y0);
        b.2 = b.2.max(x1);
        b.3 = b.3.max(y1);
    }

    fn empty() -> Self {
        Layout {
            bounds: (f64::MAX, f64::MAX, f64::MIN, f64::MIN),
            ..Layout::default()
        }
    }

    fn place(&mut self, group: usize, host: &api::HostSummary, x: f64, y: f64, gx: f64, gy: f64) {
        self.of_host
            .entry(host.host_id.clone())
            .or_default()
            .push((group, Some(self.hosts.len())));
        self.hosts.push(Placed {
            host_id: host.host_id.clone(),
            tone: tone(host_state(host)),
            flag: host.conflicting || !host.pending_fingerprints.is_empty(),
            proactive: !host.agent.is_empty(),
            behind: host.repo_status == "behind",
            x,
            y,
            gx,
            gy,
        });
    }

    fn fold(&mut self, group: usize, host: &api::HostSummary) {
        self.of_host
            .entry(host.host_id.clone())
            .or_default()
            .push((group, None));
    }
}

/// Positions of `n` hosts on concentric rings around a card, and the radius
/// of the outer ring.
fn ring_offsets(n: usize) -> (Vec<(f64, f64)>, f64) {
    let mut out = Vec::with_capacity(n);
    let mut radius = FIRST_RING;
    let mut outer = FIRST_RING;
    let mut ring = 0usize;
    while out.len() < n {
        let capacity = ((std::f64::consts::TAU * radius) / RING_SPACING).floor() as usize;
        let take = capacity.min(n - out.len());
        // rings alternate their start angle so dots do not line up radially
        let shift = if ring.is_multiple_of(2) { 0.0 } else { 0.5 };
        for k in 0..take {
            let a = -std::f64::consts::FRAC_PI_2
                + (k as f64 + shift) / take as f64 * std::f64::consts::TAU;
            out.push((radius * a.cos(), radius * a.sin()));
        }
        outer = radius;
        radius += RING_SPACING;
        ring += 1;
    }
    (out, outer)
}

fn card_radius() -> f64 {
    (CARD_W * CARD_W + CARD_H * CARD_H).sqrt() / 2.0 + 10.0
}

/// A group before placement: its hosts, worst first.
struct Members<'a> {
    name: String,
    target: Option<GroupTarget>,
    hosts: Vec<&'a api::HostSummary>,
    collapsed: bool,
}

impl Members<'_> {
    fn counts(&self) -> (usize, usize, usize) {
        let ok = self.hosts.iter().filter(|h| is_ok(host_state(h))).count();
        let bad = self.hosts.iter().filter(|h| is_bad(host_state(h))).count();
        (self.hosts.len(), ok, bad)
    }
}

/// The groups of `hosts`. A host in several assignments belongs to its last
/// one only (the most specific, D29), unless `every_group`. `assignments`:
/// selector of each assignment (D23), for its Assess / Reach.
fn members<'a>(
    hosts: &'a BTreeMap<String, api::HostSummary>,
    group_by: GroupBy,
    label_key: &str,
    collapsed: &BTreeSet<String>,
    every_group: bool,
    assignments: &BTreeMap<String, api::HostSelector>,
) -> Vec<Members<'a>> {
    let mut members: BTreeMap<String, (Option<GroupTarget>, Vec<&api::HostSummary>)> =
        BTreeMap::new();
    for host in hosts.values() {
        let groups: Vec<(String, Option<GroupTarget>)> = match group_by {
            GroupBy::Assignment if host.assignments.is_empty() => {
                vec![("(unassigned)".to_string(), None)]
            }
            // graph: one group per host, its most specific assignment (the
            // last applied); areas: every assignment
            GroupBy::Assignment => host
                .assignments
                .iter()
                .rev()
                .take(if every_group { usize::MAX } else { 1 })
                .map(|reference| {
                    let target = assignments.get(reference).map(|selector| {
                        GroupTarget::Assignment(reference.clone(), selector.clone())
                    });
                    (reference.clone(), target)
                })
                .collect(),
            GroupBy::Label => vec![match host.labels.get(label_key) {
                Some(value) => (
                    format!("{label_key}={value}"),
                    Some(GroupTarget::Label(label_key.to_string(), value.clone())),
                ),
                None => (format!("(no {label_key})"), None),
            }],
            GroupBy::State => vec![(host_state(host).replace('_', " "), None)],
        };
        for (name, target) in groups {
            members
                .entry(name)
                .or_insert_with(|| (target, Vec::new()))
                .1
                .push(host);
        }
    }
    members
        .into_iter()
        // hosts in name order (the map's), whatever their state
        .map(|(name, (target, hosts))| Members {
            collapsed: collapsed.contains(&name),
            name,
            target,
            hosts,
        })
        .collect()
}

/// Shelf packing: the top left corner of each `(width, height)` box, in
/// rows about as wide as the whole is tall, rows centered on x = 0. Each box
/// is centered vertically in its row when `center` is set.
fn shelves(sizes: &[(f64, f64)], gap: f64, center: bool) -> Vec<(f64, f64)> {
    let area: f64 = sizes.iter().map(|(w, h)| (w + gap) * (h + gap)).sum();
    let widest = sizes.iter().map(|(w, _)| w + gap).fold(0.0, f64::max);
    let row_width = (area.sqrt() * 1.3).max(widest);
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut x = 0.0;
    for (i, (w, _)) in sizes.iter().enumerate() {
        if x + w + gap > row_width && x > 0.0 {
            rows.push(Vec::new());
            x = 0.0;
        }
        x += w + gap;
        rows.last_mut().expect("one row").push(i);
    }
    let mut out = vec![(0.0, 0.0); sizes.len()];
    let mut y = 0.0;
    for row in rows {
        let height = row.iter().map(|i| sizes[*i].1 + gap).fold(0.0, f64::max);
        let width: f64 = row.iter().map(|i| sizes[*i].0 + gap).sum();
        let mut x = -width / 2.0;
        for i in row {
            let (w, h) = sizes[i];
            let dy = if center {
                (height - h - gap) / 2.0
            } else {
                0.0
            };
            out[i] = (x + gap / 2.0, y + gap / 2.0 + dy);
            x += w + gap;
        }
        y += height;
    }
    out
}

fn layout(
    hosts: &BTreeMap<String, api::HostSummary>,
    display: Display,
    group_by: GroupBy,
    label_key: &str,
    collapsed: &BTreeSet<String>,
    assignments: &BTreeMap<String, api::HostSelector>,
) -> Layout {
    let groups = |every| members(hosts, group_by, label_key, collapsed, every, assignments);
    let mut layout = match display {
        // name order in both displays: a host changing state stays where it
        // is
        Display::Graph => graph_layout(groups(false)),
        Display::Areas => areas_layout(groups(true)),
    };
    if layout.groups.is_empty() {
        layout.bounds = (-200.0, -200.0, 200.0, 200.0);
    }
    // graph, hosts in several assignments: no links to the other ones
    // (every host would link to the generic ones, e.g. a baseline on every
    // host); areas draw them in each
    layout
}

/// Graph display: a card per group, its hosts on rings around it.
fn graph_layout(groups: Vec<Members>) -> Layout {
    let rings: Vec<(Vec<(f64, f64)>, f64)> = groups
        .iter()
        .map(|g| {
            let (offsets, outer) = if g.collapsed {
                (Vec::new(), 0.0)
            } else {
                ring_offsets(g.hosts.len())
            };
            let radius = if offsets.is_empty() {
                card_radius()
            } else {
                outer + DOT + 24.0
            };
            (offsets, radius)
        })
        .collect();
    let sizes: Vec<(f64, f64)> = rings.iter().map(|(_, r)| (2.0 * r, 2.0 * r)).collect();
    let corners = shelves(&sizes, GAP, true);

    let mut layout = Layout {
        links: true,
        ..Layout::empty()
    };
    for ((group, (offsets, radius)), (x, y)) in groups.into_iter().zip(rings).zip(corners) {
        let (cx, cy) = (x + radius, y + radius);
        layout.grow(cx - radius, cy - radius, cx + radius, cy + radius);
        let index = layout.groups.len();
        let (total, ok, bad) = group.counts();
        for host in &group.hosts[offsets.len()..] {
            layout.fold(index, host);
        }
        for (host, (dx, dy)) in group.hosts.iter().zip(offsets) {
            layout.place(index, host, cx + dx, cy + dy, cx, cy);
        }
        layout.groups.push(Group {
            name: group.name,
            target: group.target,
            x: cx,
            y: cy,
            radius,
            area: None,
            total,
            ok,
            bad,
            collapsed: group.collapsed,
        });
    }
    layout
}

/// Host offsets in an area, and the area's width and height.
type Cells = (Vec<(f64, f64)>, f64, f64);

/// Areas display: cells of the hosts of a group, row by row, around its card
/// (top right corner): offsets from the area's top left corner, and the
/// area's size.
fn area_cells(n: usize) -> Cells {
    // the card, and a margin around it, in the top right corner
    let card_cols = ((CARD_W + AREA_PAD) / CELL).ceil() as usize;
    let card_rows = ((CARD_H + AREA_PAD) / CELL).ceil() as usize;
    let cols = if n == 0 {
        card_cols
    } else {
        // about square, but at least a few columns left of the card
        ((n + card_cols * card_rows) as f64)
            .sqrt()
            .ceil()
            .max((card_cols + 4) as f64) as usize
    };
    let mut cells = Vec::with_capacity(n);
    let mut row = 0;
    while cells.len() < n {
        for col in 0..cols {
            if cells.len() == n {
                break;
            }
            if row < card_rows && col >= cols - card_cols {
                continue;
            }
            cells.push((
                AREA_PAD + (col as f64 + 0.5) * CELL,
                AREA_PAD + (row as f64 + 0.5) * CELL,
            ));
        }
        row += 1;
    }
    let rows = row.max(card_rows);
    let w = 2.0 * AREA_PAD + cols as f64 * CELL;
    let h = 2.0 * AREA_PAD + rows as f64 * CELL;
    (cells, w, h)
}

fn areas_layout(groups: Vec<Members>) -> Layout {
    let cells: Vec<Cells> = groups
        .iter()
        .map(|g| {
            if g.collapsed {
                // the card alone
                (Vec::new(), CARD_W + 2.0 * AREA_PAD, CARD_H + 2.0 * AREA_PAD)
            } else {
                area_cells(g.hosts.len())
            }
        })
        .collect();
    let sizes: Vec<(f64, f64)> = cells.iter().map(|(_, w, h)| (*w, *h)).collect();
    let corners = shelves(&sizes, AREA_GAP, false);

    let mut layout = Layout::empty();
    for ((group, (offsets, w, h)), (x, y)) in groups.into_iter().zip(cells).zip(corners) {
        layout.grow(x, y, x + w, y + h);
        let index = layout.groups.len();
        let (total, ok, bad) = group.counts();
        // card: top right corner, inside the margin
        let (cx, cy) = (x + w - AREA_PAD - CARD_W / 2.0, y + AREA_PAD + CARD_H / 2.0);
        for host in &group.hosts[offsets.len()..] {
            layout.fold(index, host);
        }
        for (host, (dx, dy)) in group.hosts.iter().zip(offsets) {
            layout.place(index, host, x + dx, y + dy, cx, cy);
        }
        let area = Area {
            x,
            y,
            w,
            h,
            folded: group.collapsed,
        };
        layout.areas.push(area);
        layout.groups.push(Group {
            name: group.name,
            target: group.target,
            x: cx,
            y: cy,
            radius: card_radius(),
            area: Some(area),
            total,
            ok,
            bad,
            collapsed: group.collapsed,
        });
    }
    layout
}

/// SVG donut arc: a circle stroke covering `[start, start + len)` of the
/// circumference, starting at 12 o'clock.
fn arc(cx: f64, cy: f64, r: f64, start: f64, len: f64, color: &'static str) -> impl IntoView {
    let c = std::f64::consts::TAU * r;
    view! {
        <circle class="donut" cx=cx cy=cy r=r stroke=color
            stroke-dasharray=format!("{} {}", len * c, c)
            stroke-dashoffset=-(start * c)
            transform=format!("rotate(-90 {cx} {cy})") />
    }
}

/// Key part for a position, so `<For>` re-renders moved cards.
fn position_key(x: f64, y: f64) -> (i64, i64) {
    ((x * 10.0) as i64, (y * 10.0) as i64)
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max - 1).collect::<String>())
    }
}

/// A run asked from a group card, waiting for confirmation.
#[derive(Clone)]
struct PendingRun {
    target: GroupTarget,
    job: api::JobKind,
    hosts: usize,
}

/// Hover tip (D24): delay before it opens, and its size (to keep it on screen).
const TIP_DELAY_MS: u64 = 150;
const TIP_WIDTH: f64 = 380.0;
const TIP_HEIGHT: f64 = 260.0;

#[component]
pub fn Map() -> impl IntoView {
    let session = expect_context::<Session>();
    // every host and the last hour's jobs: the app-wide feeds (crate::feeds)
    let Feeds {
        hosts, ready, jobs, ..
    } = expect_context::<Feeds>();
    let error = RwSignal::new(None::<String>);
    let assignments = RwSignal::new(Vec::<AssignmentEntry>::new());
    load_assignments(assignments, error);
    // reference -> host selector, for the assignment groups' Assess / Reach
    let selectors = Memo::new(move |_| {
        assignments.with(|list| {
            list.iter()
                .map(|a| (a.reference.clone(), a.selector.clone()))
                .collect::<BTreeMap<_, _>>()
        })
    });

    let display = RwSignal::new(Display::saved());
    // D23: the colours mean "against the assignments", so group by them first
    let group_by = RwSignal::new(GroupBy::Assignment);
    let label_key = RwSignal::new(String::new());
    let collapsed = RwSignal::new(BTreeSet::<String>::new());
    let label_keys = Memo::new(move |_| {
        hosts.with(|hosts| {
            let keys: BTreeSet<&str> = hosts
                .values()
                .flat_map(|host| host.labels.keys().map(String::as_str))
                .collect();
            keys.into_iter().map(str::to_string).collect::<Vec<_>>()
        })
    });
    let rings = Memo::new(move |_| jobs.with(rings));
    let map = Memo::new(move |_| {
        hosts.with(|hosts| {
            label_key.with(|label_key| {
                collapsed.with(|collapsed| {
                    selectors.with(|selectors| {
                        layout(
                            hosts,
                            display.get(),
                            group_by.get(),
                            label_key,
                            collapsed,
                            selectors,
                        )
                    })
                })
            })
        })
    });

    // view box: zoom (wheel, around the pointer) and pan (drag)
    let scale = RwSignal::new(1.0f64);
    let center = RwSignal::new(None::<(f64, f64)>); // None = fit the whole map
    let drag = RwSignal::new(None::<(f64, f64)>);
    let moved = RwSignal::new(false);
    let half = move || {
        let (x0, y0, x1, y1) = map.with(|m| m.bounds);
        ((x1 - x0) / 2.0 + 40.0, (y1 - y0) / 2.0 + 40.0)
    };
    let view_center = move || {
        center.get().unwrap_or_else(|| {
            let (x0, y0, x1, y1) = map.with(|m| m.bounds);
            ((x0 + x1) / 2.0, (y0 + y1) / 2.0)
        })
    };
    let view = move || {
        let (hw, hh) = half();
        let s = scale.get();
        let (cx, cy) = view_center();
        View {
            cx,
            cy,
            w: 2.0 * hw / s,
            h: 2.0 * hh / s,
        }
    };
    let view_box = move || {
        let v = view();
        format!("{} {} {} {}", v.cx - v.w / 2.0, v.cy - v.h / 2.0, v.w, v.h)
    };
    // map units per screen pixel, and the element's screen center
    let metrics = |target: Option<web_sys::EventTarget>| {
        let rect = target?
            .dyn_into::<web_sys::Element>()
            .ok()?
            .get_bounding_client_rect();
        Some((
            rect.width().max(1.0),
            rect.height().max(1.0),
            rect.left(),
            rect.top(),
        ))
    };
    let units_per_pixel = move |w: f64, h: f64| untrack(view).units_per_pixel(w, h);
    // the host under the pointer of a mouse event, if any
    let host_under = move |ev: &web_sys::MouseEvent| {
        let (w, h, left, top) = metrics(ev.current_target())?;
        let view = untrack(view);
        let (x, y) = view.map_point(
            ev.client_x() as f64 - left,
            ev.client_y() as f64 - top,
            w,
            h,
        );
        // the dot and a little around it, but at least 5 pixels when zoomed out
        let radius = (DOT + 3.0).max(5.0 * view.units_per_pixel(w, h));
        map.with_untracked(|m| m.host_at(x, y, radius).map(|p| p.host_id.clone()))
    };

    // hovered host (its id: the side panel follows its live summary)
    let hovered = RwSignal::new(None::<String>);
    let host_with_job = move |id: &str| {
        let job = rings.with(|rings| rings.get(id).map(|r| r.state().to_string()));
        hosts
            .with(|hosts| hosts.get(id).cloned())
            .map(|host| (host, job))
    };

    // D24: the hover tip follows the cursor; it opens (and starts its live
    // stream) only once the pointer rests on a host, so sweeping across
    // thousands of hosts opens nothing
    let pointer = RwSignal::new((0.0f64, 0.0f64));
    let tip = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        let id = hovered.get();
        if tip.get_untracked() == id {
            return;
        }
        tip.set(None);
        if let Some(id) = id {
            set_timeout(
                move || {
                    if hovered.try_get_untracked().flatten().as_deref() == Some(id.as_str()) {
                        let _ = tip.try_set(Some(id));
                    }
                },
                std::time::Duration::from_millis(TIP_DELAY_MS),
            );
        }
    });
    // the tip's running job (its id and state): a change re-renders the tip
    let tip_job = Memo::new(move |_| {
        tip.with(|id| {
            id.as_deref().and_then(|id| {
                jobs.with(|jobs| {
                    running_jobs(jobs, id)
                        .first()
                        .map(|job| (job.job_id.clone(), job.state.clone()))
                })
            })
        })
    });
    let tip_style = move || {
        let (x, y) = pointer.get();
        let window = web_sys::window();
        let size =
            |v: Option<js_sys::wasm_bindgen::JsValue>| v.and_then(|v| v.as_f64()).unwrap_or(1e9);
        let width = size(window.as_ref().and_then(|w| w.inner_width().ok()));
        let height = size(window.as_ref().and_then(|w| w.inner_height().ok()));
        let left = if x + TIP_WIDTH + 24.0 > width {
            x - TIP_WIDTH - 16.0
        } else {
            x + 16.0
        };
        let top = if y + TIP_HEIGHT + 24.0 > height {
            (y - TIP_HEIGHT - 16.0).max(0.0)
        } else {
            y + 16.0
        };
        format!("left:{left}px;top:{top}px;width:{TIP_WIDTH}px")
    };

    // selected host (clicked): shown in the right panel, kept in the URL
    // (`/?host=<id>`) so it can be shared and survives a reload
    let selected = RwSignal::new(use_query_map().read_untracked().get("host"));
    let selected_host =
        Memo::new(move |_| selected.with(|id| id.as_deref().and_then(host_with_job)));
    // D24: the selected host's running jobs (live) and its last job's log
    let selected_running = Signal::derive(move || {
        selected.with(|id| {
            id.as_deref()
                .map(|id| jobs.with(|jobs| running_jobs(jobs, id)))
                .unwrap_or_default()
        })
    });
    let selected_last_log = Memo::new(move |_| {
        selected_host.with(|host| {
            host.as_ref()
                .map(|(host, _)| host)
                .filter(|host| !host.last_job.is_empty())
                .map(|host| {
                    (
                        host.last_job.clone(),
                        host.last_job_month.clone(),
                        host.state_at,
                    )
                })
        })
    });
    let navigate = use_navigate();
    Effect::new({
        let navigate = navigate.clone();
        move |previous: Option<Option<String>>| {
            let id = selected.get();
            if previous.is_some_and(|p| p != id) {
                let url = match &id {
                    Some(id) => format!("/?host={}", js_sys::encode_uri_component(id)),
                    None => "/".to_string(),
                };
                navigate(
                    &url,
                    NavigateOptions {
                        replace: true,
                        scroll: false,
                        ..Default::default()
                    },
                );
            }
            id
        }
    });
    // canvas: redrawn on the next animation frame after a change only (job
    // events included: a host with a job only changes colour)
    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();
    let Theme(theme) = expect_context::<Theme>();
    let alive = Arc::new(AtomicBool::new(true));
    let dirty = Arc::new(AtomicBool::new(true));
    on_cleanup({
        let alive = alive.clone();
        move || alive.store(false, Ordering::Relaxed)
    });
    Effect::new({
        let dirty = dirty.clone();
        move |_| {
            map.track();
            scale.track();
            center.track();
            hovered.track();
            selected.track();
            rings.track();
            theme.track();
            dirty.store(true, Ordering::Relaxed);
        }
    });
    {
        // colours of the current theme, read again when it changes
        let mut palette = Palette::read();
        let mut palette_theme = theme.get_untracked();
        let context = |canvas: &web_sys::HtmlCanvasElement| {
            canvas
                .get_context("2d")
                .ok()
                .flatten()
                .and_then(|c| c.dyn_into::<web_sys::CanvasRenderingContext2d>().ok())
        };
        canvas::animate(alive, move |_| {
            let Some(hosts_canvas) = canvas_ref.get_untracked() else {
                return;
            };
            let (w, h) = (
                hosts_canvas.client_width() as f64,
                hosts_canvas.client_height() as f64,
            );
            if w < 1.0 || h < 1.0 {
                return;
            }
            let dpr = web_sys::window().map_or(1.0, |w| w.device_pixel_ratio());
            let (bw, bh) = ((w * dpr).round() as u32, (h * dpr).round() as u32);
            let resized = hosts_canvas.width() != bw || hosts_canvas.height() != bh;
            if resized {
                hosts_canvas.set_width(bw);
                hosts_canvas.set_height(bh);
            }
            if !(dirty.swap(false, Ordering::Relaxed) || resized) {
                return;
            }
            if theme.get_untracked() != palette_theme {
                palette_theme = theme.get_untracked();
                palette = Palette::read();
            }
            let hovered = hovered.get_untracked();
            let selected = selected.get_untracked();
            let names = scale.get_untracked() >= 2.5;
            let view = untrack(view);
            map.with_untracked(|layout| {
                rings.with_untracked(|rings| {
                    let frame = Frame {
                        layout,
                        rings,
                        view,
                        hovered: hovered.as_deref(),
                        selected: selected.as_deref(),
                        names,
                    };
                    if let Some(ctx) = context(&hosts_canvas) {
                        canvas::draw_hosts(&ctx, w, h, dpr, &palette, &frame);
                    }
                })
            });
        });
    }

    // Assess / Reach from a group card, after confirmation: every host
    // runs its own effective expected state (D23), nothing to choose
    let pending = RwSignal::new(None::<PendingRun>);
    let message = RwSignal::new(None::<String>);
    // one-shot attribute dialog (D23), opened from an assignment's dialog
    let one_shot = RwSignal::new(None::<(GroupTarget, usize)>);
    let ask = move |target: GroupTarget, job: api::JobKind, count: usize| {
        if moved.get_untracked() {
            return;
        }
        pending.set(Some(PendingRun {
            target,
            job,
            hosts: count,
        }));
    };
    let confirm = move || {
        let Some(run) = pending.get_untracked() else {
            return;
        };
        pending.set(None);
        spawn_local(async move {
            let request = api::RunNowRequest {
                command_id: command_id("run"),
                selector: Some(run.target.selector()),
                job: run.job as i32,
                pool: String::new(),
                one_shot_yaml: String::new(),
            };
            let text = match client().run_now(authed(request, &session.token())).await {
                Ok(_) => format!(
                    "{} of {} accepted",
                    job_label(run.job as i32),
                    run.target.describe()
                ),
                Err(status) => {
                    session.check(&status);
                    format!("refused: {}", status.message())
                }
            };
            let _ = message.try_set(Some(text));
        });
    };

    // the host panel is open whenever a host is selected
    let right_open = RwSignal::new(true);
    // panel widths: set by dragging the splitters between the panels and the
    // map (double click: default width), remembered by the browser
    let width = |key: &'static str, default: f64| {
        let stored = crate::client::storage()
            .and_then(|s| s.get_item(key).ok().flatten())
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|w| w.is_finite());
        (
            RwSignal::new(clamp_panel(stored.unwrap_or(default))),
            key,
            default,
        )
    };
    let right_width = width("regency.map.right.width", 400.0);
    let page_style = move || {
        let mut style = String::new();
        if right_open.get() {
            style.push_str(&format!("--right:{}px;", right_width.0.get()));
        }
        style
    };
    // `sign`: +1 when dragging right widens the panel, -1 otherwise (the right one)
    let splitter = move |(width, key, default): (RwSignal<f64>, &'static str, f64),
                         open: RwSignal<bool>,
                         sign: f64| {
        let start = StoredValue::new(None::<(f64, f64)>); // pointer x, width
        let save = move || {
            if let Some(storage) = crate::client::storage() {
                let _ = storage.set_item(key, &width.get_untracked().round().to_string());
            }
        };
        view! {
            <div class=move || if open.get() { "splitter" } else { "splitter off" }
                title="Drag to resize the panel (double click: default width)"
                on:pointerdown=move |ev: web_sys::PointerEvent| {
                    if !open.get_untracked() || ev.button() != 0 {
                        return;
                    }
                    ev.prevent_default();
                    if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                        let _ = el.set_pointer_capture(ev.pointer_id());
                    }
                    start.set_value(Some((ev.client_x() as f64, width.get_untracked())));
                }
                on:pointermove=move |ev: web_sys::PointerEvent| {
                    if let Some((x0, w0)) = start.get_value() {
                        width.set(clamp_panel(w0 + sign * (ev.client_x() as f64 - x0)));
                    }
                }
                on:pointerup=move |_| {
                    if start.get_value().is_some() {
                        start.set_value(None);
                        save();
                    }
                }
                on:pointercancel=move |_| {
                    start.set_value(None);
                    save();
                }
                on:dblclick=move |_| {
                    width.set(clamp_panel(default));
                    save();
                }>
            </div>
        }
    };
    let page_class = move || {
        if selected.with(Option::is_some) {
            "map-page"
        } else {
            "map-page no-panel"
        }
    };
    let all_folded = move || {
        map.with(|m| {
            collapsed.with(|c| !m.groups.is_empty() && m.groups.iter().all(|g| c.contains(&g.name)))
        })
    };
    let svg_class = move || {
        if hovered.get().is_some() {
            "map over-host"
        } else {
            "map"
        }
    };

    view! {
        <div class=page_class style=page_style>
            <section class="map-canvas">
                // D24: hover tip, following the cursor
                {move || {
                    let job = tip_job.get();
                    let id = tip.get()?;
                    let host = hosts.with_untracked(|hosts| hosts.get(&id).cloned())?;
                    let running = job.and_then(|(job_id, _)| {
                        jobs.with_untracked(|jobs| jobs.get(&job_id).cloned())
                    });
                    Some(view! {
                        <div class="live-tip" style=tip_style>
                            <HostTip host=host running=running />
                        </div>
                    })
                }}
                <div class="toolbar">
                    <span class="muted">"Group by"</span>
                    <select on:change=move |ev| {
                        let value = event_target_value(&ev);
                        collapsed.set(BTreeSet::new());
                        center.set(None);
                        group_by.set(match value.as_str() {
                            "state" => GroupBy::State,
                            "assignment" => GroupBy::Assignment,
                            key => {
                                label_key.set(key.trim_start_matches("label:").to_string());
                                GroupBy::Label
                            }
                        });
                    }>
                        <option value="assignment">"assignment"</option>
                        <For each=move || label_keys.get() key=|k| k.clone() let:key>
                            <option value=format!("label:{key}")>{format!("label {key}")}</option>
                        </For>
                        <option value="state">"state"</option>
                    </select>
                    <span class="muted">"Display"</span>
                    <select title="Graph: hosts around their group; areas: hosts inside their group"
                        on:change=move |ev| {
                            let value = if event_target_value(&ev) == "areas" { Display::Areas } else { Display::Graph };
                            value.save();
                            scale.set(1.0);
                            center.set(None);
                            display.set(value);
                        }>
                        <option value="graph" selected=move || display.get() == Display::Graph>"graph"</option>
                        <option value="areas" selected=move || display.get() == Display::Areas>"areas"</option>
                    </select>
                    // one button: fold every group, or unfold them all once all are folded
                    <button title="Fold or unfold every group"
                        on:click=move |_| if all_folded() {
                            collapsed.set(BTreeSet::new());
                        } else {
                            collapsed.set(map.with_untracked(|m| m.groups.iter().map(|g| g.name.clone()).collect()));
                        }>
                        {move || if all_folded() { "Unfold" } else { "Fold" }}
                    </button>
                    <span class="muted">{move || if ready.get() { format!("{} hosts, live", hosts.with(|h| h.len())) } else { "loading…".into() }}</span>
                    <span class="muted">{move || message.get()}</span>
                </div>
                <Show when=move || error.get().is_some()>
                    <p class="error">{move || error.get()}</p>
                </Show>
                <div class="map-view">
                <canvas node_ref=canvas_ref />
                <svg class=svg_class viewBox=view_box
                    on:wheel=move |ev| {
                        ev.prevent_default();
                        let Some((w, h, left, top)) = metrics(ev.current_target()) else { return };
                        let factor = if ev.delta_y() < 0.0 { 1.15 } else { 1.0 / 1.15 };
                        let old = scale.get_untracked();
                        let new = (old * factor).clamp(0.1, 20.0);
                        // keep the point under the pointer in place
                        let k_old = units_per_pixel(w, h);
                        let k_new = k_old * old / new;
                        let (px, py) = (ev.client_x() as f64 - left - w / 2.0, ev.client_y() as f64 - top - h / 2.0);
                        let (cx, cy) = view_center();
                        let (wx, wy) = (cx + px * k_old, cy + py * k_old);
                        scale.set(new);
                        center.set(Some((wx - px * k_new, wy - py * k_new)));
                    }
                    on:pointerdown=move |ev| {
                        hovered.set(None);
                        moved.set(false);
                        drag.set(Some((ev.client_x() as f64, ev.client_y() as f64)));
                    }
                    on:pointerup=move |_| drag.set(None)
                    on:pointerleave=move |_| { drag.set(None); hovered.set(None); }
                    on:click=move |ev| {
                        if moved.get_untracked() {
                            return;
                        }
                        // a host: show it in the right panel; empty space: deselect
                        selected.set(host_under(&ev));
                    }
                    on:pointermove=move |ev| {
                        let Some((x, y)) = drag.get_untracked() else {
                            pointer.set((ev.client_x() as f64, ev.client_y() as f64));
                            let id = host_under(&ev);
                            if hovered.with_untracked(|h| *h != id) {
                                hovered.set(id);
                            }
                            return;
                        };
                        let (nx, ny) = (ev.client_x() as f64, ev.client_y() as f64);
                        if !moved.get_untracked() && (nx - x).abs() + (ny - y).abs() < 4.0 {
                            return;
                        }
                        moved.set(true);
                        let Some((w, h, _, _)) = metrics(ev.current_target()) else { return };
                        let k = units_per_pixel(w, h);
                        let (cx, cy) = view_center();
                        center.set(Some((cx - (nx - x) * k, cy - (ny - y) * k)));
                        drag.set(Some((nx, ny)));
                    }>
                    // hosts and links: on the canvas, under the cards
                    // group cards, on top
                    <For each=move || map.with(|m| {
                            let running = rings.with(|rings| m.running(rings));
                            m.groups.iter().cloned().zip(running).collect::<Vec<_>>()
                        })
                        key=|(g, running)| (g.name.clone(), position_key(g.x, g.y), g.total, g.ok, g.bad, *running, g.collapsed)
                        let:card>
                        {group_card(card.0, card.1, collapsed, moved, ask)}
                    </For>
                </svg>
                // legend: a layer on top of the hosts, bottom left
                        <details class="map-legend">
                            <summary>"Legend"</summary>
                            <ul class="legend">
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="var(--ok)" /></svg> " compliant / reach success"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="var(--bad)" /></svg> " not compliant / failed / error"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="var(--warn)" /></svg> " aborted / assignments in conflict"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="var(--unknown)" /></svg> " not checked yet, or against an older expected state"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="var(--unassigned)" /></svg> " unassigned: no expected state"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="var(--job)" /></svg> " job in progress (pending, running or abort requested)"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="none" stroke="var(--warn)" stroke-width="2" stroke-dasharray="2 2" /></svg> " conflicting definitions or certificate pending"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="7" fill="none" stroke="var(--accent)" stroke-width="2" /></svg> " selected host"</li>
                                <li><svg width="16" height="16"><path d="M8 1 L15 8 L8 15 L1 8 Z" fill="var(--unknown)" /></svg> " proactive host (regops): its own Git state, coloured by its last pass"</li>
                                <li><svg width="16" height="16"><path d="M8 1 L15 8 L8 15 L1 8 Z" fill="none" stroke="var(--warn)" stroke-width="2" /></svg> " proactive host behind its repository's latest commit"</li>
                            </ul>
                        </details>
                </div>
            </section>
            // the host panel: only for a selected host (a click on it)
            <Show when=move || selected.with(Option::is_some)>
            {splitter(right_width, right_open, -1.0)}
            <aside class="map-side map-right">
                    <div class="host-head">
                        <h2>{move || selected.get().unwrap_or_default()}</h2>
                        <button class="primary" on:click={
                            let navigate = navigate.clone();
                            move |_| if let Some(id) = selected.get_untracked() {
                                navigate(&format!("/hosts/{}", js_sys::encode_uri_component(&id)), Default::default());
                            }
                        }>"Details"</button>
                        <button title="Close" on:click=move |_| selected.set(None)>"×"</button>
                    </div>
                    {move || match selected_host.get() {
                        None => view! { <p class="muted">"This host is not on the map (deleted, or not visible to you)."</p> }.into_any(),
                        Some((host, job)) => view! {
                            {host_summary(host, job, false)}
                        }.into_any(),
                    }}
                    <h2>"Live"</h2>
                    <HostLive running=selected_running />
                    <h2>"Last job log"</h2>
                    {move || match selected_last_log.get() {
                        Some((job_id, month, at)) => view! { <RunLogView job_id=job_id month=month finished_at=at /> }.into_any(),
                        None => view! { <p class="muted">"No finished job within retention."</p> }.into_any(),
                    }}
                    <h2>"Expected state"</h2>
                    <HostStateExplorer host=Signal::derive(move || selected_host.with(|h| h.as_ref().map(|(host, _)| host.clone()))) />
            </aside>
            </Show>
        </div>
        <Show when=move || pending.get().is_some()>
            {move || pending.get().map(|run| {
                let reach = run.job == api::JobKind::Reach;
                view! {
                    <div class="overlay" on:click=move |_| pending.set(None)>
                        <form class="dialog" on:click=|ev| { ev.stop_propagation(); }
                            on:submit=move |ev| { ev.prevent_default(); confirm(); }>
                            <h2>{format!("{} {}", job_label(run.job as i32), run.target.describe())}</h2>
                            <p class="muted">{format!("{} hosts on the map; the selection is resolved again by the scheduler.", run.hosts)}</p>
                            {match &run.target {
                                GroupTarget::Assignment(_, selector) => Some(view! { <p class="muted">{format!("Selects {}.", describe_selector(selector))}</p> }),
                                _ => None,
                            }}
                            {reach.then(|| view! { <p class="reach-warning">"Reach changes the hosts to match their expected state."</p> })}
                            <div class="actions">
                                {matches!(run.target, GroupTarget::Assignment(..)).then(|| {
                                    let (target, hosts) = (run.target.clone(), run.hosts);
                                    view! {
                                        <button type="button" class="left" title="Write one attribute by hand and run it once on these hosts"
                                            on:click=move |_| { pending.set(None); one_shot.set(Some((target.clone(), hosts))); }>
                                            "Run an attribute…"
                                        </button>
                                    }
                                })}
                                <button type="button" on:click=move |_| pending.set(None)>"Cancel"</button>
                                <button class="primary" type="submit">{job_label(run.job as i32)}</button>
                            </div>
                        </form>
                    </div>
                }
            })}
        </Show>
        {move || one_shot.get().map(|(target, hosts)| view! {
            <OneShotDialog target hosts on_close=Callback::new(move |_| one_shot.set(None)) />
        })}
    }
}

/// A host's summary in the right panel (live: rebuilt on every update).
/// A side panel's width in pixels: at least 180, at most 45 % of the window
/// (the map keeps room).
fn clamp_panel(width: f64) -> f64 {
    let window = web_sys::window()
        .and_then(|w| w.inner_width().ok())
        .and_then(|w| w.as_f64())
        .unwrap_or(1920.0);
    width.clamp(180.0, (window * 0.45).max(180.0))
}

fn host_summary(host: api::HostSummary, job: Option<String>, with_name: bool) -> impl IntoView {
    view! {
        <dl>
            {with_name.then(|| view! { <dt>"Host"</dt><dd><b>{host.host_id.clone()}</b></dd> })}
            <dt>"State"</dt><dd>{badge(host_state(&host).to_string())}</dd>
            <dt>"Assignments"</dt><dd>{assignments_line(&host)}</dd>
            {crate::pages::agent_lines(&host)}
            {job.map(|state| view! { <dt>"Job"</dt><dd>{badge(state)}</dd> })}
            {host.conflicting.then(|| view! { <dt>"Flag"</dt><dd>"conflicting definitions"</dd> })}
            {(!host.pending_fingerprints.is_empty()).then(|| view! { <dt>"Flag"</dt><dd>"certificate pending"</dd> })}
            <dt>"Last outcome"</dt><dd>{format_ms(host.state_at)}</dd>
            <dt>"Last seen"</dt><dd>{format_ms(host.last_seen)}</dd>
            <dt>"Labels"</dt><dd>{host.labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")}</dd>
        </dl>
        {(!host.error.is_empty()).then(|| view! { <p class="error">{host.error.clone()}</p> })}
    }
}

/// A group (assignment, label or state) node: name, counts, compliance donut,
/// Assess / Reach buttons.
fn group_card(
    group: Group,
    running: usize,
    collapsed: RwSignal<BTreeSet<String>>,
    moved: RwSignal<bool>,
    ask: impl Fn(GroupTarget, api::JobKind, usize) + Copy + 'static,
) -> impl IntoView {
    let (w, h) = (CARD_W, CARD_H);
    let (left, top) = (-w / 2.0, -h / 2.0);
    let name = group.name.clone();
    let toggle = move |_| {
        if moved.get_untracked() {
            return;
        }
        let name = name.clone();
        collapsed.update(|set| {
            if !set.remove(&name) {
                set.insert(name);
            }
        });
    };
    let total = group.total.max(1) as f64;
    let (ok, bad) = (group.ok as f64 / total, group.bad as f64 / total);
    let other = 1.0 - ok - bad;
    let (dx, dy, dr) = (left + 34.0, 6.0, 17.0);
    let pct = if group.total == 0 {
        "—".to_string()
    } else {
        format!("{}%", (ok * 100.0).round())
    };
    let sub = format!(
        "{} host{}{}",
        group.total,
        if group.total == 1 { "" } else { "s" },
        if group.collapsed { " · folded" } else { "" }
    );
    let running_text = (running > 0).then(|| {
        view! {
            <text class="running" x=-left - 12.0 y=top + 24.0 text-anchor="end">{format!("{} running", running)}</text>
        }
    });
    let buttons = group.target.clone().map(|target| {
        let bw = (w - 36.0) / 2.0;
        let by = h / 2.0 - 30.0;
        let count = group.total;
        let (t1, t2) = (target.clone(), target);
        view! {
            <g class="btn assess" on:click=move |ev| { ev.stop_propagation(); ask(t1.clone(), api::JobKind::Assess, count); }>
                <rect x=left + 12.0 y=by width=bw height="22" rx="4" />
                <text x=left + 12.0 + bw / 2.0 y=by + 15.0 text-anchor="middle">"Assess"</text>
                <title>"Assess every host of this group"</title>
            </g>
            <g class="btn reach" on:click=move |ev| { ev.stop_propagation(); ask(t2.clone(), api::JobKind::Reach, count); }>
                <rect x=left + 24.0 + bw y=by width=bw height="22" rx="4" />
                <text x=left + 24.0 + bw * 1.5 y=by + 15.0 text-anchor="middle">"Reach"</text>
                <title>"Reach the expected state on every host of this group"</title>
            </g>
        }
    });
    let bad_class = if group.bad > 0 {
        "count-bad"
    } else {
        "count-none"
    };
    view! {
        <g class="group" transform=format!("translate({} {})", group.x, group.y) on:click=toggle>
            <rect class="card" x=left y=top width=w height=h rx="10" />
            <rect class="band" x=left + 8.0 y=top width=w - 16.0 height="4" />
            <text class="title" x=left + 12.0 y=top + 24.0>{truncate(&group.name, if running > 0 { 18 } else { 28 })}</text>
            <text class="sub" x=left + 12.0 y=top + 40.0>{sub}</text>
            <circle class="donut-track" cx=dx cy=dy r=dr />
            {(group.ok > 0).then(|| arc(dx, dy, dr, 0.0, ok, "var(--ok)"))}
            {(group.bad > 0).then(|| arc(dx, dy, dr, ok, bad, "var(--bad)"))}
            {(other > 0.001 && group.total > 0).then(|| arc(dx, dy, dr, ok + bad, other, "var(--unknown)"))}
            <text class="pct" x=dx y=dy + 3.5 text-anchor="middle">{pct}</text>
            <text class="count-ok" x=left + 64.0 y=dy - 3.0>{format!("{} compliant", group.ok)}</text>
            <text class=bad_class x=left + 64.0 y=dy + 12.0>{format!("{} not compliant", group.bad)}</text>
            {running_text}
            {buttons}
            <title>{format!("{} — click to {}", group.name, if group.collapsed { "unfold" } else { "fold" })}</title>
        </g>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rings_never_overlap_and_clear_the_card() {
        let (points, outer) = ring_offsets(500);
        assert_eq!(points.len(), 500);
        for (i, a) in points.iter().enumerate() {
            let r = (a.0 * a.0 + a.1 * a.1).sqrt();
            assert!(r >= card_radius() - 10.0 && r <= outer + 0.01, "{r}");
            for b in &points[i + 1..] {
                let d = ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
                assert!(d > DOT * 2.5, "{d}");
            }
        }
    }

    /// A host checked against its current effective state: label `kind`
    /// = its first kind; assignments `git/base`, then `git/<kind>` for each
    /// kind (D29: they add up); none for a host without kind.
    fn host(id: &str, kinds: &[&str], compliance: &str) -> api::HostSummary {
        let mut assignments = Vec::new();
        if !kinds.is_empty() {
            assignments.push("git/base".to_string());
        }
        assignments.extend(kinds.iter().map(|k| format!("git/{k}")));
        api::HostSummary {
            host_id: id.into(),
            compliance: compliance.into(),
            labels: kinds
                .first()
                .map(|k| ("kind".to_string(), k.to_string()))
                .into_iter()
                .collect(),
            assignments,
            effective_revision: "r1".into(),
            state_revision: "r1".into(),
            ..Default::default()
        }
    }

    fn selectors() -> BTreeMap<String, api::HostSelector> {
        ["git/base", "git/web", "git/db", "git/cache"]
            .into_iter()
            .map(|r| {
                let selector = api::HostSelector {
                    assignments: vec![r.to_string()],
                    ..Default::default()
                };
                (r.to_string(), selector)
            })
            .collect()
    }

    #[test]
    fn groups_by_assignment() {
        let hosts = fleet();
        let map = layout(
            &hosts,
            Display::Areas,
            GroupBy::Assignment,
            "",
            &BTreeSet::new(),
            &selectors(),
        );
        let names: Vec<&str> = map.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names.len(), 5, "{names:?}");
        let base = map.groups.iter().find(|g| g.name == "git/base").unwrap();
        assert_eq!(base.total, 121);
        assert!(matches!(&base.target, Some(GroupTarget::Assignment(r, s))
            if r == "git/base" && s.assignments == ["git/base"]));
        // hosts are in every assignment selecting them (D29)
        let web = map.groups.iter().find(|g| g.name == "git/web").unwrap();
        assert_eq!(web.total, 41);
        assert_eq!(map.placements("shared").count(), 3);
        // the host without assignment: its own group, nothing to run, grey
        let none = map
            .groups
            .iter()
            .find(|g| g.name == "(unassigned)")
            .unwrap();
        assert!(none.target.is_none() && none.total == 1);
        let lonely = map.placements("lonely").next().unwrap();
        assert_eq!(lonely.tone, Tone::Unassigned);
    }

    #[test]
    fn graph_groups_by_most_specific_assignment() {
        let hosts = fleet();
        let map = layout(
            &hosts,
            Display::Graph,
            GroupBy::Assignment,
            "",
            &BTreeSet::new(),
            &selectors(),
        );
        let total = |name: &str| map.groups.iter().find(|g| g.name == name).unwrap().total;
        // each host under its last (most specific) assignment only: the
        // shared web+db host under git/db, nothing left under git/base
        assert_eq!(total("git/web"), 40);
        assert_eq!(total("git/db"), 41);
        assert!(map.groups.iter().all(|g| g.name != "git/base"));
    }

    #[test]
    fn host_states_follow_assignments() {
        let mut h = host("h", &["web"], "compliant");
        assert_eq!(host_state(&h), "compliant");
        h.state_revision = "r0".into();
        assert_eq!(host_state(&h), "outdated");
        h.assignment_error = "same Name at equal priority".into();
        assert_eq!(host_state(&h), "invalid");
        h.assignments.clear();
        assert_eq!(host_state(&h), "unassigned");
        // D28: a proactive host shows its own pass's compliance
        h.agent = "regops".into();
        assert_eq!(host_state(&h), "compliant");
        h.compliance = "not_compliant".into();
        assert_eq!(host_state(&h), "not_compliant");
    }

    /// 120 hosts of three kinds, one of two kinds, one of none.
    fn fleet() -> BTreeMap<String, api::HostSummary> {
        let mut hosts = BTreeMap::new();
        for i in 0..120 {
            let kind = ["web", "db", "cache"][i % 3];
            let h = host(
                &format!("h{i}"),
                &[kind],
                if i % 7 == 0 {
                    "not_compliant"
                } else {
                    "compliant"
                },
            );
            hosts.insert(h.host_id.clone(), h);
        }
        let multi = host("shared", &["web", "db"], "compliant");
        hosts.insert(multi.host_id.clone(), multi);
        hosts.insert("lonely".into(), host("lonely", &[], "unknown"));
        hosts
    }

    #[test]
    fn areas_keep_hosts_in_name_order() {
        // states vary across the fleet: the cells must not follow them,
        // or every host would move when one changes state
        let hosts = fleet();
        let map = layout(
            &hosts,
            Display::Areas,
            GroupBy::Label,
            "kind",
            &BTreeSet::new(),
            &selectors(),
        );
        for group in &map.groups {
            let mut cells: Vec<(f64, f64, &str)> = map
                .of_host
                .keys()
                .flat_map(|id| {
                    map.placements(id)
                        .filter(|p| (p.gx, p.gy) == (group.x, group.y))
                        .map(move |p| (p.y, p.x, id.as_str()))
                })
                .collect();
            cells.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
            let ids: Vec<&str> = cells.iter().map(|c| c.2).collect();
            let mut sorted = ids.clone();
            sorted.sort();
            assert_eq!(ids, sorted, "{}", group.name);
        }
    }

    #[test]
    fn graph_keeps_hosts_in_name_order() {
        // like areas: ring places follow names, not states
        let hosts = fleet();
        let map = layout(
            &hosts,
            Display::Graph,
            GroupBy::Label,
            "kind",
            &BTreeSet::new(),
            &selectors(),
        );
        for group in &map.groups {
            let ids: Vec<&str> = map
                .hosts
                .iter()
                .filter(|p| (p.gx, p.gy) == (group.x, group.y))
                .map(|p| p.host_id.as_str())
                .collect();
            let mut sorted = ids.clone();
            sorted.sort();
            assert_eq!(ids, sorted, "{}", group.name);
        }
    }

    #[test]
    fn areas_hold_their_hosts() {
        let hosts = fleet();
        let map = layout(
            &hosts,
            Display::Areas,
            GroupBy::Assignment,
            "",
            &BTreeSet::new(),
            &selectors(),
        );
        assert!(!map.links);
        assert_eq!(map.areas.len(), 5);
        // the shared host is drawn in its three assignments, and counted in each
        assert_eq!(map.placements("shared").count(), 3);
        assert_eq!(map.hosts.len(), 244);
        let web = map.groups.iter().find(|g| g.name == "git/web").unwrap();
        assert_eq!(web.total, 41);
        for (i, a) in map.areas.iter().enumerate() {
            for b in &map.areas[i + 1..] {
                let apart =
                    a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                assert!(apart, "{a:?} / {b:?}");
            }
        }
        for (index, group) in map.groups.iter().enumerate() {
            let a = group.area.unwrap();
            // card inside its area, top right
            assert!(group.x + CARD_W / 2.0 <= a.x + a.w && group.y - CARD_H / 2.0 >= a.y);
            let ids: Vec<&str> = map
                .of_host
                .iter()
                .filter(|(_, places)| places.iter().any(|(g, _)| *g == index))
                .map(|(id, _)| id.as_str())
                .collect();
            for id in ids {
                for p in map
                    .placements(id)
                    .filter(|p| (p.gx, p.gy) == (group.x, group.y))
                {
                    // inside the area, off the card
                    assert!(p.x - DOT >= a.x && p.x + DOT <= a.x + a.w, "{id}");
                    assert!(p.y - DOT >= a.y && p.y + DOT <= a.y + a.h, "{id}");
                    let on_card = (p.x - group.x).abs() < CARD_W / 2.0 + DOT
                        && (p.y - group.y).abs() < CARD_H / 2.0 + DOT;
                    assert!(!on_card, "{id}");
                }
            }
        }
        // folded: the card alone, its hosts not drawn there
        let folded: BTreeSet<String> = ["git/web".to_string()].into();
        let map = layout(
            &hosts,
            Display::Areas,
            GroupBy::Assignment,
            "",
            &folded,
            &selectors(),
        );
        let web = map.groups.iter().find(|g| g.name == "git/web").unwrap();
        let area = web.area.unwrap();
        assert!(area.folded && area.w < CARD_W + 40.0 && web.total == 41);
        assert_eq!(map.placements("shared").count(), 2);
    }

    #[test]
    fn groups_by_label_without_overlap() {
        let hosts = fleet();
        let job = |id: &str, host: &str, state: &str, created_at| {
            let job = api::JobSummary {
                job_id: id.into(),
                host_id: host.into(),
                state: state.into(),
                created_at,
                ..Default::default()
            };
            (job.job_id.clone(), job)
        };
        // h1: a job started; h2: an old pending job, then a finished one
        let jobs = [
            job("a", "h1", "STARTED", 10),
            job("b", "h2", "PENDING", 10),
            job("c", "h2", "FINISHED", 20),
        ]
        .into_iter()
        .collect();
        let rings = rings(&jobs);
        assert_eq!(rings, [("h1".to_string(), Ring::Started)].into());
        let map = layout(
            &hosts,
            Display::Graph,
            GroupBy::Label,
            "kind",
            &BTreeSet::new(),
            &selectors(),
        );
        assert_eq!(map.groups.len(), 4);
        assert_eq!(map.hosts.len(), 122);
        let web = map.groups.iter().find(|g| g.name == "kind=web").unwrap();
        assert_eq!(
            web.target,
            Some(GroupTarget::Label("kind".into(), "web".into()))
        );
        assert!(
            map.groups
                .iter()
                .any(|g| g.name == "(no kind)" && g.target.is_none())
        );
        assert_eq!(map.running(&rings).iter().sum::<usize>(), 1);
        // worst hosts on the inner ring
        let first_web = map
            .hosts
            .iter()
            .find(|p| (p.gx, p.gy) == (web.x, web.y))
            .unwrap();
        assert_eq!(first_web.tone, Tone::Bad);
        // hit test
        let h1 = map.placements("h1").next().unwrap();
        assert_eq!(
            map.host_at(h1.x + 2.0, h1.y - 2.0, DOT).unwrap().host_id,
            "h1"
        );
        assert!(map.host_at(web.x, web.y, DOT).is_none());
        for (i, a) in map.groups.iter().enumerate() {
            for b in &map.groups[i + 1..] {
                let d = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
                assert!(d >= a.radius + b.radius, "{} / {}", a.name, b.name);
            }
        }
        // folded: no hosts placed
        let folded: BTreeSet<String> = ["kind=web".to_string(), "kind=db".to_string()].into();
        let map = layout(
            &hosts,
            Display::Graph,
            GroupBy::Label,
            "kind",
            &folded,
            &selectors(),
        );
        let web = map.groups.iter().find(|g| g.name == "kind=web").unwrap();
        assert!(web.collapsed && web.total == 41);
        assert!(map.hosts.iter().all(|p| (p.gx, p.gy) != (web.x, web.y)));
        // folded hosts still count in their group (h1 is a db), but are not placed
        let db_index = map.groups.iter().position(|g| g.name == "kind=db").unwrap();
        assert_eq!(map.running(&rings)[db_index], 1);
        assert!(map.placements("h1").next().is_none());
    }
}
