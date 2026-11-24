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
//! pan by dragging, click a card to fold/unfold its hosts (positions glide
//! for 200 ms, [`Motion`]). Organize: dragging a card or a host moves it,
//! kept by the browser per display ([`Arrangement`]); Auto-arrange goes
//! back to the automatic layout.
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
use crate::live::{HostLive, HostTip, running_jobs};
use crate::oneshot::OneShotDialog;
use crate::pages::{assignments_line, badge, host_state};
use crate::runs::{ActiveRun, KeptRun, RunPanel, RunStatus, watch_run_over};
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
/// Graph display, a group with proactive and passive hosts: each half is
/// pushed this far from the card's vertical axis (a gap twice as wide).
const SPLIT: f64 = 36.0;
const AREA_GAP: f64 = 40.0;

fn is_ok(state: &str) -> bool {
    matches!(state, "compliant" | "reach_success")
}

fn is_bad(state: &str) -> bool {
    matches!(state, "not_compliant" | "reach_failed" | "error")
}

/// A host state as the map names it: a Reach's result reads as the
/// compliance it left (`reach_success` → `compliant`, `reach_failed` →
/// `not_compliant`); the host's details keep the Reach result.
pub fn map_state(state: &str) -> &str {
    match state {
        "reach_success" => "compliant",
        "reach_failed" => "not_compliant",
        other => other,
    }
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
    /// Index of its group in the layout.
    pub group: usize,
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
    /// A group with proactive and passive hosts: x of the line between its
    /// two columns, from `x`.
    pub split: Option<f64>,
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
            group,
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

/// A proactive host (an agent reports for it, D28), else passive (reached
/// by runners).
fn is_active(host: &api::HostSummary) -> bool {
    !host.agent.is_empty()
}

/// Positions of `n` hosts on concentric half rings right of the card
/// (mirrored when `left`), pushed `SPLIT` away from its vertical axis, and
/// the radius of the outer ring.
fn half_ring_offsets(n: usize, left: bool) -> (Vec<(f64, f64)>, f64) {
    let mut out = Vec::with_capacity(n);
    let mut radius = FIRST_RING;
    let mut outer = FIRST_RING;
    let mut ring = 0usize;
    while out.len() < n {
        let capacity = ((std::f64::consts::PI * radius) / RING_SPACING).floor() as usize;
        let take = capacity.min(n - out.len());
        // rings alternate their start angle so dots do not line up radially
        let shift = if ring.is_multiple_of(2) { 0.35 } else { 0.65 };
        for k in 0..take {
            let a = -std::f64::consts::FRAC_PI_2
                + (k as f64 + shift) / take as f64 * std::f64::consts::PI;
            let x = SPLIT + radius * a.cos();
            out.push((if left { -x } else { x }, radius * a.sin()));
        }
        outer = radius;
        radius += RING_SPACING;
        ring += 1;
    }
    (out, outer)
}

/// Graph display: the offset of every host of an unfolded group from its
/// card (in `hosts`' order) and the group's radius. Proactive hosts on the
/// left, passive ones on the right, when it has both (like ∞); else full
/// rings.
fn graph_offsets(hosts: &[&api::HostSummary]) -> (Vec<(f64, f64)>, f64) {
    let active = hosts.iter().filter(|h| is_active(h)).count();
    if active == 0 || active == hosts.len() {
        let (offsets, outer) = ring_offsets(hosts.len());
        let radius = if offsets.is_empty() {
            card_radius()
        } else {
            outer + DOT + 24.0
        };
        return (offsets, radius);
    }
    let (left, l_outer) = half_ring_offsets(active, true);
    let (right, r_outer) = half_ring_offsets(hosts.len() - active, false);
    let (mut left, mut right) = (left.into_iter(), right.into_iter());
    let offsets = hosts
        .iter()
        .map(|h| {
            if is_active(h) {
                left.next()
            } else {
                right.next()
            }
            .expect("one offset per host")
        })
        .collect();
    (offsets, l_outer.max(r_outer) + SPLIT + DOT + 24.0)
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

/// The groups of `hosts`, the same in both displays: one per assignment, a
/// host in every assignment selecting it (D29), drawn in each.
/// `assignments`: selector of each assignment (D23), for its Assess / Reach.
fn members<'a>(
    hosts: &'a BTreeMap<String, api::HostSummary>,
    collapsed: &BTreeSet<String>,
    assignments: &BTreeMap<String, api::HostSelector>,
) -> Vec<Members<'a>> {
    let mut members: BTreeMap<String, (Option<GroupTarget>, Vec<&api::HostSummary>)> =
        BTreeMap::new();
    for host in hosts.values() {
        let groups: Vec<(String, Option<GroupTarget>)> = if host.assignments.is_empty() {
            vec![("(unassigned)".to_string(), None)]
        } else {
            host.assignments
                .iter()
                .map(|reference| {
                    let target = assignments.get(reference).map(|selector| {
                        GroupTarget::Assignment(reference.clone(), selector.clone())
                    });
                    (reference.clone(), target)
                })
                .collect()
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
    collapsed: &BTreeSet<String>,
    assignments: &BTreeMap<String, api::HostSelector>,
) -> Layout {
    let groups = members(hosts, collapsed, assignments);
    let mut layout = match display {
        // same groups and name order in both displays (a host changing
        // state stays where it is); only the shape differs
        Display::Graph => graph_layout(groups),
        Display::Areas => areas_layout(groups),
    };
    if layout.groups.is_empty() {
        layout.bounds = (-200.0, -200.0, 200.0, 200.0);
    }
    layout
}

/// What the user moved on the map (Organize), per display, kept by the
/// browser: a moved group's card center, and a moved host's offset from
/// its group's card center (so it follows its group). Everything else stays
/// in the automatic layout; "Auto-arrange" forgets it all.
#[derive(Clone, Default, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
struct Arrangement {
    #[serde(default)]
    groups: BTreeMap<String, (f64, f64)>,
    /// group name -> host id -> offset
    #[serde(default)]
    hosts: BTreeMap<String, BTreeMap<String, (f64, f64)>>,
}

impl Arrangement {
    fn key(display: Display) -> String {
        format!("regency.map.arrangement.{}", display.name())
    }

    fn saved(display: Display) -> Self {
        crate::client::storage()
            .and_then(|s| s.get_item(&Self::key(display)).ok().flatten())
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default()
    }

    fn save(&self, display: Display) {
        let Some(storage) = crate::client::storage() else {
            return;
        };
        let key = Self::key(display);
        if self.is_empty() {
            let _ = storage.remove_item(&key);
        } else if let Ok(json) = serde_json::to_string(self) {
            let _ = storage.set_item(&key, &json);
        }
    }

    fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.hosts.is_empty()
    }
}

/// Move the groups and hosts of `layout` where the user put them; areas
/// grow to hold their moved hosts.
fn arrange(layout: &mut Layout, arrangement: &Arrangement) {
    // nothing on the map yet (loading): its default bounds stay
    if arrangement.is_empty() || layout.groups.is_empty() {
        return;
    }
    let mut shifts = vec![(0.0, 0.0); layout.groups.len()];
    for (group, shift) in layout.groups.iter_mut().zip(&mut shifts) {
        if let Some(&(x, y)) = arrangement.groups.get(&group.name) {
            *shift = (x - group.x, y - group.y);
            group.x = x;
            group.y = y;
            if let Some(area) = group.area.as_mut() {
                area.x += shift.0;
                area.y += shift.1;
            }
        }
    }
    for p in &mut layout.hosts {
        let group = &mut layout.groups[p.group];
        let (dx, dy) = shifts[p.group];
        match arrangement
            .hosts
            .get(&group.name)
            .and_then(|hosts| hosts.get(&p.host_id))
        {
            Some(&(ox, oy)) => (p.x, p.y) = (group.x + ox, group.y + oy),
            None => (p.x, p.y) = (p.x + dx, p.y + dy),
        }
        (p.gx, p.gy) = (group.x, group.y);
        if let Some(area) = group.area.as_mut() {
            let m = CELL / 2.0;
            let (x1, y1) = (
                (area.x + area.w).max(p.x + m),
                (area.y + area.h).max(p.y + m),
            );
            let line = area.split.map(|s| area.x + s);
            area.x = area.x.min(p.x - m);
            area.y = area.y.min(p.y - m);
            area.split = line.map(|l| l - area.x);
            area.w = x1 - area.x;
            area.h = y1 - area.y;
        }
    }
    layout.areas = layout.groups.iter().filter_map(|g| g.area).collect();
    layout.bounds = Layout::empty().bounds;
    for i in 0..layout.groups.len() {
        let g = &layout.groups[i];
        let (x0, y0, x1, y1) = match g.area {
            Some(a) => (a.x, a.y, a.x + a.w, a.y + a.h),
            None => (
                g.x - g.radius,
                g.y - g.radius,
                g.x + g.radius,
                g.y + g.radius,
            ),
        };
        layout.grow(x0, y0, x1, y1);
    }
    for i in 0..layout.hosts.len() {
        let (x, y) = (layout.hosts[i].x, layout.hosts[i].y);
        layout.grow(x - DOT, y - DOT, x + DOT, y + DOT);
    }
}

/// Something picked on the map (Organize): a group (by name) or a host in
/// one of its groups. Picked things are dragged together.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Grab {
    Group(String),
    Host { group: String, host: String },
}

type Picks = BTreeSet<Grab>;

impl Layout {
    fn group_named(&self, name: &str) -> Option<(usize, &Group)> {
        self.groups.iter().enumerate().find(|(_, g)| g.name == name)
    }

    /// Index in `hosts` of host `host` drawn in group `group`.
    fn placed_in(&self, group: &str, host: &str) -> Option<usize> {
        let (index, _) = self.group_named(group)?;
        self.of_host
            .get(host)?
            .iter()
            .find(|(g, _)| *g == index)
            .and_then(|(_, i)| *i)
    }

    /// What `picks` moves: indices of the groups, and of the hosts moved on
    /// their own (a host of a picked group moves with it).
    fn picked(&self, picks: &Picks) -> (Vec<usize>, Vec<usize>) {
        let groups: Vec<usize> = picks
            .iter()
            .filter_map(|p| match p {
                Grab::Group(name) => self.group_named(name).map(|(i, _)| i),
                Grab::Host { .. } => None,
            })
            .collect();
        let hosts = picks
            .iter()
            .filter_map(|p| match p {
                Grab::Host { group, host } => self.placed_in(group, host),
                Grab::Group(_) => None,
            })
            .filter(|i| !groups.contains(&self.hosts[*i].group))
            .collect();
        (groups, hosts)
    }

    /// This layout with `picks` moved by (`dx`, `dy`): a frame of a drag.
    fn shifted(&self, picks: &Picks, dx: f64, dy: f64) -> Layout {
        let mut out = self.clone();
        let (groups, hosts) = self.picked(picks);
        for &index in &groups {
            let group = &mut out.groups[index];
            group.x += dx;
            group.y += dy;
            if let Some(area) = group.area.as_mut() {
                area.x += dx;
                area.y += dy;
            }
        }
        for p in &mut out.hosts {
            if groups.contains(&p.group) {
                p.x += dx;
                p.y += dy;
                (p.gx, p.gy) = (out.groups[p.group].x, out.groups[p.group].y);
            }
        }
        for i in hosts {
            out.hosts[i].x += dx;
            out.hosts[i].y += dy;
        }
        out.areas = out.groups.iter().filter_map(|g| g.area).collect();
        out
    }

    /// `picks` moved by (`dx`, `dy`) for good: written into `arrangement`.
    fn moved(&self, picks: &Picks, dx: f64, dy: f64, arrangement: &mut Arrangement) {
        let (groups, hosts) = self.picked(picks);
        for index in groups {
            let g = &self.groups[index];
            arrangement
                .groups
                .insert(g.name.clone(), (g.x + dx, g.y + dy));
        }
        for i in hosts {
            let p = &self.hosts[i];
            let g = &self.groups[p.group];
            arrangement
                .hosts
                .entry(g.name.clone())
                .or_default()
                .insert(p.host_id.clone(), (p.x + dx - g.x, p.y + dy - g.y));
        }
    }

    /// What a selection rectangle holds (map units): the groups whose card
    /// center is inside, and the hosts inside but not in those groups.
    fn in_box(&self, x0: f64, y0: f64, x1: f64, y1: f64) -> Picks {
        let (x0, x1) = (x0.min(x1), x0.max(x1));
        let (y0, y1) = (y0.min(y1), y0.max(y1));
        let inside = |x: f64, y: f64| x >= x0 && x <= x1 && y >= y0 && y <= y1;
        let groups: Vec<usize> = (0..self.groups.len())
            .filter(|i| inside(self.groups[*i].x, self.groups[*i].y))
            .collect();
        let mut picks: Picks = groups
            .iter()
            .map(|i| Grab::Group(self.groups[*i].name.clone()))
            .collect();
        picks.extend(
            self.hosts
                .iter()
                .filter(|p| !groups.contains(&p.group) && inside(p.x, p.y))
                .map(|p| Grab::Host {
                    group: self.groups[p.group].name.clone(),
                    host: p.host_id.clone(),
                }),
        );
        picks
    }
}

/// Fold / unfold animation: the layout before the change, from which the
/// next layouts glide for a moment (hosts unfolding leave their card,
/// folding ones go back under it). Positions only, eased out.
struct Motion {
    groups: HashMap<String, (f64, f64, Option<Area>)>,
    /// group name -> host id -> position
    hosts: HashMap<String, HashMap<String, (f64, f64)>>,
    before: Vec<Placed>,
    names: Vec<String>,
    /// Hosts no longer drawn, their group's index in the new layout (set by
    /// the first frame).
    leaving: Option<Vec<Placed>>,
    /// Time of the first frame.
    start: Option<f64>,
}

const MOTION_MS: f64 = 200.0;

impl Motion {
    fn of(layout: &Layout) -> Self {
        let mut hosts: HashMap<String, HashMap<String, (f64, f64)>> = HashMap::new();
        for p in &layout.hosts {
            hosts
                .entry(layout.groups[p.group].name.clone())
                .or_default()
                .insert(p.host_id.clone(), (p.x, p.y));
        }
        Motion {
            groups: layout
                .groups
                .iter()
                .map(|g| (g.name.clone(), (g.x, g.y, g.area)))
                .collect(),
            hosts,
            before: layout.hosts.clone(),
            names: layout.groups.iter().map(|g| g.name.clone()).collect(),
            leaving: None,
            start: None,
        }
    }

    /// `to` at `t` (0 to 1) of the way from the layout before.
    fn frame(&mut self, to: &Layout, t: f64) -> Layout {
        let e = 1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3);
        let lerp = |a: f64, b: f64| a + (b - a) * e;
        let leaving = self.leaving.get_or_insert_with(|| {
            self.before
                .iter()
                .filter(|p| to.placed_in(&self.names[p.group], &p.host_id).is_none())
                .filter_map(|p| {
                    let (index, _) = to.group_named(&self.names[p.group])?;
                    Some(Placed {
                        group: index,
                        ..p.clone()
                    })
                })
                .collect()
        });
        let mut out = to.clone();
        for g in &mut out.groups {
            if let Some((x, y, area)) = self.groups.get(&g.name) {
                g.x = lerp(*x, g.x);
                g.y = lerp(*y, g.y);
                if let (Some(a), Some(b)) = (area, g.area.as_mut()) {
                    b.x = lerp(a.x, b.x);
                    b.y = lerp(a.y, b.y);
                    b.w = lerp(a.w, b.w);
                    b.h = lerp(a.h, b.h);
                }
            }
        }
        for p in &mut out.hosts {
            let g = &out.groups[p.group];
            let (fx, fy) = self
                .hosts
                .get(&g.name)
                .and_then(|hosts| hosts.get(&p.host_id))
                .copied()
                // unfolding: from its card
                .or_else(|| self.groups.get(&g.name).map(|(x, y, _)| (*x, *y)))
                .unwrap_or((p.x, p.y));
            p.x = lerp(fx, p.x);
            p.y = lerp(fy, p.y);
            (p.gx, p.gy) = (g.x, g.y);
        }
        // folding: back under their card
        for p in leaving.iter() {
            let Some(g) = out.groups.get(p.group) else {
                continue;
            };
            let mut p = p.clone();
            p.x = lerp(p.x, g.x);
            p.y = lerp(p.y, g.y);
            (p.gx, p.gy) = (g.x, g.y);
            out.hosts.push(p);
        }
        out.areas = out.groups.iter().filter_map(|g| g.area).collect();
        out
    }
}

/// Graph display: a card per group, its hosts on rings around it.
fn graph_layout(groups: Vec<Members>) -> Layout {
    let rings: Vec<(Vec<(f64, f64)>, f64)> = groups
        .iter()
        .map(|g| {
            if g.collapsed {
                (Vec::new(), card_radius())
            } else {
                graph_offsets(&g.hosts)
            }
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

/// Areas display: the cells of an unfolded group's hosts (in `hosts`'
/// order), the area's size and the x of the line between its columns:
/// proactive hosts in a left column, passive ones in a right column under
/// the card, when it has both; else [`area_cells`].
fn split_cells(hosts: &[&api::HostSummary]) -> (Cells, Option<f64>) {
    let active = hosts.iter().filter(|h| is_active(h)).count();
    let passive = hosts.len() - active;
    if active == 0 || passive == 0 {
        return (area_cells(hosts.len()), None);
    }
    let card_cols = ((CARD_W + AREA_PAD) / CELL).ceil() as usize;
    let card_rows = ((CARD_H + AREA_PAD) / CELL).ceil() as usize;
    // about square overall; each column as wide as its hosts need to end
    // at about the same height
    let cells_needed = active + passive + card_cols * card_rows;
    let rows = (cells_needed as f64)
        .sqrt()
        .ceil()
        .max((card_rows + 1) as f64) as usize;
    let left_cols = active.div_ceil(rows).max(1);
    let right_cols = (passive + card_cols * card_rows)
        .div_ceil(rows)
        .max(card_cols + 2);
    // one empty column between them, holding the line
    let right_x = AREA_PAD + (left_cols + 1) as f64 * CELL;
    let mut left = (0..active).map(|i| {
        (
            AREA_PAD + ((i % left_cols) as f64 + 0.5) * CELL,
            AREA_PAD + ((i / left_cols) as f64 + 0.5) * CELL,
        )
    });
    let mut right = Vec::with_capacity(passive);
    let mut row = 0;
    while right.len() < passive {
        for col in 0..right_cols {
            if right.len() == passive {
                break;
            }
            if row < card_rows && col >= right_cols - card_cols {
                continue;
            }
            right.push((
                right_x + (col as f64 + 0.5) * CELL,
                AREA_PAD + (row as f64 + 0.5) * CELL,
            ));
        }
        row += 1;
    }
    let rows = active.div_ceil(left_cols).max(row).max(card_rows);
    let mut right = right.into_iter();
    let cells = hosts
        .iter()
        .map(|h| {
            if is_active(h) {
                left.next()
            } else {
                right.next()
            }
            .expect("one cell per host")
        })
        .collect();
    let w = 2.0 * AREA_PAD + (left_cols + 1 + right_cols) as f64 * CELL;
    let h = 2.0 * AREA_PAD + rows as f64 * CELL;
    (
        (cells, w, h),
        Some(AREA_PAD + (left_cols as f64 + 0.5) * CELL),
    )
}

fn areas_layout(groups: Vec<Members>) -> Layout {
    let cells: Vec<(Cells, Option<f64>)> = groups
        .iter()
        .map(|g| {
            if g.collapsed {
                // the card alone
                (
                    (Vec::new(), CARD_W + 2.0 * AREA_PAD, CARD_H + 2.0 * AREA_PAD),
                    None,
                )
            } else {
                split_cells(&g.hosts)
            }
        })
        .collect();
    let sizes: Vec<(f64, f64)> = cells.iter().map(|((_, w, h), _)| (*w, *h)).collect();
    let corners = shelves(&sizes, AREA_GAP, false);

    let mut layout = Layout::empty();
    for ((group, ((offsets, w, h), split)), (x, y)) in groups.into_iter().zip(cells).zip(corners) {
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
            split,
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
        hosts,
        ready,
        jobs,
        flashes,
        ..
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
    // hosts are grouped by assignment (D23: colours mean "against the
    // assignments"), hosts without one in "(unassigned)"
    let collapsed = RwSignal::new(BTreeSet::<String>::new());
    let rings = Memo::new(move |_| jobs.with(rings));
    // where the user moved groups and hosts (Organize), per display
    let arrangement = RwSignal::new(Arrangement::saved(display.get_untracked()));
    let map = Memo::new(move |_| {
        hosts.with(|hosts| {
            collapsed.with(|collapsed| {
                selectors.with(|selectors| {
                    let mut map = layout(hosts, display.get(), collapsed, selectors);
                    arrangement.with(|a| arrange(&mut map, a));
                    map
                })
            })
        })
    });
    // Organize: cards and hosts are picked (click, Ctrl + click, a
    // selection rectangle) and dragged together
    let organize = RwSignal::new(false);
    let picks = RwSignal::new(Picks::new());
    // the picks being dragged, by how much (map units)
    let grab = RwSignal::new(None::<(f64, f64)>);
    // a selection rectangle being drawn (map units: start, end), and
    // whether it adds to the picks (Ctrl)
    let marquee = RwSignal::new(None::<(f64, f64, f64, f64)>);
    let marquee_adds = StoredValue::new(false);
    // pointer position at the start of that drag, and map units per pixel
    let grab_start = StoredValue::new((0.0f64, 0.0f64, 1.0f64));
    // fold / unfold: positions glide from the layout before
    let motion = StoredValue::new(None::<Motion>);

    // view box: zoom (wheel, around the pointer) and pan (drag)
    let scale = RwSignal::new(1.0f64);
    let center = RwSignal::new(None::<(f64, f64)>); // None = fit the whole map
    // the map's size the view is fitted to: None = the whole map, as it is
    // now; kept while moving or folding things, so the view stays put
    let fitted = RwSignal::new(None::<(f64, f64)>);
    let drag = RwSignal::new(None::<(f64, f64)>);
    let moved = RwSignal::new(false);
    let half = move || {
        fitted.get().unwrap_or_else(|| {
            let (x0, y0, x1, y1) = map.with(|m| m.bounds);
            ((x1 - x0) / 2.0 + 40.0, (y1 - y0) / 2.0 + 40.0)
        })
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
    // the view as it is, kept from now on (whatever the map's size becomes)
    let hold_view = move || {
        if fitted.get_untracked().is_none() {
            fitted.set(Some(untrack(half)));
        }
        center.set(Some(untrack(view_center)));
    };
    // the layout changes next (fold, unfold, auto-arrange): glide to it
    let glide = move || {
        motion.set_value(Some(map.with_untracked(Motion::of)));
    };
    // the host under the pointer of a mouse event, if any, and its group
    let grabbed_host = move |ev: &web_sys::MouseEvent| {
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
        map.with_untracked(|m| {
            m.host_at(x, y, radius)
                .map(|p| (p.host_id.clone(), m.groups[p.group].name.clone()))
        })
    };
    let host_under = move |ev: &web_sys::MouseEvent| grabbed_host(ev).map(|(id, _)| id);
    // the map point under the pointer
    let map_point = move |ev: &web_sys::MouseEvent| {
        let (w, h, left, top) = metrics(ev.current_target())?;
        Some(untrack(view).map_point(
            ev.client_x() as f64 - left,
            ev.client_y() as f64 - top,
            w,
            h,
        ))
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
    // D24: the selected host's running jobs (live)
    let selected_running = Signal::derive(move || {
        selected.with(|id| {
            id.as_deref()
                .map(|id| jobs.with(|jobs| running_jobs(jobs, id)))
                .unwrap_or_default()
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
    // the group cards' positions, as drawn (they glide and are dragged too)
    let cards = RwSignal::new(HashMap::<String, (f64, f64)>::new());
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
            flashes.track();
            theme.track();
            grab.track();
            picks.track();
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
        canvas::animate(alive, move |now| {
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
            // a drag or a fold in progress: this frame's positions
            let grabbed = grab.get_untracked();
            let moving = map.with_untracked(|base| match grabbed {
                Some((dx, dy)) => Some(picks.with_untracked(|p| base.shifted(p, dx, dy))),
                None => motion
                    .try_update_value(|m| {
                        let step = m.as_mut()?;
                        let start = *step.start.get_or_insert(now);
                        let t = (now - start) / MOTION_MS;
                        if t >= 1.0 {
                            *m = None;
                            return None;
                        }
                        Some(step.frame(base, t))
                    })
                    .flatten(),
            });
            if moving.is_some() && grabbed.is_none() {
                // the next frame of the glide
                dirty.store(true, Ordering::Relaxed);
            }
            // the cards (SVG) follow the canvas
            let positions = |layout: &Layout| -> HashMap<String, (f64, f64)> {
                layout
                    .groups
                    .iter()
                    .map(|g| (g.name.clone(), (g.x, g.y)))
                    .collect()
            };
            let now_cards = match &moving {
                Some(layout) => positions(layout),
                None => map.with_untracked(positions),
            };
            if cards.with_untracked(|c| *c != now_cards) {
                cards.set(now_cards);
            }
            let draw = |layout: &Layout| {
                // hosts picked on their own (Organize)
                let picked = picks.with_untracked(|p| {
                    if p.is_empty() {
                        Vec::new()
                    } else {
                        layout.picked(p).1
                    }
                });
                rings.with_untracked(|rings| {
                    flashes.with_untracked(|flashes| {
                        let frame = Frame {
                            layout,
                            rings,
                            flashes,
                            view,
                            hovered: hovered.as_deref(),
                            selected: selected.as_deref(),
                            picked: &picked,
                            names,
                        };
                        if let Some(ctx) = context(&hosts_canvas) {
                            canvas::draw_hosts(&ctx, w, h, dpr, &palette, &frame);
                        }
                    })
                })
            };
            match &moving {
                Some(layout) => draw(layout),
                None => map.with_untracked(draw),
            }
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
    // the run asked last: followed live in the right panel
    let active_run = RwSignal::new(None::<ActiveRun>);
    // the page's owner: a run's status outlives the confirmation dialog
    // (where `confirm` runs, disposed once closed)
    let page_owner = StoredValue::new(Owner::current().expect("the Live view has an owner"));
    // runs not over yet, kept by the browser: their panel comes back after a
    // reload or another page, until every job of theirs has ended
    let kept = RwSignal::new(KeptRun::load(js_sys::Date::now() as i64));
    let forget = move |id: &str| {
        // from storage itself too: the page may be gone (a refusal arriving
        // after the user left)
        let mut runs = KeptRun::load(js_sys::Date::now() as i64);
        runs.retain(|r| r.command_id != id);
        KeptRun::save(&runs);
        let _ = kept.try_update(|k| k.retain(|r| r.command_id != id));
    };
    let reopen = move |run: &KeptRun| {
        let status =
            page_owner.with_value(|owner| owner.with(|| RwSignal::new(RunStatus::Accepted)));
        active_run.set(Some(run.active(status)));
    };
    // the latest run not over: shown again when the page opens
    if let Some(last) = kept.with_untracked(|k| k.last().cloned()) {
        reopen(&last);
    }
    let confirm = move || {
        let Some(run) = pending.get_untracked() else {
            return;
        };
        pending.set(None);
        let id = command_id("run");
        let status =
            page_owner.with_value(|owner| owner.with(|| RwSignal::new(RunStatus::Sending)));
        // the panel opens at once, before backend answers
        selected.set(None);
        let active = ActiveRun {
            command_id: id.clone(),
            job: run.job,
            target: run.target.describe(),
            expected: run.hosts,
            started_at: js_sys::Date::now() as i64,
            status,
        };
        kept.update(|k| k.push(KeptRun::of(&active)));
        KeptRun::save(&kept.get_untracked());
        active_run.set(Some(active));
        spawn_local(async move {
            let request = api::RunNowRequest {
                command_id: id,
                selector: Some(run.target.selector()),
                job: run.job as i32,
                pool: String::new(),
                one_shot_yaml: String::new(),
            };
            let kept_id = request.command_id.clone();
            let result = match client().run_now(authed(request, &session.token())).await {
                Ok(_) => RunStatus::Accepted,
                Err(status) => {
                    session.check(&status);
                    forget(&kept_id);
                    RunStatus::Refused(status.message().to_string())
                }
            };
            let _ = status.try_set(result);
        });
    };
    // the right panel shows the selected host, else the run
    let showing_host = Memo::new(move |_| selected.with(Option::is_some));
    let panel_open = Memo::new(move |_| showing_host.get() || active_run.with(Option::is_some));

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
        if panel_open.get() {
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
        let mut class = String::from("map");
        if hovered.get().is_some() {
            class.push_str(" over-host");
        }
        if organize.get() {
            class.push_str(" organizing");
        }
        if grab.with(Option::is_some) {
            class.push_str(" grabbing");
        }
        class
    };
    // a drag (Organize) ends: the picks stay where they were dropped; a
    // selection rectangle picks what it holds
    let drop_grab = move || {
        if let Some((x0, y0, x1, y1)) = marquee.get_untracked() {
            marquee.set(None);
            if moved.get_untracked() {
                let inside = map.with_untracked(|m| m.in_box(x0, y0, x1, y1));
                if marquee_adds.get_value() {
                    picks.update(|p| p.extend(inside));
                } else {
                    picks.set(inside);
                }
            } else if !marquee_adds.get_value() {
                // a click on empty space
                picks.set(Picks::new());
            }
            return;
        }
        let Some((dx, dy)) = grab.get_untracked() else {
            return;
        };
        grab.set(None);
        if !moved.get_untracked() {
            return;
        }
        let mut arranged = arrangement.get_untracked();
        picks.with_untracked(|p| map.with_untracked(|m| m.moved(p, dx, dy, &mut arranged)));
        arranged.save(display.get_untracked());
        arrangement.set(arranged);
    };
    let toggle_group = move |name: String| {
        hold_view();
        glide();
        collapsed.update(|set| {
            if !set.remove(&name) {
                set.insert(name);
            }
        });
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
                // while the right panel floats over the map, the toolbar stops
                // at its left edge (the runs' buttons stay visible)
                <div class="toolbar" class:beside-panel=move || panel_open.get()>
                    <span class="muted">"Display"</span>
                    <select title="Graph: hosts around their group; areas: hosts inside their group"
                        on:change=move |ev| {
                            let value = if event_target_value(&ev) == "areas" { Display::Areas } else { Display::Graph };
                            value.save();
                            scale.set(1.0);
                            center.set(None);
                            fitted.set(None);
                            arrangement.set(Arrangement::saved(value));
                            display.set(value);
                        }>
                        <option value="graph" selected=move || display.get() == Display::Graph>"graph"</option>
                        <option value="areas" selected=move || display.get() == Display::Areas>"areas"</option>
                    </select>
                    // one button: fold every group, or unfold them all once all are folded
                    <button title="Fold or unfold every group"
                        on:click=move |_| {
                            hold_view();
                            glide();
                            if all_folded() {
                                collapsed.set(BTreeSet::new());
                            } else {
                                collapsed.set(map.with_untracked(|m| m.groups.iter().map(|g| g.name.clone()).collect()));
                            }
                        }>
                        {move || if all_folded() { "Unfold" } else { "Fold" }}
                    </button>
                    <button class:active=move || organize.get()
                        title="Move group cards and hosts: drag them; pick several with Ctrl + click or a selection rectangle (drag on empty space); pan with the middle button"
                        on:click=move |_| {
                            organize.update(|o| *o = !*o);
                            picks.set(Picks::new());
                        }>
                        "Organize"
                    </button>
                    <button title="Put every group and host back where the map places them"
                        disabled=move || arrangement.with(Arrangement::is_empty)
                        on:click=move |_| {
                            glide();
                            let empty = Arrangement::default();
                            empty.save(display.get_untracked());
                            arrangement.set(empty);
                            // the whole map in view
                            scale.set(1.0);
                            center.set(None);
                            fitted.set(None);
                        }>
                        "Auto-arrange"
                    </button>
                    <span class="muted">{move || if ready.get() { format!("{} hosts, live", hosts.with(|h| h.len())) } else { "loading…".into() }}</span>
                    <span class="muted">{move || message.get()}</span>
                    // runs not over: their panel, whatever happened since
                    <span class="run-chips">
                    <For each=move || kept.get() key=|run| run.command_id.clone() let:run>
                        {
                        let shown = {
                            let id = run.command_id.clone();
                            move || !showing_host.get()
                                && active_run.with(|a| a.as_ref().is_some_and(|a| a.command_id == id))
                        };
                        // the run is forgotten (button gone) once its jobs ended, panel
                        // open or not (a refused command is forgotten by `confirm`)
                        watch_run_over(run.command_id.clone(), {
                            let id = run.command_id.clone();
                            Callback::new(move |()| forget(&id))
                        });
                        let label = format!("Job in progress : {} {}", job_label(run.job), run.target);
                        view! {
                            <button class="run-chip" class:active=shown title="A run in progress: show it"
                                on:click=move |_| {
                                    selected.set(None);
                                    let id = run.command_id.clone();
                                    if !active_run.with_untracked(|a| a.as_ref().is_some_and(|a| a.command_id == id)) {
                                        reopen(&run);
                                    }
                                }>
                                <i class="live-dot"></i>{label}
                            </button>
                        }
                        }
                    </For>
                    </span>
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
                    on:pointerdown=move |ev: web_sys::PointerEvent| {
                        hovered.set(None);
                        moved.set(false);
                        let (x, y) = (ev.client_x() as f64, ev.client_y() as f64);
                        // Organize, main button: pick and move cards and
                        // hosts, or draw a selection rectangle
                        if organize.get_untracked() && ev.button() == 0 {
                            let Some((w, h, _, _)) = metrics(ev.current_target()) else { return };
                            let ctrl = ev.ctrl_key() || ev.meta_key();
                            let card = ev
                                .target()
                                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                                .and_then(|e| e.closest("g.group").ok().flatten())
                                .and_then(|e| e.get_attribute("data-group"));
                            let what = match card {
                                Some(name) => Some(Grab::Group(name)),
                                None => grabbed_host(&ev).map(|(host, group)| Grab::Host { group, host }),
                            };
                            match what {
                                Some(what) if ctrl => {
                                    // add or remove it; not a click (no fold,
                                    // no selection of the host)
                                    moved.set(true);
                                    picks.update(|p| if !p.remove(&what) { p.insert(what); });
                                }
                                Some(what) => {
                                    if !picks.with_untracked(|p| p.contains(&what)) {
                                        picks.set([what].into());
                                    }
                                    hold_view();
                                    grab_start.set_value((x, y, units_per_pixel(w, h)));
                                    grab.set(Some((0.0, 0.0)));
                                }
                                None => {
                                    let Some((mx, my)) = map_point(&ev) else { return };
                                    hold_view();
                                    marquee_adds.set_value(ctrl);
                                    grab_start.set_value((x, y, units_per_pixel(w, h)));
                                    marquee.set(Some((mx, my, mx, my)));
                                }
                            }
                            return;
                        }
                        // middle button (or not organizing): pan
                        if ev.button() == 0 || ev.button() == 1 {
                            if ev.button() == 1 {
                                ev.prevent_default();
                            }
                            drag.set(Some((x, y)));
                        }
                    }
                    // the middle button pans, without the browser's autoscroll
                    on:mousedown=move |ev: web_sys::MouseEvent| if ev.button() == 1 { ev.prevent_default(); }
                    on:pointerup=move |_| { drag.set(None); drop_grab(); }
                    on:pointercancel=move |_| { drag.set(None); grab.set(None); marquee.set(None); }
                    on:pointerleave=move |_| { drag.set(None); drop_grab(); hovered.set(None); }
                    on:click=move |ev| {
                        if moved.get_untracked() {
                            return;
                        }
                        // a card handles its own clicks (hosts may lie under
                        // it: groups moved over others, or gliding)
                        let on_card = ev
                            .target()
                            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                            .and_then(|e| e.closest("g.group").ok().flatten())
                            .is_some();
                        if on_card {
                            return;
                        }
                        // a host: show it in the right panel; empty space: deselect
                        selected.set(host_under(&ev));
                    }
                    on:pointermove=move |ev| {
                        if grab.with_untracked(Option::is_some) || marquee.with_untracked(Option::is_some) {
                            let (x0, y0, k) = grab_start.get_value();
                            let (nx, ny) = (ev.client_x() as f64, ev.client_y() as f64);
                            if !moved.get_untracked() {
                                if (nx - x0).abs() + (ny - y0).abs() < 4.0 {
                                    return;
                                }
                                // a drag, not a click (a captured pointer's
                                // click would miss the card's buttons)
                                if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                                    let _ = el.set_pointer_capture(ev.pointer_id());
                                }
                            }
                            moved.set(true);
                            let (dx, dy) = ((nx - x0) * k, (ny - y0) * k);
                            if grab.with_untracked(Option::is_some) {
                                grab.set(Some((dx, dy)));
                            } else {
                                marquee.update(|m| if let Some((mx, my, ex, ey)) = m {
                                    (*ex, *ey) = (*mx + dx, *my + dy);
                                });
                            }
                            return;
                        }
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
                        // the target too: the buttons appear once the assignments are loaded
                        key=|(g, running)| (g.name.clone(), g.total, g.ok, g.bad, *running, g.collapsed, g.target.is_some())
                        let:card>
                        {group_card(card.0, card.1, CardSignals { cards, picks, organize, moved }, toggle_group, ask)}
                    </For>
                    // selection rectangle (Organize)
                    {move || marquee.get().map(|(x0, y0, x1, y1)| view! {
                        <rect class="marquee" x=x0.min(x1) y=y0.min(y1)
                            width=(x1 - x0).abs() height=(y1 - y0).abs() />
                    })}
                </svg>
                // legend: a layer on top of the hosts, bottom left
                        <details class="map-legend">
                            <summary>"Legend"</summary>
                            <ul class="legend">
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="var(--ok)" /></svg> " compliant"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="var(--bad)" /></svg> " not compliant / error"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="var(--warn)" /></svg> " aborted / assignments in conflict"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="var(--unknown)" /></svg> " not checked yet, or against an older expected state"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="var(--unassigned)" /></svg> " unassigned: no expected state"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="var(--job)" /></svg> " job in progress (pending, running or abort requested)"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6" fill="none" stroke="var(--warn)" stroke-width="2" stroke-dasharray="2 2" /></svg> " conflicting definitions or certificate pending"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="7" fill="none" stroke="var(--accent)" stroke-width="2" /></svg> " selected host"</li>
                                <li><svg width="16" height="16"><path d="M8 1 L15 8 L8 15 L1 8 Z" fill="var(--unknown)" /></svg> " proactive host (regops): its own Git state, coloured by its last pass"</li>
                                <li><svg width="16" height="16"><path d="M8 1 L15 8 L8 15 L1 8 Z" fill="none" stroke="var(--warn)" stroke-width="2" /></svg> " proactive host behind its repository's latest commit"</li>
                                <li><svg width="16" height="16"><path d="M8 1 V15" stroke="var(--muted)" stroke-width="2" stroke-dasharray="2 2" /></svg> " a group with both: proactive hosts on the left, passive ones on the right"</li>
                            </ul>
                        </details>
                </div>
            </section>
            // the right panel: a selected host (a click on it), else the
            // run asked last
            <Show when=move || panel_open.get()>
            {splitter(right_width, right_open, -1.0)}
            <aside class="map-side map-right">
                {
                let navigate = navigate.clone();
                move || if showing_host.get() {
                    let navigate = navigate.clone();
                    view! {
                    {move || active_run.with(Option::is_some).then(|| view! {
                        <button class="small back" on:click=move |_| selected.set(None)>"← Back to the run"</button>
                    })}
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
                    <h2>"Last known state"</h2>
                    <HostStateExplorer with_last_run=true host=Signal::derive(move || selected_host.with(|h| h.as_ref().map(|(host, _)| host.clone()))) />
                    }.into_any()
                } else {
                    active_run.get().map(|run| view! {
                        <RunPanel run=run
                            on_close=Callback::new(move |()| active_run.set(None))
                            on_host=Callback::new(move |id: String| selected.set(Some(id))) />
                    }).into_any()
                }
                }
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
                                GroupTarget::Assignment(_, selector) => view! { <p class="muted">{format!("Selects {}.", describe_selector(selector))}</p> },
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
            <dt>"Last outcome"</dt><dd><span class="ts">{format_ms(host.state_at)}</span></dd>
            <dt>"Last seen"</dt><dd><span class="ts">{format_ms(host.last_seen)}</span></dd>
            <dt>"Labels"</dt><dd>{host.labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")}</dd>
        </dl>
        {(!host.error.is_empty()).then(|| view! { <p class="error">{host.error.clone()}</p> })}
    }
}

/// The map's state a group card reads.
#[derive(Clone, Copy)]
struct CardSignals {
    /// Where each card is drawn.
    cards: RwSignal<HashMap<String, (f64, f64)>>,
    picks: RwSignal<Picks>,
    organize: RwSignal<bool>,
    /// The pointer moved since it went down (a drag, not a click).
    moved: RwSignal<bool>,
}

/// A group (assignment, label or state) node: name, counts, compliance donut,
/// Assess / Reach buttons.
fn group_card(
    group: Group,
    running: usize,
    signals: CardSignals,
    toggle_group: impl Fn(String) + Copy + 'static,
    ask: impl Fn(GroupTarget, api::JobKind, usize) + Copy + 'static,
) -> impl IntoView {
    let CardSignals {
        cards,
        picks,
        organize,
        moved,
    } = signals;
    let (w, h) = (CARD_W, CARD_H);
    let (left, top) = (-w / 2.0, -h / 2.0);
    let name = group.name.clone();
    let toggle = {
        let name = name.clone();
        move |_| {
            // Organize: a click picks the card (pointerdown), no fold
            if !moved.get_untracked() && !organize.get_untracked() {
                toggle_group(name.clone());
            }
        }
    };
    let picked_as = Grab::Group(group.name.clone());
    let (name_for_title, collapsed_now) = (group.name.clone(), group.collapsed);
    // where it is drawn (it glides and is dragged), else where it belongs
    let (gx, gy) = (group.x, group.y);
    let transform = move || {
        let (x, y) = cards.with(|c| c.get(&name).copied()).unwrap_or((gx, gy));
        format!("translate({x} {y})")
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
                <rect x=left + 12.0 y=by width=bw height="22" rx="6" />
                <text x=left + 12.0 + bw / 2.0 y=by + 15.0 text-anchor="middle">"Assess"</text>
                <title>"Assess every host of this group"</title>
            </g>
            <g class="btn reach" on:click=move |ev| { ev.stop_propagation(); ask(t2.clone(), api::JobKind::Reach, count); }>
                <rect x=left + 24.0 + bw y=by width=bw height="22" rx="6" />
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
        <g class="group" class:picked=move || picks.with(|p| p.contains(&picked_as))
            data-group=group.name.clone() transform=transform on:click=toggle>
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
            <title>{move || if organize.get() {
                format!("{} — drag to move (Ctrl + click: add to the picked ones)", name_for_title)
            } else {
                format!("{} — click to {}", name_for_title, if collapsed_now { "unfold" } else { "fold" })
            }}</title>
        </g>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_names_compliance_only() {
        assert_eq!(map_state("reach_success"), "compliant");
        assert_eq!(map_state("reach_failed"), "not_compliant");
        assert_eq!(map_state("error"), "error");
        assert_eq!(tone(map_state("reach_failed")), tone("reach_failed"));
    }

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
        let map = layout(&hosts, Display::Areas, &BTreeSet::new(), &selectors());
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
    fn both_displays_show_the_same_groups() {
        let hosts = mixed_fleet();
        let groups = |display| {
            let map = layout(&hosts, display, &BTreeSet::new(), &selectors());
            let mut groups: Vec<(String, usize)> = map
                .groups
                .iter()
                .map(|g| (g.name.clone(), g.total))
                .collect();
            groups.sort();
            (groups, map.hosts.len(), map.placements("shared").count())
        };
        let graph = groups(Display::Graph);
        assert_eq!(graph, groups(Display::Areas));
        // every host in each assignment selecting it, e.g. git/base holds all
        assert!(graph.0.contains(&("git/base".to_string(), 121)));
        assert_eq!(graph.2, 3);
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

    /// `fleet()` with every even host proactive (some of each kind).
    fn mixed_fleet() -> BTreeMap<String, api::HostSummary> {
        let mut hosts = fleet();
        for i in (0..120).step_by(2) {
            hosts.get_mut(&format!("h{i}")).unwrap().agent = "regops".into();
        }
        hosts
    }

    #[test]
    fn proactive_left_passive_right() {
        let hosts = mixed_fleet();
        // graph: every host of a mixed group on its side of the gap
        let map = layout(&hosts, Display::Graph, &BTreeSet::new(), &selectors());
        let mut sides = (0, 0);
        for p in &map.hosts {
            let g = &map.groups[p.group];
            let active = !hosts[&p.host_id].agent.is_empty();
            let mixed = map
                .hosts
                .iter()
                .filter(|q| q.group == p.group)
                .any(|q| hosts[&q.host_id].agent.is_empty() == active);
            if !mixed {
                continue;
            }
            if active {
                assert!(p.x <= g.x - SPLIT, "{}", p.host_id);
                sides.0 += 1;
            } else {
                assert!(p.x >= g.x + SPLIT, "{}", p.host_id);
                sides.1 += 1;
            }
            // within the group's radius
            let d = ((p.x - g.x).powi(2) + (p.y - g.y).powi(2)).sqrt();
            assert!(d + DOT <= g.radius, "{}", p.host_id);
        }
        assert!(sides.0 > 0 && sides.1 > 0);
        for (i, a) in map.groups.iter().enumerate() {
            for b in &map.groups[i + 1..] {
                let d = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
                assert!(d >= a.radius + b.radius, "{} / {}", a.name, b.name);
            }
        }
        // areas: two columns either side of the line, off the card
        let map = layout(&hosts, Display::Areas, &BTreeSet::new(), &selectors());
        let web = map.groups.iter().find(|g| g.name == "git/web").unwrap();
        let area = web.area.unwrap();
        let line = area.x + area.split.expect("a mixed group is split");
        let mut seen = 0;
        for p in map.hosts.iter().filter(|p| (p.gx, p.gy) == (web.x, web.y)) {
            if hosts[&p.host_id].agent.is_empty() {
                assert!(p.x - DOT >= line, "{}", p.host_id);
            } else {
                assert!(p.x + DOT <= line, "{}", p.host_id);
            }
            assert!(p.x + DOT <= area.x + area.w && p.y + DOT <= area.y + area.h);
            let on_card = (p.x - web.x).abs() < CARD_W / 2.0 + DOT
                && (p.y - web.y).abs() < CARD_H / 2.0 + DOT;
            assert!(!on_card, "{}", p.host_id);
            seen += 1;
        }
        assert_eq!(seen, web.total);
        // a group of passive hosts only: not split
        let map = layout(&fleet(), Display::Areas, &BTreeSet::new(), &selectors());
        assert!(map.areas.iter().all(|a| a.split.is_none()));
    }

    #[test]
    fn areas_keep_hosts_in_name_order() {
        // states vary across the fleet: the cells must not follow them,
        // or every host would move when one changes state
        let hosts = fleet();
        let map = layout(&hosts, Display::Areas, &BTreeSet::new(), &selectors());
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
        let map = layout(&hosts, Display::Graph, &BTreeSet::new(), &selectors());
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
        let map = layout(&hosts, Display::Areas, &BTreeSet::new(), &selectors());
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
        let map = layout(&hosts, Display::Areas, &folded, &selectors());
        let web = map.groups.iter().find(|g| g.name == "git/web").unwrap();
        let area = web.area.unwrap();
        assert!(area.folded && area.w < CARD_W + 40.0 && web.total == 41);
        assert_eq!(map.placements("shared").count(), 2);
    }

    #[test]
    fn moved_groups_and_hosts_stay_where_put() {
        let hosts = fleet();
        for display in [Display::Graph, Display::Areas] {
            let auto = layout(&hosts, display, &BTreeSet::new(), &selectors());
            let (web_index, web) = auto.group_named("git/web").unwrap();
            let h3 = auto.placed_in("git/web", "h3").unwrap();
            let mut arranged = Arrangement::default();
            arranged
                .groups
                .insert("git/web".into(), (web.x + 1000.0, web.y - 500.0));
            arranged
                .hosts
                .entry("git/web".into())
                .or_default()
                .insert("h3".into(), (-400.0, 30.0));
            let mut map = auto.clone();
            arrange(&mut map, &arranged);
            let (_, moved) = map.group_named("git/web").unwrap();
            assert_eq!((moved.x, moved.y), (web.x + 1000.0, web.y - 500.0));
            // its other hosts follow it, the moved one sits at its offset
            for (i, (a, b)) in auto.hosts.iter().zip(&map.hosts).enumerate() {
                if i == h3 {
                    assert_eq!((b.x, b.y), (moved.x - 400.0, moved.y + 30.0));
                } else if a.group == web_index {
                    let (dx, dy) = (b.x - a.x, b.y - a.y);
                    assert!(
                        (dx - 1000.0).abs() < 1e-6 && (dy + 500.0).abs() < 1e-6,
                        "{}",
                        a.host_id
                    );
                    assert_eq!((b.gx, b.gy), (moved.x, moved.y));
                } else {
                    assert_eq!((a.x, a.y), (b.x, b.y));
                }
            }
            // areas hold their moved hosts; bounds hold everything
            if let Some(area) = moved.area {
                let p = &map.hosts[h3];
                assert!(p.x >= area.x && p.x <= area.x + area.w);
                assert_eq!(map.areas[web_index], area);
            }
            let (x0, y0, x1, y1) = map.bounds;
            assert!(
                map.hosts
                    .iter()
                    .all(|p| p.x >= x0 && p.x <= x1 && p.y >= y0 && p.y <= y1)
            );
            // a drag shows the same as its result
            let picks: Picks = [Grab::Group("git/web".into())].into();
            let dragged = auto.shifted(&picks, 1000.0, -500.0);
            let (_, g) = dragged.group_named("git/web").unwrap();
            assert_eq!((g.x, g.y), (moved.x, moved.y));
            // nothing moved: the automatic layout
            let mut same = auto.clone();
            arrange(&mut same, &Arrangement::default());
            assert!(same == auto);
        }
    }

    #[test]
    fn picks_move_together() {
        let hosts = fleet();
        let auto = layout(&hosts, Display::Areas, &BTreeSet::new(), &selectors());
        let host = |group: &str, host: &str| Grab::Host {
            group: group.into(),
            host: host.into(),
        };
        // a group, one of its hosts (moves with it, once) and a host of
        // another group
        let picks: Picks = [
            Grab::Group("git/web".into()),
            host("git/web", "h3"),
            host("git/db", "h1"),
        ]
        .into();
        let mut arranged = Arrangement::default();
        auto.moved(&picks, 50.0, 20.0, &mut arranged);
        let (_, web) = auto.group_named("git/web").unwrap();
        assert_eq!(arranged.groups["git/web"], (web.x + 50.0, web.y + 20.0));
        assert_eq!(arranged.hosts.len(), 1);
        let mut map = auto.clone();
        arrange(&mut map, &arranged);
        let dragged = auto.shifted(&picks, 50.0, 20.0);
        for (i, a) in auto.hosts.iter().enumerate() {
            let (b, c) = (&map.hosts[i], &dragged.hosts[i]);
            // the drag shows what is saved
            assert!(
                (b.x - c.x).abs() < 1e-6 && (b.y - c.y).abs() < 1e-6,
                "{}",
                a.host_id
            );
            let moves = auto.groups[a.group].name == "git/web"
                || (auto.groups[a.group].name == "git/db" && a.host_id == "h1");
            let d = (b.x - a.x, b.y - a.y);
            if moves {
                assert!(
                    (d.0 - 50.0).abs() < 1e-6 && (d.1 - 20.0).abs() < 1e-6,
                    "{}",
                    a.host_id
                );
            } else {
                assert_eq!(d, (0.0, 0.0), "{}", a.host_id);
            }
        }
    }

    #[test]
    fn a_selection_rectangle_picks_what_it_holds() {
        let hosts = fleet();
        let map = layout(&hosts, Display::Areas, &BTreeSet::new(), &selectors());
        // around a whole area: its group, not its hosts one by one
        let (_, web) = map.group_named("git/web").unwrap();
        let a = web.area.unwrap();
        let picks = map.in_box(a.x + a.w + 1.0, a.y + a.h + 1.0, a.x - 1.0, a.y - 1.0);
        assert!(picks.contains(&Grab::Group("git/web".into())));
        assert!(
            !picks
                .iter()
                .any(|p| matches!(p, Grab::Host { group, .. } if group == "git/web"))
        );
        // around one host: that host
        let i = map.placed_in("git/db", "h1").unwrap();
        let p = &map.hosts[i];
        let picks = map.in_box(p.x - 2.0, p.y - 2.0, p.x + 2.0, p.y + 2.0);
        assert_eq!(
            picks,
            [Grab::Host {
                group: "git/db".into(),
                host: "h1".into()
            }]
            .into()
        );
        // empty space: nothing
        assert!(map.in_box(-1e6, -1e6, -1e6 + 5.0, -1e6 + 5.0).is_empty());
    }

    #[test]
    fn folding_glides_from_the_layout_before() {
        let hosts = fleet();
        let open = layout(&hosts, Display::Areas, &BTreeSet::new(), &selectors());
        let folded: BTreeSet<String> = ["git/web".to_string()].into();
        let closed = layout(&hosts, Display::Areas, &folded, &selectors());
        let mut motion = Motion::of(&open);
        // start: where things were, the folding hosts included
        let first = motion.frame(&closed, 0.0);
        assert_eq!(first.hosts.len(), open.hosts.len());
        for p in &first.hosts {
            let g = &first.groups[p.group].name;
            let i = open.placed_in(g, &p.host_id).unwrap();
            assert_eq!(
                (p.x, p.y),
                (open.hosts[i].x, open.hosts[i].y),
                "{g} {}",
                p.host_id
            );
        }
        // end: the new layout, the folded hosts under their card
        let last = motion.frame(&closed, 1.0);
        let (web_index, web) = closed.group_named("git/web").unwrap();
        for p in &last.hosts[closed.hosts.len()..] {
            assert_eq!((p.group, p.x, p.y), (web_index, web.x, web.y));
        }
        assert!(last.hosts[..closed.hosts.len()] == closed.hosts[..]);
        // unfolding: hosts leave their card
        let mut motion = Motion::of(&closed);
        let first = motion.frame(&open, 0.0);
        let (_, folded_web) = closed.group_named("git/web").unwrap();
        let h3 = open.placed_in("git/web", "h3").unwrap();
        assert_eq!(
            (first.hosts[h3].x, first.hosts[h3].y),
            (folded_web.x, folded_web.y)
        );
    }

    #[test]
    fn graph_groups_without_overlap() {
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
        let map = layout(&hosts, Display::Graph, &BTreeSet::new(), &selectors());
        // every assignment of a host, as in the areas display
        assert_eq!(map.groups.len(), 5);
        assert_eq!(map.hosts.len(), 244);
        let web = map.groups.iter().find(|g| g.name == "git/web").unwrap();
        assert!(matches!(&web.target, Some(GroupTarget::Assignment(r, _)) if r == "git/web"));
        assert!(
            map.groups
                .iter()
                .any(|g| g.name == "(unassigned)" && g.target.is_none())
        );
        // h1 runs a job: counted in each of its groups (git/base, git/db)
        assert_eq!(map.running(&rings).iter().sum::<usize>(), 2);
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
        let folded: BTreeSet<String> = ["git/web".to_string(), "git/db".to_string()].into();
        let map = layout(&hosts, Display::Graph, &folded, &selectors());
        let web = map.groups.iter().find(|g| g.name == "git/web").unwrap();
        assert!(web.collapsed && web.total == 41);
        assert!(map.hosts.iter().all(|p| (p.gx, p.gy) != (web.x, web.y)));
        // folded hosts still count in their group (h1 is a db), but are not
        // placed there (only in git/base, not folded)
        let db_index = map.groups.iter().position(|g| g.name == "git/db").unwrap();
        assert_eq!(map.running(&rings)[db_index], 1);
        let db = &map.groups[db_index];
        assert!(map.placements("h1").all(|p| (p.gx, p.gy) != (db.x, db.y)));
        assert_eq!(map.placements("h1").count(), 1);
    }
}
