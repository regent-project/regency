//! Main page: live infrastructure map, a graph drawing each host once
//! (D31; the areas display was dropped). Hosts are clustered by signature: the exact set of
//! assignments selecting them (D29); a host's colour is the worst of its
//! states per assignment (D30; grey: unassigned, outdated or not checked).
//! Graph: each assignment is a node (a card with its hosts' states for it
//! and Assess / Reach of that assignment only, D30), placed near the
//! assignments it shares many hosts with ([`node_anchors`]); each cluster
//! lies between its assignments' nodes, its hosts on rings inside a soft
//! outline with a caption shown while the pointer is on it (one line per
//! assignment, then the counts; a visual helper, never a card; folded, a
//! small pie badge). Links ([`Links`]): grouped by default, one ribbon per
//! (cluster, assignment), plain and as wide as its hosts (their dots show
//! their colours), an arrow from the assignment to the hosts it constrains (faint
//! chevrons slide along it, on a canvas of their own: [`canvas::draw_flow`];
//! none with prefers-reduced-motion); hovering or clicking an assignment's card shows its ribbons'
//! statistics (hosts per state); a host's own links (in its colour for
//! each assignment) show on demand: hovering the host, hovering or
//! clicking a ribbon (Escape closes them), or Links: per host. Hosts stay in name order in
//! their cluster (they never move when a state changes). A host with a job
//! in progress (PENDING, STARTED or ABORT_REQUIRED) is drawn in the job
//! colour, without animation (redrawing rings every frame cost too much at
//! scale). Zoom with the wheel,
//! pan by dragging empty space (or with the middle button), click a
//! cluster's card to fold/unfold its hosts (positions glide for 200 ms,
//! [`Motion`]). Dragging a card, a host or a cluster (anywhere on its surface,
//! [`Layout::cluster_on`]) moves it (no mode to enter);
//! Ctrl / Cmd + click or a Shift + drag rectangle picks several, dragged
//! together; Escape drops the picks. Moves are kept by the browser
//! ([`Arrangement`]); Auto-arrange goes back to the automatic
//! layout.
//!
//! D32 ([`crate::graphedit`]): assignments and labels are edited on the
//! graph: an arrow drawn from a card's handle to hosts, or a selected
//! ribbon's ×, opens [`ArrowDialog`] (relabel the hosts or change the
//! selector, with a preview); + Assignment, Edit and Delete open the
//! assignment form in the right panel (the hosts its selector selects
//! ringed) or [`DeleteAssignmentDialog`]; a click inside a cluster opens
//! [`ClusterPanel`] (its shared labels).
//!
//! Shown as "Live view". The legend is a layer on top of the map; one panel,
//! on the right: groups and the selected host (the fleet counts and the
//! jobs-in-progress chart are in the masthead, `crate::feeds`; jobs in
//! progress and their Abort on the Jobs page). Clicking a host
//! selects it (`/?host=<id>`): its summary and expected state explorer show
//! in the right panel, "Details" opens the host page.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use js_sys::wasm_bindgen::JsCast;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};

use crate::canvas::{self, Frame, Palette, View};
use crate::catalog::{AssignmentEditor, AssignmentForm, CatalogData};
use crate::client::{api, authed, client, command_id, format_ms};
use crate::config::message_view;
use crate::expected::{
    AssignmentEntry, GroupTarget, HostStateExplorer, describe_selector, job_label,
};
use crate::feeds::Feeds;
use crate::graphedit::{ArrowDialog, ClusterPanel, DeleteAssignmentDialog, GraphEdit};
use crate::live::{HostLive, HostTip, running_jobs};
use crate::oneshot::OneShotDialog;
use crate::pages::{assignments_line, badge, host_state};
use crate::runs::{ActiveRun, KeptRun, RunPanel, RunStatus, watch_run_over};
use crate::{Session, Theme};

pub const DOT: f64 = 7.0;
/// Distance between two hosts on a ring and between two rings.
const RING_SPACING: f64 = 24.0;
/// D31 (graph display): radius of a folded cluster's badge (a pie of its
/// hosts' states; unfolded, a cluster is only an outline around its hosts).
const BADGE_R: f64 = 22.0;
/// Height of a line of a cluster's caption (D31).
const CAPTION_LINE: f64 = 14.0;
/// Milliseconds between two frames of the ribbons' flow (D31).
const FLOW_MS: f64 = 33.0;
const CARD_W: f64 = 208.0;
const CARD_H: f64 = 116.0;
/// Free space between two groups.
const GAP: f64 = 70.0;
/// Graph display, a group with proactive and passive hosts: each half is
/// pushed this far from the card's vertical axis (a gap twice as wide).
const SPLIT: f64 = 36.0;

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

/// D31 graph display: how host → assignment links are drawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Links {
    /// One plain ribbon per (cluster, assignment), as wide as its hosts;
    /// its statistics when its assignment is hovered or selected; a host's
    /// own links only on demand (hovering it, a ribbon hovered or opened).
    Grouped,
    /// Every host's links (bundled only when zoomed far out or folded).
    PerHost,
}

impl Links {
    const KEY: &str = "regency.map.links";

    fn saved() -> Self {
        match crate::client::storage().and_then(|s| s.get_item(Self::KEY).ok().flatten()) {
            Some(v) if v == "per-host" => Links::PerHost,
            _ => Links::Grouped,
        }
    }

    fn save(self) {
        if let Some(storage) = crate::client::storage() {
            let value = match self {
                Links::Grouped => "grouped",
                Links::PerHost => "per-host",
            };
            let _ = storage.set_item(Self::KEY, value);
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

/// The assignment a run's job checks, from its key
/// `run:{command}:{host}:{assignment}` (D30). `None` for one-shot runs
/// (`run:{command}:{host}`) and recurring jobs.
fn job_assignment(job: &api::JobSummary) -> Option<&str> {
    job.job_id
        .strip_prefix("run:")?
        .strip_prefix(job.origin.as_str())?
        .strip_prefix(':')?
        .strip_prefix(job.host_id.as_str())?
        .strip_prefix(':')
        .filter(|a| !a.is_empty())
}

/// (host, assignment) pairs whose newest run job is in progress: the links
/// between them are drawn in the job colour until it ends.
pub type Working = HashSet<(String, String)>;

fn working(jobs: &BTreeMap<String, api::JobSummary>) -> Working {
    let mut newest: HashMap<(&str, &str), &api::JobSummary> = HashMap::new();
    for job in jobs.values() {
        let Some(assignment) = job_assignment(job) else {
            continue;
        };
        let entry = newest
            .entry((job.host_id.as_str(), assignment))
            .or_insert(job);
        if job.created_at > entry.created_at {
            *entry = job;
        }
    }
    newest
        .into_iter()
        .filter(|(_, job)| Ring::of(&job.state).is_some())
        .map(|((host, assignment), _)| (host.to_string(), assignment.to_string()))
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
    /// D31 (graph display): its links, one per assignment selecting it:
    /// the index of the assignment's node in the layout, and the host's
    /// colour for that assignment (D30).
    pub links: Vec<(usize, Tone)>,
}

/// What a group of the map is (D31).
#[derive(Clone, PartialEq, Debug)]
enum Kind {
    /// An assignment's node (graph display): its card and Assess / Reach.
    Node,
    /// The hosts selected by exactly these assignments (their signature;
    /// empty: unassigned hosts), each host drawn once.
    Cluster { signature: Vec<String> },
}

#[derive(Clone, PartialEq)]
struct Group {
    /// Unique key (moves, fold): `a:<assignment>` or `c:<signature>`.
    name: String,
    /// What the card shows.
    label: String,
    kind: Kind,
    /// Its card's size.
    card: (f64, f64),
    /// What the Assess/Reach buttons select; `None` = no buttons.
    target: Option<GroupTarget>,
    /// Center of its card.
    x: f64,
    y: f64,
    radius: f64,
    total: usize,
    ok: usize,
    bad: usize,
    collapsed: bool,
}

#[derive(Clone, PartialEq, Default)]
pub struct Layout {
    groups: Vec<Group>,
    /// Hosts of the unfolded clusters, each once (D31).
    pub hosts: Vec<Placed>,
    /// Draw the links of hosts to their group (graph display).
    pub links: bool,
    /// D31 (graph display): the links of a cluster to one of its
    /// assignments, bundled (zoomed out, or the cluster folded).
    pub bundles: Vec<Bundle>,
    /// Every host (folded ones too): its groups, and its index in `hosts`
    /// in each of them.
    of_host: HashMap<String, Vec<(usize, Option<usize>)>>,
    /// min x, min y, max x, max y
    bounds: (f64, f64, f64, f64),
}

/// The links of a cluster's hosts to one assignment node (D31).
#[derive(Clone, PartialEq, Debug)]
pub struct Bundle {
    pub cluster: usize,
    pub node: usize,
    /// Hosts per colour, indexed by [`Tone::rank`].
    pub tones: [usize; 5],
}

impl Layout {
    /// Center of group `index` (a node's card, a cluster's center).
    pub fn center(&self, index: usize) -> Option<(f64, f64)> {
        self.groups.get(index).map(|g| (g.x, g.y))
    }

    /// D31: where the ribbon of `bundle` starts (on its cluster's outline)
    /// and ends (under its assignment's card).
    pub fn bundle_ends(&self, bundle: &Bundle) -> Option<((f64, f64), (f64, f64))> {
        let (c, n) = (
            self.groups.get(bundle.cluster)?,
            self.groups.get(bundle.node)?,
        );
        let (dx, dy) = (n.x - c.x, n.y - c.y);
        let len = (dx * dx + dy * dy).sqrt().max(1e-9);
        let start = (c.x + dx / len * c.radius, c.y + dy / len * c.radius);
        Some((start, canvas::link_end(start.0, start.1, n.x, n.y)))
    }

    /// D32: the middle of the ribbon (cluster, node names), if drawn.
    fn bundle_mid(&self, names: &(String, String)) -> Option<(f64, f64)> {
        let bundle = self.bundles.get(self.bundle_index(names)?)?;
        let ((x0, y0), (x1, y1)) = self.bundle_ends(bundle)?;
        Some(((x0 + x1) / 2.0, (y0 + y1) / 2.0))
    }

    /// D31: the cluster under (`x`, `y`): inside its outline (or badge),
    /// a few units around it, or on its caption above it (one line per
    /// assignment and the counts); the smallest one when they overlap.
    pub fn cluster_at(&self, x: f64, y: f64) -> Option<usize> {
        self.cluster_near(x, y, true)
    }

    /// D31: the cluster whose surface (inside its outline, or its badge
    /// when folded) holds (`x`, `y`): grabbed there to move it; the
    /// smallest one when they overlap.
    pub fn cluster_on(&self, x: f64, y: f64) -> Option<usize> {
        self.cluster_near(x, y, false)
    }

    fn cluster_near(&self, x: f64, y: f64, around: bool) -> Option<usize> {
        self.groups
            .iter()
            .enumerate()
            .filter_map(|(i, g)| {
                let Kind::Cluster { signature } = &g.kind else {
                    return None;
                };
                let (dx, dy) = (x - g.x, y - g.y);
                let margin = if around { 10.0 } else { 0.0 };
                let inside = (dx * dx + dy * dy).sqrt() <= g.radius + margin;
                let lines = signature.len().max(1) as f64 + 1.0;
                let caption = around
                    && dy <= -g.radius
                    && dy >= -g.radius - 12.0 - lines * CAPTION_LINE
                    && dx.abs() <= g.radius.max(130.0);
                (inside || caption).then_some((i, g.radius))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    /// D31: the ribbon nearest to (`x`, `y`) among those `drawn`, within
    /// its half width plus a few pixels (`k`: map units per pixel).
    pub fn bundle_at(
        &self,
        x: f64,
        y: f64,
        k: f64,
        drawn: impl Fn(&Bundle) -> bool,
    ) -> Option<usize> {
        self.bundles
            .iter()
            .enumerate()
            .filter(|(_, b)| drawn(b))
            .filter_map(|(i, b)| {
                let ((ax, ay), (bx, by)) = self.bundle_ends(b)?;
                let (vx, vy) = (bx - ax, by - ay);
                let len2 = (vx * vx + vy * vy).max(1e-9);
                let t = (((x - ax) * vx + (y - ay) * vy) / len2).clamp(0.0, 1.0);
                let d = ((x - ax - t * vx).powi(2) + (y - ay - t * vy).powi(2)).sqrt();
                let reach = (ribbon_pixels(b.tones.iter().sum()) / 2.0 + 4.0) * k;
                (d <= reach).then_some((i, d))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    /// The names of the cluster and the node of bundle `index`.
    fn bundle_names(&self, index: usize) -> Option<(String, String)> {
        let b = self.bundles.get(index)?;
        Some((
            self.groups.get(b.cluster)?.name.clone(),
            self.groups.get(b.node)?.name.clone(),
        ))
    }

    /// The bundle of that cluster and node (names), if any.
    pub fn bundle_index(&self, names: &(String, String)) -> Option<usize> {
        self.bundles.iter().position(|b| {
            self.groups
                .get(b.cluster)
                .is_some_and(|g| g.name == names.0)
                && self.groups.get(b.node).is_some_and(|g| g.name == names.1)
        })
    }

    /// A cluster folded (its hosts not drawn).
    pub fn folded(&self, index: usize) -> bool {
        self.groups.get(index).is_some_and(|g| g.collapsed)
    }

    /// The ribbons (indices in `bundles`) and the per-host links (index in
    /// `hosts`, node) with a job in progress, from [`working`].
    pub fn working(&self, working: &Working) -> (Vec<usize>, HashSet<(usize, usize)>) {
        let mut ribbons = Vec::new();
        let mut links = HashSet::new();
        for (host, assignment) in working {
            let Some(node) = self.group_index(&node_key(assignment)) else {
                continue;
            };
            for (group, index) in self.of_host.get(host).into_iter().flatten() {
                if !matches!(self.groups[*group].kind, Kind::Cluster { .. }) {
                    continue;
                }
                if let Some(b) = self
                    .bundles
                    .iter()
                    .position(|b| b.cluster == *group && b.node == node)
                    && !ribbons.contains(&b)
                {
                    ribbons.push(b);
                }
                if let Some(index) = index {
                    links.insert((*index, node));
                }
            }
        }
        (ribbons, links)
    }

    /// Index of the node of assignment `name` (its group key).
    pub fn group_index(&self, name: &str) -> Option<usize> {
        self.groups.iter().position(|g| g.name == name)
    }

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

    #[allow(clippy::too_many_arguments)]
    fn place(
        &mut self,
        group: usize,
        host: &api::HostSummary,
        x: f64,
        y: f64,
        gx: f64,
        gy: f64,
        links: Vec<(usize, Tone)>,
    ) {
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
            links,
        });
    }

    fn fold(&mut self, group: usize, host: &api::HostSummary) {
        self.of_host
            .entry(host.host_id.clone())
            .or_default()
            .push((group, None));
    }
}

/// Positions of `n` hosts on concentric rings around a card, the first at
/// `first`, and the radius of the outer ring.
fn ring_offsets(n: usize, first: f64) -> (Vec<(f64, f64)>, f64) {
    let mut out = Vec::with_capacity(n);
    let mut radius = first;
    let mut outer = first;
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

/// Width in pixels of a bundle's ribbon of `hosts` links (graph display).
pub fn ribbon_pixels(hosts: usize) -> f64 {
    (1.5 + (hosts as f64).sqrt() * 0.6).min(14.0)
}

/// A proactive host (an agent reports for it, D28), else passive (reached
/// by runners).
fn is_active(host: &api::HostSummary) -> bool {
    !host.agent.is_empty()
}

/// Positions of `n` hosts on concentric half rings right of the card
/// (mirrored when `left`), pushed `SPLIT` away from its vertical axis, and
/// the radius of the outer ring.
fn half_ring_offsets(n: usize, left: bool, first: f64) -> (Vec<(f64, f64)>, f64) {
    let mut out = Vec::with_capacity(n);
    let mut radius = first;
    let mut outer = first;
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
        // nothing in the middle (D31: no chip): the first host there
        if hosts.is_empty() {
            return (Vec::new(), BADGE_R);
        }
        let (mut offsets, outer) = ring_offsets(hosts.len() - 1, RING_SPACING);
        offsets.insert(0, (0.0, 0.0));
        let outer = if hosts.len() == 1 { 0.0 } else { outer };
        return (offsets, outer + DOT + 12.0);
    }
    let (left, l_outer) = half_ring_offsets(active, true, RING_SPACING);
    let (right, r_outer) = half_ring_offsets(hosts.len() - active, false, RING_SPACING);
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
    (offsets, l_outer.max(r_outer) + SPLIT + DOT + 12.0)
}

/// D30: a host's colour for one of its assignments.
fn link_tone(host: &api::HostSummary, reference: &str) -> Tone {
    match host
        .assignment_states
        .iter()
        .find(|s| s.assignment == reference)
    {
        Some(status) => tone(crate::pages::link_state(host, status)),
        // a backend from before D30: the host's one state
        None => tone(host_state(host)),
    }
}

/// D31: the map's groups before placement: a node per assignment, and the
/// hosts by signature (the exact set of assignments selecting them).
struct Groups<'a> {
    /// reference -> (its target, hosts it selects, ok, bad)
    nodes: BTreeMap<String, (Option<GroupTarget>, usize, usize, usize)>,
    /// signature -> its hosts, in name order
    clusters: BTreeMap<Vec<String>, Vec<&'a api::HostSummary>>,
}

fn node_key(reference: &str) -> String {
    format!("a:{reference}")
}

fn cluster_key(signature: &[String]) -> String {
    format!("c:{}", signature.join("+"))
}

fn cluster_label(signature: &[String]) -> String {
    if signature.is_empty() {
        "(unassigned)".to_string()
    } else {
        signature.join(" + ")
    }
}

/// `assignments`: selector of each assignment (D23), for its Assess /
/// Reach; every assignment gets a node, selecting hosts or not.
fn groups<'a>(
    hosts: &'a BTreeMap<String, api::HostSummary>,
    assignments: &BTreeMap<String, api::HostSelector>,
) -> Groups<'a> {
    let target = |reference: &str| {
        assignments
            .get(reference)
            .map(|selector| GroupTarget::Assignment(reference.to_string(), selector.clone()))
    };
    let mut nodes: BTreeMap<String, (Option<GroupTarget>, usize, usize, usize)> = assignments
        .keys()
        .map(|r| (r.clone(), (target(r), 0, 0, 0)))
        .collect();
    let mut clusters: BTreeMap<Vec<String>, Vec<&api::HostSummary>> = BTreeMap::new();
    for host in hosts.values() {
        let mut signature = host.assignments.clone();
        signature.sort();
        signature.dedup();
        for reference in &signature {
            let node = nodes
                .entry(reference.clone())
                .or_insert_with(|| (target(reference), 0, 0, 0));
            node.1 += 1;
            match link_tone(host, reference) {
                Tone::Ok => node.2 += 1,
                Tone::Bad => node.3 += 1,
                _ => {}
            }
        }
        clusters.entry(signature).or_default().push(host);
    }
    Groups { nodes, clusters }
}

fn counts(hosts: &[&api::HostSummary]) -> (usize, usize, usize) {
    let ok = hosts.iter().filter(|h| is_ok(host_state(h))).count();
    let bad = hosts.iter().filter(|h| is_bad(host_state(h))).count();
    (hosts.len(), ok, bad)
}

fn layout(
    hosts: &BTreeMap<String, api::HostSummary>,
    collapsed: &BTreeSet<String>,
    assignments: &BTreeMap<String, api::HostSelector>,
) -> Layout {
    // each host drawn once, in name order in its cluster (a host changing
    // state stays where it is; D31)
    let mut layout = graph_layout(&groups(hosts, assignments), collapsed);
    if layout.groups.is_empty() {
        layout.bounds = (-200.0, -200.0, 200.0, 200.0);
    }
    layout
}

/// What the user moved on the map, kept by the browser: a moved group's
/// card center, and a moved host's offset from
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
    /// v2: groups are assignment nodes and clusters (D31); the graph
    /// display's key (the only display left).
    const KEY: &str = "regency.map.arrangement.v2.graph";

    fn saved() -> Self {
        crate::client::storage()
            .and_then(|s| s.get_item(Self::KEY).ok().flatten())
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let Some(storage) = crate::client::storage() else {
            return;
        };
        if self.is_empty() {
            let _ = storage.remove_item(Self::KEY);
        } else if let Ok(json) = serde_json::to_string(self) {
            let _ = storage.set_item(Self::KEY, &json);
        }
    }

    fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.hosts.is_empty()
    }
}

/// Move the groups and hosts of `layout` where the user put them.
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
        }
    }
    for p in &mut layout.hosts {
        let group = &layout.groups[p.group];
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
    }
    layout.bounds = Layout::empty().bounds;
    for i in 0..layout.groups.len() {
        let g = &layout.groups[i];
        let (x0, y0, x1, y1) = (
            g.x - g.radius,
            g.y - g.radius,
            g.x + g.radius,
            g.y + g.radius,
        );
        layout.grow(x0, y0, x1, y1);
    }
    for i in 0..layout.hosts.len() {
        let (x, y) = (layout.hosts[i].x, layout.hosts[i].y);
        layout.grow(x - DOT, y - DOT, x + DOT, y + DOT);
    }
}

/// Something picked on the map: a group (by name) or a host in
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

    /// What a selection rectangle holds (map units): the assignments whose
    /// card center is inside, the clusters whose whole outline is inside
    /// (a host lies at a cluster's center), and the hosts inside but not in
    /// those groups.
    fn in_box(&self, x0: f64, y0: f64, x1: f64, y1: f64) -> Picks {
        let (x0, x1) = (x0.min(x1), x0.max(x1));
        let (y0, y1) = (y0.min(y1), y0.max(y1));
        let inside = |x: f64, y: f64| x >= x0 && x <= x1 && y >= y0 && y <= y1;
        let groups: Vec<usize> = (0..self.groups.len())
            .filter(|i| {
                let g = &self.groups[*i];
                match g.kind {
                    Kind::Node => inside(g.x, g.y),
                    Kind::Cluster { .. } => {
                        inside(g.x - g.radius, g.y - g.radius)
                            && inside(g.x + g.radius, g.y + g.radius)
                    }
                }
            })
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
    groups: HashMap<String, (f64, f64)>,
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
                .map(|g| (g.name.clone(), (g.x, g.y)))
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
            if let Some((x, y)) = self.groups.get(&g.name) {
                g.x = lerp(*x, g.x);
                g.y = lerp(*y, g.y);
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
                .or_else(|| self.groups.get(&g.name).copied())
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
        out
    }
}

/// A cluster before placement (graph display): its signature, hosts,
/// whether folded, its hosts' offsets and its radius.
type GraphCluster<'a> = (
    &'a Vec<String>,
    &'a Vec<&'a api::HostSummary>,
    bool,
    Vec<(f64, f64)>,
    f64,
);

/// D31 graph display: a node (card) per assignment, on a circle; each
/// cluster of hosts starts between its assignments' nodes (beyond its
/// node for a single assignment, below everything when unassigned), then
/// everything overlapping is pushed apart. Deterministic: the same fleet
/// gives the same map.
fn graph_layout(groups: &Groups, collapsed: &BTreeSet<String>) -> Layout {
    let references: Vec<&String> = groups.nodes.keys().collect();
    let node_r = card_radius();
    // the clusters' hosts around them, unless folded
    let clusters: Vec<GraphCluster> = groups
        .clusters
        .iter()
        .map(|(signature, hosts)| {
            let folded = collapsed.contains(&cluster_key(signature));
            let (offsets, radius) = if folded {
                (Vec::new(), BADGE_R)
            } else {
                graph_offsets(hosts)
            };
            (signature, hosts, folded, offsets, radius.max(BADGE_R))
        })
        .collect();
    let area: f64 = references.len() as f64 * (node_r + GAP).powi(2) * std::f64::consts::PI
        + clusters
            .iter()
            .map(|c| (c.4 + GAP).powi(2) * std::f64::consts::PI)
            .sum::<f64>();
    let n = references.len().max(1) as f64;
    let ring = ((area / std::f64::consts::PI).sqrt() * 0.8)
        .max(n * (2.0 * node_r + GAP) / std::f64::consts::TAU);
    let anchors = node_anchors(&references, &clusters, node_r, ring);
    // the nodes' extent (clusters without assignment go below it)
    let ring = anchors
        .iter()
        .map(|(x, y)| (x * x + y * y).sqrt())
        .fold(0.0, f64::max);
    let index_of: HashMap<&str, usize> = references
        .iter()
        .enumerate()
        .map(|(i, r)| (r.as_str(), i))
        .collect();
    // discs: (x, y, radius, target x, target y, mass); nodes first
    let mut discs: Vec<[f64; 6]> = anchors
        .iter()
        .map(|&(x, y)| [x, y, node_r, x, y, 4.0])
        .collect();
    for (k, (signature, _, _, _, radius)) in clusters.iter().enumerate() {
        let nodes: Vec<(f64, f64)> = signature
            .iter()
            .filter_map(|r| index_of.get(r.as_str()).map(|i| anchors[*i]))
            .collect();
        let (tx, ty) = match nodes.len() {
            0 => (0.0, ring + node_r + radius + 2.0 * GAP),
            // outwards (from the nodes' center), else around the node
            1 => {
                let (x, y) = nodes[0];
                let len = (x * x + y * y).sqrt();
                let out = node_r + radius + GAP;
                let (ux, uy) = if len < 1.0 {
                    let a = k as f64 * 2.399;
                    (a.cos(), a.sin())
                } else {
                    (x / len, y / len)
                };
                (x + ux * out, y + uy * out)
            }
            m => (
                nodes.iter().map(|p| p.0).sum::<f64>() / m as f64,
                nodes.iter().map(|p| p.1).sum::<f64>() / m as f64,
            ),
        };
        // a little apart from clusters with the same target
        let a = k as f64 * 2.399;
        discs.push([
            tx + a.cos(),
            ty + a.sin(),
            *radius,
            tx,
            ty,
            1.0 + radius / 200.0,
        ]);
    }
    for iteration in 0..500 {
        let pull = if iteration < 350 { 0.04 } else { 0.0 };
        for d in &mut discs {
            d[0] += (d[3] - d[0]) * pull;
            d[1] += (d[4] - d[1]) * pull;
        }
        for i in 0..discs.len() {
            for j in i + 1..discs.len() {
                let (dx, dy) = (discs[j][0] - discs[i][0], discs[j][1] - discs[i][1]);
                let d = (dx * dx + dy * dy).sqrt();
                let min = discs[i][2] + discs[j][2] + GAP;
                if d >= min {
                    continue;
                }
                let (ux, uy) = if d < 1e-6 {
                    let a = (i * 7 + j * 13) as f64;
                    (a.cos(), a.sin())
                } else {
                    (dx / d, dy / d)
                };
                let (mi, mj) = (discs[i][5], discs[j][5]);
                let push = min - d;
                discs[i][0] -= ux * push * mj / (mi + mj);
                discs[i][1] -= uy * push * mj / (mi + mj);
                discs[j][0] += ux * push * mi / (mi + mj);
                discs[j][1] += uy * push * mi / (mi + mj);
            }
        }
    }

    let mut layout = Layout {
        links: true,
        ..Layout::empty()
    };
    for (i, reference) in references.iter().enumerate() {
        let [x, y, radius, ..] = discs[i];
        let (target, total, ok, bad) = groups.nodes[*reference].clone();
        layout.grow(x - radius, y - radius, x + radius, y + radius);
        layout.groups.push(Group {
            name: node_key(reference),
            label: (*reference).clone(),
            kind: Kind::Node,
            card: (CARD_W, CARD_H),
            target,
            x,
            y,
            radius,
            total,
            ok,
            bad,
            collapsed: false,
        });
    }
    for (k, (signature, hosts, folded, offsets, radius)) in clusters.into_iter().enumerate() {
        let [cx, cy, ..] = discs[references.len() + k];
        layout.grow(cx - radius, cy - radius, cx + radius, cy + radius);
        let index = layout.groups.len();
        let (total, ok, bad) = counts(hosts);
        let mut tones = vec![[0usize; 5]; signature.len()];
        for host in hosts.iter() {
            for (slot, reference) in tones.iter_mut().zip(signature.iter()) {
                slot[link_tone(host, reference).rank() as usize] += 1;
            }
        }
        for (reference, tones) in signature.iter().zip(tones) {
            if let Some(&node) = index_of.get(reference.as_str()) {
                layout.bundles.push(Bundle {
                    cluster: index,
                    node,
                    tones,
                });
            }
        }
        for host in &hosts[offsets.len()..] {
            layout.fold(index, host);
        }
        for (host, (dx, dy)) in hosts.iter().zip(offsets) {
            let links = signature
                .iter()
                .filter_map(|r| Some((*index_of.get(r.as_str())?, link_tone(host, r))))
                .collect();
            layout.place(index, host, cx + dx, cy + dy, cx, cy, links);
        }
        layout.groups.push(Group {
            name: cluster_key(signature),
            label: cluster_label(signature),
            kind: Kind::Cluster {
                signature: signature.clone(),
            },
            card: (2.0 * radius, 2.0 * radius),
            target: None,
            x: cx,
            y: cy,
            radius,
            total,
            ok,
            bad,
            collapsed: folded,
        });
    }
    layout
}

/// D31: where the assignment nodes go, centered on (0, 0). Assignments
/// sharing many hosts lie close together (their shared clusters between
/// them), unrelated ones apart: each pair gets a target distance from its
/// overlap (shared hosts / hosts of the smaller one: 1 when one holds all
/// the other's hosts), reached by stress majorization (stochastic
/// gradient descent, Zheng et al. 2018), from a ring ordered so that each
/// node follows the one it shares most with. Deterministic.
fn node_anchors(
    references: &[&String],
    clusters: &[GraphCluster],
    node_r: f64,
    ring: f64,
) -> Vec<(f64, f64)> {
    let n = references.len();
    if n < 2 {
        return vec![(0.0, 0.0); n];
    }
    let index: HashMap<&str, usize> = references
        .iter()
        .enumerate()
        .map(|(i, r)| (r.as_str(), i))
        .collect();
    let mut size = vec![0usize; n];
    let mut shared = vec![vec![0usize; n]; n];
    // largest cluster between two nodes, around one node
    let mut between = vec![vec![0.0f64; n]; n];
    let mut own = vec![0.0f64; n];
    for (signature, hosts, _, _, radius) in clusters {
        let ids: Vec<usize> = signature
            .iter()
            .filter_map(|r| index.get(r.as_str()).copied())
            .collect();
        for &i in &ids {
            size[i] += hosts.len();
            own[i] = own[i].max(*radius);
            for &j in &ids {
                if i != j {
                    shared[i][j] += hosts.len();
                    between[i][j] = between[i][j].max(*radius);
                }
            }
        }
    }
    let overlap = |i: usize, j: usize| match size[i].min(size[j]) {
        0 => 0.0,
        m => shared[i][j] as f64 / m as f64,
    };
    // ring order: from the largest node, each next one the unplaced node
    // sharing most with the last (ties: name order)
    let mut order = vec![
        (0..n)
            .max_by_key(|i| (size[*i], std::cmp::Reverse(*i)))
            .unwrap_or(0),
    ];
    let mut left: Vec<usize> = (0..n).filter(|i| *i != order[0]).collect();
    while !left.is_empty() {
        let last = *order.last().unwrap_or(&0);
        let (k, _) = left
            .iter()
            .enumerate()
            .max_by(|a, b| {
                overlap(last, *a.1)
                    .total_cmp(&overlap(last, *b.1))
                    .then(b.1.cmp(a.1))
            })
            .unwrap_or((0, &0));
        order.push(left.remove(k));
    }
    let mut pos = vec![(0.0, 0.0); n];
    for (k, &i) in order.iter().enumerate() {
        let a = -std::f64::consts::FRAC_PI_2 + k as f64 / n as f64 * std::f64::consts::TAU;
        pos[i] = (ring * a.cos(), ring * a.sin());
    }
    // target distances: sharing everything = side by side with their
    // clusters between them; nothing in common = well apart
    let mut pairs = Vec::new();
    for i in 0..n {
        for j in i + 1..n {
            let close = 2.0 * node_r + GAP + 2.0 * between[i][j];
            let apart = ((2.0 * node_r + 2.0 * GAP + own[i] + own[j]) * 1.6).max(close);
            pairs.push((i, j, close + (apart - close) * (1.0 - overlap(i, j))));
        }
    }
    let (l_min, l_max) = pairs
        .iter()
        .fold((f64::MAX, 0.0f64), |(a, b), p| (a.min(p.2), b.max(p.2)));
    let (eta_max, eta_min) = (l_max * l_max, 0.01 * l_min * l_min);
    const EPOCHS: usize = 60;
    let decay = (eta_max / eta_min).ln() / (EPOCHS - 1) as f64;
    for epoch in 0..EPOCHS {
        let eta = eta_max * (-decay * epoch as f64).exp();
        let forward = epoch % 2 == 0;
        for k in 0..pairs.len() {
            let (i, j, l) = pairs[if forward { k } else { pairs.len() - 1 - k }];
            let mu = (eta / (l * l)).min(1.0);
            let (dx, dy) = (pos[i].0 - pos[j].0, pos[i].1 - pos[j].1);
            let d = (dx * dx + dy * dy).sqrt();
            let (ux, uy) = if d < 1e-6 {
                let a = (i * 7 + j * 13) as f64;
                (a.cos(), a.sin())
            } else {
                (dx / d, dy / d)
            };
            let r = mu * (d - l) / 2.0;
            pos[i].0 -= r * ux;
            pos[i].1 -= r * uy;
            pos[j].0 += r * ux;
            pos[j].1 += r * uy;
        }
    }
    let (mx, my) = (
        pos.iter().map(|p| p.0).sum::<f64>() / n as f64,
        pos.iter().map(|p| p.1).sum::<f64>() / n as f64,
    );
    pos.iter().map(|(x, y)| (x - mx, y - my)).collect()
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

/// D32: what the right panel edits.
#[derive(Clone, PartialEq)]
enum Editing {
    /// The assignment of the form (a new one or from the catalog).
    Assignment,
    /// A cluster's labels (its signature).
    Cluster(Vec<String>),
}

/// D32: where an arrow drawn from an assignment's handle would land.
#[derive(Clone, PartialEq, Debug)]
enum DropTarget {
    Cluster(String, Vec<String>),
    Host(String),
}

/// D32: an arrow being drawn from an assignment's handle.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Connecting {
    /// The assignment (`source/name`) and its node's name.
    reference: String,
    node: String,
    /// The pointer (map units).
    x: f64,
    y: f64,
    target: Option<DropTarget>,
}

/// The hosts selected by exactly the assignments of `signature`.
fn signature_hosts(
    hosts: &BTreeMap<String, api::HostSummary>,
    signature: &[String],
) -> Vec<String> {
    hosts
        .values()
        .filter(|h| {
            let mut s = h.assignments.clone();
            s.sort();
            s.dedup();
            s == signature
        })
        .map(|h| h.host_id.clone())
        .collect()
}

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
    // D32: the catalog, to edit assignments and hosts' labels on the graph
    let catalog = CatalogData::new();
    catalog.load(session);
    let error = catalog.error;
    let assignments = Memo::new(move |_| {
        catalog.assignments.with(|list| {
            let mut entries: Vec<AssignmentEntry> = list
                .iter()
                .filter_map(|e| {
                    AssignmentEntry::from_catalog(&api::CatalogEntry {
                        doc_id: e.doc_id.clone(),
                        item_json: e.item.to_string(),
                    })
                })
                .collect();
            entries.sort_by(|a, b| a.reference.cmp(&b.reference));
            entries
        })
    });
    // D32: the assignments regency may edit (not from Git)
    let editable = Memo::new(move |_| {
        catalog.assignments.with(|list| {
            list.iter()
                .filter(|e| {
                    catalog
                        .sources
                        .is_editable(&crate::config::str_of(&e.item, "source_id"))
                })
                .map(crate::catalog::reference_of)
                .collect::<BTreeSet<String>>()
        })
    });
    // D32: an arrow being drawn, the dialogs, and what the right panel edits
    let connecting = RwSignal::new(None::<Connecting>);
    let arrow = RwSignal::new(None::<(String, Vec<String>, bool)>);
    let deleting = RwSignal::new(None::<(String, usize)>);
    let editing = RwSignal::new(None::<Editing>);
    let form = RwSignal::new(None::<AssignmentForm>);
    let catalog_message = RwSignal::new(None::<(bool, String)>);
    // the form closed (saved or cancelled): so is its panel
    Effect::new(move |_| {
        if form.with(Option::is_none) && editing.with_untracked(|e| *e == Some(Editing::Assignment))
        {
            editing.set(None);
        }
    });
    // D32: the hosts the assignment being edited would select, ringed on
    // the graph
    let host_sources = Memo::new(move |_| {
        catalog.hostdefs.with(|defs| {
            let mut map = BTreeMap::<String, Vec<String>>::new();
            for e in defs {
                map.entry(crate::config::str_of(&e.item, "host_id"))
                    .or_default()
                    .push(crate::config::str_of(&e.item, "source_id"));
            }
            map
        })
    });
    let previewed = Memo::new(move |_| {
        if editing.with(|e| *e != Some(Editing::Assignment)) {
            return BTreeSet::new();
        }
        let Some(Ok(selector)) = form.with(|f| {
            f.as_ref()
                .map(|f| crate::labels::LabelSelector::parse(&f.selector))
        }) else {
            return BTreeSet::new();
        };
        host_sources.with(|sources| {
            hosts.with(|hosts| {
                hosts
                    .values()
                    .filter(|h| {
                        let mut from: Vec<&str> = sources
                            .get(&h.host_id)
                            .map(|v| v.iter().map(String::as_str).collect())
                            .unwrap_or_default();
                        if !h.agent.is_empty() {
                            from.push(&h.agent);
                        }
                        let own: BTreeMap<String, String> = h.labels.clone().into_iter().collect();
                        selector.matches(&crate::labels::HostLabels::new(&h.host_id, from, &own))
                    })
                    .map(|h| h.host_id.clone())
                    .collect()
            })
        })
    });
    // reference -> host selector, for the assignment groups' Assess / Reach
    let selectors = Memo::new(move |_| {
        assignments.with(|list| {
            list.iter()
                .map(|a| (a.reference.clone(), a.selector.clone()))
                .collect::<BTreeMap<_, _>>()
        })
    });

    // hosts are grouped by assignment (D23: colours mean "against the
    // assignments"), hosts without one in "(unassigned)"
    let collapsed = RwSignal::new(BTreeSet::<String>::new());
    let rings = Memo::new(move |_| jobs.with(rings));
    // (host, assignment) with a run job in progress: their links in the job colour
    let working = Memo::new(move |_| jobs.with(working));
    // where the user moved groups and hosts
    let arrangement = RwSignal::new(Arrangement::saved());
    let map = Memo::new(move |_| {
        hosts.with(|hosts| {
            collapsed.with(|collapsed| {
                selectors.with(|selectors| {
                    let mut map = layout(hosts, collapsed, selectors);
                    arrangement.with(|a| arrange(&mut map, a));
                    map
                })
            })
        })
    });
    // cards and hosts are moved by dragging them (no mode to enter); several
    // are picked with Ctrl / Cmd + click or a Shift + drag rectangle and
    // dragged together; Escape or a click on empty space drops the picks
    // D31: how links are drawn; the ribbon under the pointer and the ones
    // opened by a click (cluster, node names): their hosts' links fan out
    let links_mode = RwSignal::new(Links::saved());
    let hovered_bundle = RwSignal::new(None::<(String, String)>);
    let opened = RwSignal::new(BTreeSet::<(String, String)>::new());
    // D31: assignment nodes selected by a click on their card (their
    // ribbons' statistics stay shown); Escape or empty space drops them
    let chosen = RwSignal::new(BTreeSet::<String>::new());
    let picks = RwSignal::new(Picks::new());
    // a card or host pressed: it becomes a drag once the pointer moves
    let pressed = StoredValue::new(None::<Grab>);
    let escape = window_event_listener(leptos::ev::keydown, move |ev| {
        // D32: Delete removes the selected arrow (one opened ribbon), else
        // the selected assignment (not while typing, nor under a dialog)
        let typing = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|e| {
                e.closest("input, textarea, select, .overlay")
                    .ok()
                    .flatten()
                    .is_some()
            });
        if matches!(ev.key().as_str(), "Delete" | "Backspace") && !typing {
            let ribbon = opened
                .with_untracked(|o| (o.len() == 1).then(|| o.iter().next().cloned()).flatten());
            if let Some((cluster, node)) = ribbon {
                let reference = node.strip_prefix("a:").unwrap_or(&node).to_string();
                let signature =
                    map.with_untracked(|m| match m.group_named(&cluster).map(|(_, g)| &g.kind) {
                        Some(Kind::Cluster { signature }) => Some(signature.clone()),
                        _ => None,
                    });
                if let Some(signature) = signature
                    && editable.with_untracked(|e| e.contains(&reference))
                {
                    let targets = hosts.with_untracked(|h| signature_hosts(h, &signature));
                    arrow.set(Some((reference, targets, false)));
                }
                return;
            }
            let node = chosen
                .with_untracked(|c| (c.len() == 1).then(|| c.iter().next().cloned()).flatten());
            if let Some(node) = node {
                let reference = node.strip_prefix("a:").unwrap_or(&node).to_string();
                if editable.with_untracked(|e| e.contains(&reference)) {
                    let count =
                        map.with_untracked(|m| m.group_named(&node).map_or(0, |(_, g)| g.total));
                    deleting.set(Some((reference, count)));
                }
            }
            return;
        }
        if ev.key() == "Escape" {
            connecting.set(None);
            if picks.with_untracked(|p| !p.is_empty()) {
                picks.set(Picks::new());
            }
            if opened.with_untracked(|o| !o.is_empty()) {
                opened.set(BTreeSet::new());
            }
            if chosen.with_untracked(|c| !c.is_empty()) {
                chosen.set(BTreeSet::new());
            }
        }
    });
    on_cleanup(move || escape.remove());
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
    // D31: the ribbon under the pointer, if drawn (cluster, node names)
    let bundle_under = move |ev: &web_sys::MouseEvent| {
        let (w, h, left, top) = metrics(ev.current_target())?;
        let view = untrack(view);
        let (x, y) = view.map_point(
            ev.client_x() as f64 - left,
            ev.client_y() as f64 - top,
            w,
            h,
        );
        let k = view.units_per_pixel(w, h);
        let per_host = links_mode.get_untracked() == Links::PerHost;
        let tiny = DOT / k < 3.0;
        map.with_untracked(|m| {
            if !m.links {
                return None;
            }
            let i = m.bundle_at(x, y, k, |b| !per_host || tiny || m.folded(b.cluster))?;
            m.bundle_names(i)
        })
    };
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
    // D31: hovered assignment node (its ribbons' statistics shown)
    let hovered_node = RwSignal::new(None::<String>);
    // D31: hovered cluster (name, signature): its assignments' cards stand out
    let hovered_cluster = RwSignal::new(None::<(String, Vec<String>)>);
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
    // D31: the flow along the ribbons, animated on its own canvas
    let flow_ref = NodeRef::<leptos::html::Canvas>::new();
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
            hovered_node.track();
            chosen.track();
            hovered_bundle.track();
            opened.track();
            links_mode.track();
            selected.track();
            rings.track();
            working.track();
            flashes.track();
            theme.track();
            grab.track();
            picks.track();
            previewed.track();
            dirty.store(true, Ordering::Relaxed);
        }
    });
    {
        // colours of the current theme, read again when it changes
        let mut palette = Palette::read();
        let mut palette_theme = theme.get_untracked();
        // the flow: ~30 frames a second, none when the user asks for
        // reduced motion
        let mut last_flow = f64::MIN;
        let still = web_sys::window()
            .and_then(|w| {
                w.match_media("(prefers-reduced-motion: reduce)")
                    .ok()
                    .flatten()
            })
            .is_some_and(|m| m.matches());
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
            let flow_canvas = flow_ref.get_untracked().filter(|_| !still);
            let mut flow_resized = false;
            if let Some(c) = &flow_canvas
                && (c.width() != bw || c.height() != bh)
            {
                c.set_width(bw);
                c.set_height(bh);
                flow_resized = true;
            }
            let redraw = dirty.swap(false, Ordering::Relaxed) || resized;
            let flow_due =
                flow_canvas.is_some() && (redraw || flow_resized || now - last_flow >= FLOW_MS);
            if !redraw && !flow_due {
                return;
            }
            if flow_due {
                last_flow = now;
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
                // hosts picked on their own
                let picked = picks.with_untracked(|p| {
                    if p.is_empty() {
                        Vec::new()
                    } else {
                        layout.picked(p).1
                    }
                });
                rings.with_untracked(|rings| {
                    flashes.with_untracked(|flashes| {
                        // assignments hovered, clicked or picked
                        let mut focus: Vec<usize> = hovered_node
                            .with_untracked(|n| n.as_deref().and_then(|n| layout.group_index(n)))
                            .into_iter()
                            .collect();
                        chosen.with_untracked(|c| {
                            focus.extend(c.iter().filter_map(|n| layout.group_index(n)))
                        });
                        picks.with_untracked(|p| {
                            focus.extend(p.iter().filter_map(|g| match g {
                                Grab::Group(n) => layout.group_index(n),
                                Grab::Host { .. } => None,
                            }))
                        });
                        focus.retain(|i| matches!(layout.groups[*i].kind, Kind::Node));
                        let bundle = hovered_bundle
                            .with_untracked(|b| b.as_ref().and_then(|b| layout.bundle_index(b)));
                        let open: Vec<usize> = opened.with_untracked(|o| {
                            o.iter().filter_map(|b| layout.bundle_index(b)).collect()
                        });
                        let preview: Vec<usize> = previewed.with_untracked(|ids| {
                            if ids.is_empty() {
                                return Vec::new();
                            }
                            layout
                                .hosts
                                .iter()
                                .enumerate()
                                .filter(|(_, p)| ids.contains(&p.host_id))
                                .map(|(i, _)| i)
                                .collect()
                        });
                        let (busy_ribbons, busy_links) =
                            working.with_untracked(|w| layout.working(w));
                        let frame = Frame {
                            layout,
                            preview: &preview,
                            busy_ribbons: &busy_ribbons,
                            busy_links: &busy_links,
                            rings,
                            flashes,
                            view,
                            hovered: hovered.as_deref(),
                            focus: &focus,
                            links: links_mode.get_untracked(),
                            hovered_bundle: bundle,
                            opened: &open,
                            selected: selected.as_deref(),
                            picked: &picked,
                            names,
                        };
                        if redraw && let Some(ctx) = context(&hosts_canvas) {
                            canvas::draw_hosts(&ctx, w, h, dpr, &palette, &frame);
                        }
                        if flow_due && let Some(ctx) = flow_canvas.as_ref().and_then(context) {
                            canvas::draw_flow(&ctx, w, h, dpr, &palette, &frame, now);
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
    let edit = GraphEdit {
        catalog,
        hosts,
        message,
    };
    // D32: open an assignment in the right panel
    let edit_assignment = Callback::new(move |reference: String| {
        if moved.get_untracked() {
            return;
        }
        if let Some(entry) = catalog.assignment(&reference) {
            selected.set(None);
            catalog_message.set(None);
            form.set(Some(AssignmentForm::from_item(&entry.item)));
            editing.set(Some(Editing::Assignment));
        }
    });
    let remove_assignment = Callback::new(move |(reference, count): (String, usize)| {
        if !moved.get_untracked() {
            deleting.set(Some((reference, count)));
        }
    });
    let new_assignment = move |selector: String| {
        selected.set(None);
        catalog_message.set(None);
        form.set(Some(AssignmentForm::new(
            catalog.sources.first_editable(),
            selector,
        )));
        editing.set(Some(Editing::Assignment));
    };
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
    // Run on an assignment's card: one attribute written by hand, run once
    // on its hosts (D23)
    let run_attribute = move |target: GroupTarget, hosts: usize| {
        if moved.get_untracked() {
            return;
        }
        one_shot.set(Some((target, hosts)));
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
    let panel_open = Memo::new(move |_| {
        showing_host.get() || editing.with(Option::is_some) || active_run.with(Option::is_some)
    });

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
    // only clusters fold (D31)
    let all_folded = move || {
        map.with(|m| {
            collapsed.with(|c| {
                let mut clusters = m
                    .groups
                    .iter()
                    .filter(|g| matches!(g.kind, Kind::Cluster { .. }))
                    .peekable();
                clusters.peek().is_some() && clusters.all(|g| c.contains(&g.name))
            })
        })
    };
    let svg_class = move || {
        let mut class = String::from("map");
        if hovered.get().is_some() || hovered_bundle.with(Option::is_some) {
            class.push_str(" over-host");
        }
        if grab.with(Option::is_some) {
            class.push_str(" grabbing");
        }
        if connecting.with(Option::is_some) {
            class.push_str(" connecting");
        }
        class
    };
    // a drag ends: the picks stay where they were dropped; a
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
        arranged.save();
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
                        <span class="muted">"Links"</span>
                        <select title="Grouped: one ribbon per cluster and assignment (hover or click an assignment for its ribbons' statistics, a ribbon or a host for its hosts' links); per host: every host's link"
                            on:change=move |ev| {
                                let value = if event_target_value(&ev) == "per-host" { Links::PerHost } else { Links::Grouped };
                                value.save();
                                opened.set(BTreeSet::new());
                                links_mode.set(value);
                            }>
                            <option value="grouped" selected=move || links_mode.get() == Links::Grouped>"grouped"</option>
                            <option value="per-host" selected=move || links_mode.get() == Links::PerHost>"per host"</option>
                        </select>
                    // one button: fold every group, or unfold them all once all are folded
                    <button title="Fold or unfold every cluster of hosts"
                        on:click=move |_| {
                            hold_view();
                            glide();
                            if all_folded() {
                                collapsed.set(BTreeSet::new());
                            } else {
                                collapsed.set(map.with_untracked(|m| m.groups.iter().filter(|g| matches!(g.kind, Kind::Cluster { .. })).map(|g| g.name.clone()).collect()));
                            }
                        }>
                        {move || if all_folded() { "Unfold" } else { "Fold" }}
                    </button>
                    <button title="Zoom and pan so that everything on the map fits in view (moved cards and hosts stay where they are)"
                        on:click=move |_| {
                            scale.set(1.0);
                            center.set(None);
                            fitted.set(None);
                        }>
                        "Center"
                    </button>
                    <button title="Put every card and host back where the map places them (drag them to move them; Ctrl + click or Shift + drag to pick several)"
                        disabled=move || arrangement.with(Arrangement::is_empty)
                        on:click=move |_| {
                            glide();
                            let empty = Arrangement::default();
                            empty.save();
                            arrangement.set(empty);
                            // the whole map in view
                            scale.set(1.0);
                            center.set(None);
                            fitted.set(None);
                        }>
                        "Auto-arrange"
                    </button>
                    <button title="Create an assignment (or click a cluster: New assignment from these hosts; drag from a card's handle to a cluster to make it select those hosts)"
                        disabled=move || catalog.sources.0.with(|_| catalog.sources.editable().is_empty())
                        on:click=move |_| new_assignment("{}".to_string())>
                        "+ Assignment"
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
                <canvas class="flow" node_ref=flow_ref />
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
                        pressed.set_value(None);
                        // middle button: pan, whatever is under the pointer
                        if ev.button() == 1 {
                            ev.prevent_default();
                            drag.set(Some((x, y)));
                            return;
                        }
                        if ev.button() != 0 {
                            return;
                        }
                        let Some((w, h, _, _)) = metrics(ev.current_target()) else { return };
                        let ctrl = ev.ctrl_key() || ev.meta_key();
                        // D32: an assignment's handle: draw an arrow from it
                        let handle = ev
                            .target()
                            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                            .filter(|e| e.closest(".handle").ok().flatten().is_some())
                            .and_then(|e| e.closest("g.group.node").ok().flatten())
                            .and_then(|e| e.get_attribute("data-group"));
                        if let Some(node) = handle
                            && let Some((mx, my)) = map_point(&ev)
                        {
                            ev.prevent_default();
                            // not a click on the card
                            moved.set(true);
                            if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                                let _ = el.set_pointer_capture(ev.pointer_id());
                            }
                            connecting.set(Some(Connecting {
                                reference: node.strip_prefix("a:").unwrap_or(&node).to_string(),
                                node,
                                x: mx,
                                y: my,
                                target: None,
                            }));
                            return;
                        }
                        // a card's buttons only click
                        let on_button = ev
                            .target()
                            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                            .and_then(|e| e.closest("g.btn").ok().flatten())
                            .is_some();
                        if on_button {
                            return;
                        }
                        let card = ev
                            .target()
                            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                            .and_then(|e| e.closest("g.group").ok().flatten())
                            .and_then(|e| e.get_attribute("data-group"));
                        // a card; else a host; else a cluster's surface
                        // (anywhere inside its outline): the cluster
                        let what = match card {
                            Some(name) => Some(Grab::Group(name)),
                            None => grabbed_host(&ev)
                                .map(|(host, group)| Grab::Host { group, host })
                                .or_else(|| {
                                    let (mx, my) = map_point(&ev)?;
                                    map.with_untracked(|m| {
                                        Some(Grab::Group(m.groups[m.cluster_on(mx, my)?].name.clone()))
                                    })
                                }),
                        };
                        grab_start.set_value((x, y, units_per_pixel(w, h)));
                        match what {
                            // add it to the picks, or take it out; not a
                            // click (no fold, no host selected)
                            Some(what) if ctrl => {
                                moved.set(true);
                                picks.update(|p| if !p.remove(&what) { p.insert(what); });
                            }
                            // a drag once the pointer moves, else a click
                            Some(what) => pressed.set_value(Some(what)),
                            // Shift + drag on empty space: selection rectangle
                            None if ev.shift_key() => {
                                let Some((mx, my)) = map_point(&ev) else { return };
                                hold_view();
                                marquee_adds.set_value(ctrl);
                                marquee.set(Some((mx, my, mx, my)));
                            }
                            // empty space: pan
                            None => drag.set(Some((x, y))),
                        }
                    }
                    // the middle button pans, without the browser's autoscroll
                    on:mousedown=move |ev: web_sys::MouseEvent| if ev.button() == 1 { ev.prevent_default(); }
                    on:pointerup=move |_| {
                        // D32: an arrow dropped on hosts it does not select yet
                        if let Some(c) = connecting.get_untracked() {
                            connecting.set(None);
                            let targets = match &c.target {
                                Some(DropTarget::Cluster(_, signature)) if signature.contains(&c.reference) => {
                                    message.set(Some(format!("{} already selects these hosts", c.reference)));
                                    None
                                }
                                Some(DropTarget::Cluster(_, signature)) => Some(hosts.with_untracked(|h| signature_hosts(h, signature))),
                                Some(DropTarget::Host(id)) => {
                                    if hosts.with_untracked(|h| h.get(id).is_some_and(|h| h.assignments.contains(&c.reference))) {
                                        message.set(Some(format!("{} already selects {id}", c.reference)));
                                        None
                                    } else {
                                        Some(vec![id.clone()])
                                    }
                                }
                                None => None,
                            };
                            if let Some(targets) = targets.filter(|t| !t.is_empty()) {
                                arrow.set(Some((c.reference, targets, true)));
                            }
                            return;
                        }
                        pressed.set_value(None); drag.set(None); drop_grab();
                    }
                    on:pointercancel=move |_| { connecting.set(None); pressed.set_value(None); drag.set(None); grab.set(None); marquee.set(None); }
                    on:pointerleave=move |_| { connecting.set(None); pressed.set_value(None); drag.set(None); drop_grab(); hovered.set(None); hovered_bundle.set(None); hovered_cluster.set(None); }
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
                            // an assignment's card: select it (its ribbons'
                            // statistics stay shown), or drop it
                            let node = ev
                                .target()
                                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                                .and_then(|e| e.closest("g.group.node").ok().flatten())
                                .and_then(|e| e.get_attribute("data-group"));
                            if let Some(name) = node {
                                let ctrl = ev.ctrl_key() || ev.meta_key();
                                chosen.update(|c| {
                                    if c.contains(&name) {
                                        c.remove(&name);
                                    } else if ctrl {
                                        c.insert(name);
                                    } else {
                                        *c = [name].into();
                                    }
                                });
                            }
                            return;
                        }
                        // D31: a ribbon: open its hosts' links (or close them)
                        let host = host_under(&ev);
                        if host.is_none() && let Some(names) = bundle_under(&ev) {
                            opened.update(|o| if !o.remove(&names) { o.insert(names); });
                            return;
                        }
                        // D32: a cluster's surface: its labels in the right panel
                        if host.is_none() {
                            let signature = map_point(&ev).and_then(|(mx, my)| map.with_untracked(|m| {
                                match &m.groups[m.cluster_on(mx, my)?].kind {
                                    Kind::Cluster { signature } => Some(signature.clone()),
                                    Kind::Node => None,
                                }
                            }));
                            if let Some(signature) = signature {
                                selected.set(None);
                                editing.set(Some(Editing::Cluster(signature)));
                                return;
                            }
                            if editing.with_untracked(|e| matches!(e, Some(Editing::Cluster(_)))) {
                                editing.set(None);
                            }
                        }
                        // a host: show it in the right panel; empty space: deselect
                        if host.is_none() && chosen.with_untracked(|c| !c.is_empty()) {
                            chosen.set(BTreeSet::new());
                        }
                        selected.set(host);
                        if picks.with_untracked(|p| !p.is_empty()) {
                            picks.set(Picks::new());
                        }
                    }
                    on:pointermove=move |ev| {
                        // D32: an arrow being drawn follows the pointer; a
                        // host, else a cluster, under it is its target
                        if connecting.with_untracked(Option::is_some) {
                            let Some((mx, my)) = map_point(&ev) else { return };
                            let target = host_under(&ev).map(DropTarget::Host).or_else(|| {
                                map.with_untracked(|m| {
                                    let g = &m.groups[m.cluster_at(mx, my)?];
                                    let Kind::Cluster { signature } = &g.kind else { return None };
                                    Some(DropTarget::Cluster(g.name.clone(), signature.clone()))
                                })
                            });
                            connecting.update(|c| if let Some(c) = c {
                                (c.x, c.y, c.target) = (mx, my, target);
                            });
                            return;
                        }
                        // a pressed card or host: dragged once the pointer
                        // has moved a little (it and the other picks)
                        if let Some(what) = pressed.get_value() {
                            let (x0, y0, _) = grab_start.get_value();
                            let (nx, ny) = (ev.client_x() as f64, ev.client_y() as f64);
                            if (nx - x0).abs() + (ny - y0).abs() < 4.0 {
                                return;
                            }
                            pressed.set_value(None);
                            if !picks.with_untracked(|p| p.contains(&what)) {
                                picks.set([what].into());
                            }
                            hovered.set(None);
                            hold_view();
                            grab.set(Some((0.0, 0.0)));
                        }
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
                            let ribbon = if id.is_none() { bundle_under(&ev) } else { None };
                            if hovered.with_untracked(|h| *h != id) {
                                hovered.set(id);
                            }
                            if hovered_bundle.with_untracked(|b| *b != ribbon) {
                                hovered_bundle.set(ribbon);
                            }
                            // the cluster under the pointer, inside its
                            // outline too: its caption shows
                            let cluster = map_point(&ev).and_then(|(mx, my)| {
                                map.with_untracked(|m| {
                                    let g = &m.groups[m.cluster_at(mx, my)?];
                                    let Kind::Cluster { signature } = &g.kind else { return None };
                                    Some((g.name.clone(), signature.clone()))
                                })
                            });
                            if hovered_cluster.with_untracked(|c| c.as_ref().map(|c| &c.0) != cluster.as_ref().map(|c| &c.0)) {
                                hovered_cluster.set(cluster);
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
                    <defs>
                        <marker id="connect-head" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto">
                            <path d="M0 0 L10 5 L0 10 Z" />
                        </marker>
                    </defs>
                    // hosts and links: on the canvas, under the cards
                    // group cards, on top
                    <For each=move || map.with(|m| {
                            let running = rings.with(|rings| m.running(rings));
                            m.groups.iter().cloned().zip(running).collect::<Vec<_>>()
                        })
                        // the target too: the buttons appear once the assignments are loaded
                        key=|(g, running)| (g.name.clone(), g.total, g.ok, g.bad, *running, g.collapsed, g.target.is_some())
                        let:entry>
                        {card(entry.0, entry.1, CardSignals { cards, picks, moved, hovered_node, hovered_cluster, chosen, editable, connecting, edit: edit_assignment, remove: remove_assignment }, toggle_group, CardActions { ask, run_attribute })}
                    </For>
                    // D32: the arrow being drawn, from the card's handle
                    {move || connecting.get().map(|c| {
                        let (sx, sy) = cards.with(|k| k.get(&c.node).copied())
                            .map_or((c.x, c.y), |(x, y)| (x, y + CARD_H / 2.0));
                        let bend = ((c.y - sy).abs() / 2.0).max(40.0);
                        let d = format!("M{sx} {sy} C{sx} {} {} {} {} {}", sy + bend, c.x, c.y - bend, c.x, c.y);
                        view! { <path class="connecting" class:on-target=c.target.is_some() d=d marker-end="url(#connect-head)" /> }
                    })}
                    // D32: a selected arrow (ribbon opened by a click) of an
                    // editable assignment: a button removing it
                    {move || {
                        let open = opened.get();
                        cards.track();
                        open.into_iter().filter_map(|names| {
                            let reference = names.1.strip_prefix("a:")?.to_string();
                            if !editable.with(|e| e.contains(&reference)) {
                                return None;
                            }
                            let ((x, y), signature) = map.with(|m| {
                                let mid = m.bundle_mid(&names)?;
                                match &m.group_named(&names.0)?.1.kind {
                                    Kind::Cluster { signature } => Some((mid, signature.clone())),
                                    Kind::Node => None,
                                }
                            })?;
                            let title = format!("Remove this arrow: {reference} stops selecting these hosts (Delete)");
                            Some(view! {
                                <g class="btn edge-btn" transform=format!("translate({x} {y})")
                                    on:click=move |ev| {
                                        ev.stop_propagation();
                                        let targets = hosts.with_untracked(|h| signature_hosts(h, &signature));
                                        arrow.set(Some((reference.clone(), targets, false)));
                                    }>
                                    <circle r="10" />
                                    <path d="M-4 -4 L4 4 M4 -4 L-4 4" />
                                    <title>{title}</title>
                                </g>
                            })
                        }).collect_view()
                    }}
                    // selection rectangle
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
                                <li><svg width="16" height="16"><path d="M8 1 V15" stroke="var(--muted)" stroke-width="2" stroke-dasharray="2 2" /></svg> " a cluster with both: proactive hosts on the left, passive ones on the right"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="6.5" fill="none" stroke="var(--muted)" stroke-width="1" /></svg> " graph: hosts selected by exactly the same assignments, drawn together; point at it for its assignments and counts (folded: a pie of their states)"</li>
                                <li><svg width="16" height="16"><path d="M1 8 H9" stroke="var(--muted)" stroke-width="5" opacity=".5" /><path d="M9 3 L15 8 L9 13 Z" fill="var(--muted)" opacity=".5" /></svg> " graph: one ribbon per cluster and assignment, as wide as its hosts, pointing from the assignment to the hosts it constrains (faint chevrons flow along it); assignments sharing many hosts lie close together; hover or click the assignment for its hosts' states, the ribbon for its hosts' links"</li>
                                <li><svg width="16" height="16"><path d="M15 4 L5 10" stroke="var(--bad)" stroke-width="2" /><path d="M1 12.5 L6.5 12.6 L4.4 7.6 Z" fill="var(--bad)" /></svg> " a link assignment → host (hover a host or a ribbon, or Links: per host): the host's state for that assignment only; a host is its worst link"</li>
                                <li><svg width="16" height="16"><circle cx="8" cy="8" r="5" fill="var(--panel)" stroke="var(--accent)" stroke-width="2" /></svg> " handle under an assignment's card: drag it to hosts to make the assignment select them; click a ribbon, then its × (or Delete) to stop; click inside a cluster to edit its labels"</li>
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
                    {move || {
                        let back = if editing.with(Option::is_some) {
                            Some("← Back to the edit")
                        } else {
                            active_run.with(Option::is_some).then_some("← Back to the run")
                        };
                        back.map(|text| view! { <button class="small back" on:click=move |_| selected.set(None)>{text}</button> })
                    }}
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
                } else if let Some(what) = editing.get() {
                    match what {
                        Editing::Assignment => view! {
                            {message_view(catalog_message)}
                            <AssignmentEditor form catalog message=catalog_message compact=true
                                on_saved=move |()| {
                                    message.set(Some("assignment saved: the map updates in a moment".into()));
                                    let _ = form.try_set(None);
                                } />
                        }.into_any(),
                        Editing::Cluster(signature) => view! {
                            <ClusterPanel edit signature
                                on_new=move |selector: String| new_assignment(selector)
                                on_close=move |()| editing.set(None) />
                        }.into_any(),
                    }
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
                                <button type="button" on:click=move |_| pending.set(None)>"Cancel"</button>
                                <button class="primary" class:reach=reach type="submit">{job_label(run.job as i32)}</button>
                            </div>
                        </form>
                    </div>
                }
            })}
        </Show>
        {move || one_shot.get().map(|(target, hosts)| view! {
            <OneShotDialog target hosts on_close=Callback::new(move |_| one_shot.set(None)) />
        })}
        {move || arrow.get().map(|(assignment, targets, adding)| view! {
            <ArrowDialog edit assignment targets adding on_close=move |()| arrow.set(None) />
        })}
        {move || deleting.get().map(|(assignment, hosts)| view! {
            <DeleteAssignmentDialog edit assignment hosts on_close=move |()| deleting.set(None) />
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
            <dt>"Assignments"</dt><dd>{assignments_line(&host)}{crate::pages::assignment_states(&host)}</dd>
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
    /// The pointer moved since it went down (a drag, not a click).
    moved: RwSignal<bool>,
    /// D31: the assignment node under the pointer (its links highlighted).
    hovered_node: RwSignal<Option<String>>,
    /// D31: the cluster under the pointer (name, signature): its
    /// assignments' cards are highlighted.
    hovered_cluster: RwSignal<Option<(String, Vec<String>)>>,
    /// D31: assignment nodes selected by a click (their ribbons' statistics
    /// shown).
    chosen: RwSignal<BTreeSet<String>>,
    /// D32: the assignments regency may edit (references), the arrow being
    /// drawn, and what the card's Edit / Delete do.
    editable: Memo<BTreeSet<String>>,
    connecting: RwSignal<Option<Connecting>>,
    edit: Callback<String>,
    remove: Callback<(String, usize)>,
}

/// What an assignment's buttons do: `ask` confirms an Assess / Reach,
/// `run_attribute` opens the one-shot dialog (D23).
#[derive(Clone, Copy)]
struct CardActions<A, R> {
    ask: A,
    run_attribute: R,
}

/// A group's card: an assignment's node, or a cluster's chip / card.
fn card(
    group: Group,
    running: usize,
    signals: CardSignals,
    toggle_group: impl Fn(String) + Copy + 'static,
    actions: CardActions<
        impl Fn(GroupTarget, api::JobKind, usize) + Copy + 'static,
        impl Fn(GroupTarget, usize) + Copy + 'static,
    >,
) -> AnyView {
    match &group.kind {
        Kind::Node => group_card(group, running, signals, actions).into_any(),
        Kind::Cluster { .. } => cluster_chip(group, running, signals, toggle_group).into_any(),
    }
}

/// Where a card is drawn: as it glides or is dragged, else where it belongs.
fn card_transform(
    cards: RwSignal<HashMap<String, (f64, f64)>>,
    group: &Group,
) -> impl Fn() -> String + use<> {
    let (name, gx, gy) = (group.name.clone(), group.x, group.y);
    move || {
        let (x, y) = cards.with(|c| c.get(&name).copied()).unwrap_or((gx, gy));
        format!("translate({x} {y})")
    }
}

/// A click on a cluster's card folds / unfolds it (not at the end of a drag,
/// nor a Ctrl + click, which picks it).
fn fold_click(
    name: String,
    signals: CardSignals,
    toggle_group: impl Fn(String) + Copy + 'static,
) -> impl Fn(web_sys::MouseEvent) {
    move |_| {
        if !signals.moved.get_untracked() {
            toggle_group(name.clone());
        }
    }
}

/// D31 graph display: a cluster is only a visual helper, not a thing of
/// its own: a soft outline around its hosts with a small caption on its
/// edge (its assignments, its count); folded, a small badge (a pie of its
/// hosts' states). Grab the outline, the caption or the badge to move it;
/// a click on the caption or the badge folds / unfolds it. Hosts inside
/// stay clickable (the outline's inside lets the pointer through).
fn cluster_chip(
    group: Group,
    running: usize,
    signals: CardSignals,
    toggle_group: impl Fn(String) + Copy + 'static,
) -> impl IntoView {
    let r = if group.collapsed {
        BADGE_R
    } else {
        group.radius
    };
    let signature = match &group.kind {
        Kind::Cluster { signature } => signature.clone(),
        Kind::Node => Vec::new(),
    };
    let picked_as = Grab::Group(group.name.clone());
    let picks = signals.picks;
    let hovered_cluster = signals.hovered_cluster;
    // D32: an arrow being drawn over it (already selecting it, or not)
    let connecting = signals.connecting;
    let target_as = group.name.clone();
    let on_target = Signal::derive(move || {
        connecting.with(|c| {
            c.as_ref().and_then(|c| match &c.target {
                Some(DropTarget::Cluster(name, signature)) if *name == target_as => {
                    Some(signature.contains(&c.reference))
                }
                _ => None,
            })
        })
    });
    let drop_target = move || on_target.get() == Some(false);
    let drop_already = move || on_target.get() == Some(true);
    let count = format!(
        "{} host{}",
        group.total,
        if group.total == 1 { "" } else { "s" }
    );
    let bad = (group.bad > 0).then(|| format!(" · {} not compliant", group.bad));
    let busy = (running > 0).then(|| format!(" · {running} running"));
    // one line per assignment, then the counts; the last line just above
    // the outline
    let names: Vec<String> = if signature.is_empty() {
        vec![cluster_label(&signature)]
    } else {
        signature.iter().map(|a| truncate(a, 40)).collect()
    };
    let top = -r - 9.0 - names.len() as f64 * CAPTION_LINE;
    let shown_as = group.name.clone();
    let shown = move || hovered_cluster.with(|h| h.as_ref().is_some_and(|h| h.0 == shown_as));
    let title = format!(
        "Hosts selected by exactly: {}. Not a group of its own, only where these hosts are drawn. Click the caption to {}, drag to move",
        group.label,
        if group.collapsed { "unfold" } else { "fold" }
    );
    let caption = view! {
        <text class="caption" class:shown=shown x="0" y=top text-anchor="middle"
            on:click=fold_click(group.name.clone(), signals, toggle_group)>
            {names.into_iter().enumerate().map(|(i, name)| view! {
                <tspan x="0" dy=if i == 0 { 0.0 } else { CAPTION_LINE }>{name}</tspan>
            }).collect_view()}
            <tspan class="caption-count" x="0" dy=CAPTION_LINE>{count}</tspan>
            {bad.map(|b| view! { <tspan class="count-bad">{b}</tspan> })}
            {busy.map(|b| view! { <tspan class="running">{b}</tspan> })}
            {group.collapsed.then_some(" · folded")}
        </text>
    };
    let body = if group.collapsed {
        let total = group.total.max(1) as f64;
        let (ok, bad) = (group.ok as f64 / total, group.bad as f64 / total);
        let other = 1.0 - ok - bad;
        let ring = r - 4.0;
        view! {
            <g class="badge" on:click=fold_click(group.name.clone(), signals, toggle_group)>
                <circle class="badge-bg" r=r />
                {(group.ok > 0).then(|| arc(0.0, 0.0, ring, 0.0, ok, "var(--ok)"))}
                {(group.bad > 0).then(|| arc(0.0, 0.0, ring, ok, bad, "var(--bad)"))}
                {(other > 0.001 && group.total > 0).then(|| arc(0.0, 0.0, ring, ok + bad, other, "var(--unknown)"))}
                <text class="badge-count" y="4" text-anchor="middle">{group.total.to_string()}</text>
            </g>
        }
        .into_any()
    } else {
        view! {
            <circle class="hull" r=r />
            <circle class="hull-hit" r=r />
        }
        .into_any()
    };
    view! {
        <g class="group cluster" class:picked=move || picks.with(|p| p.contains(&picked_as))
            class:folded=group.collapsed class:drop-target=drop_target class:drop-already=drop_already
            data-group=group.name.clone() transform=card_transform(signals.cards, &group)>
            {body}
            {caption}
            <title>{title}</title>
        </g>
    }
}

/// An assignment's node: name, host count (and jobs running), counts and
/// compliance donut, then Assess, Reach and Run (one attribute written by
/// hand, run once on its hosts).
fn group_card(
    group: Group,
    running: usize,
    signals: CardSignals,
    actions: CardActions<
        impl Fn(GroupTarget, api::JobKind, usize) + Copy + 'static,
        impl Fn(GroupTarget, usize) + Copy + 'static,
    >,
) -> impl IntoView {
    let CardSignals {
        picks,
        hovered_node,
        hovered_cluster,
        chosen,
        editable,
        edit,
        remove,
        ..
    } = signals;
    let CardActions { ask, run_attribute } = actions;
    let reference = group.label.clone();
    let related =
        move || hovered_cluster.with(|c| c.as_ref().is_some_and(|c| c.1.contains(&reference)));
    let (w, h) = (CARD_W, CARD_H);
    let (left, top) = (-w / 2.0, -h / 2.0);
    let picked_as = Grab::Group(group.name.clone());
    let chosen_as = group.name.clone();
    let name_for_title = group.label.clone();
    let transform = card_transform(signals.cards, &group);
    let (enter, leave) = (group.name.clone(), group.name.clone());
    let total = group.total.max(1) as f64;
    let (ok, bad) = (group.ok as f64 / total, group.bad as f64 / total);
    let other = 1.0 - ok - bad;
    // the donut on the right, under the title, beside the counts
    let (dx, dy, dr) = (-left - 32.0, top + 58.0, 17.0);
    let pct = if group.total == 0 {
        "—".to_string()
    } else {
        format!("{}%", (ok * 100.0).round())
    };
    let sub = format!(
        "{} host{}",
        group.total,
        if group.total == 1 { "" } else { "s" },
    );
    let running_text = (running > 0).then(|| {
        view! { <tspan class="running">{format!(" · {running} running")}</tspan> }
    });
    let buttons = group.target.clone().map(|target| {
        let (gap, pad) = (6.0, 12.0);
        let bw = (w - 2.0 * pad - 2.0 * gap) / 3.0;
        let by = top + h - 30.0;
        let x = |i: f64| left + pad + i * (bw + gap);
        let count = group.total;
        let (t1, t2, t3) = (target.clone(), target.clone(), target);
        view! {
            <g class="btn assess" on:click=move |ev| { ev.stop_propagation(); ask(t1.clone(), api::JobKind::Assess, count); }>
                <rect x=x(0.0) y=by width=bw height="22" rx="6" />
                <text x=x(0.0) + bw / 2.0 y=by + 15.0 text-anchor="middle">"Assess"</text>
                <title>"Assess this assignment on its hosts (read only; its expected state only)"</title>
            </g>
            <g class="btn reach" on:click=move |ev| { ev.stop_propagation(); ask(t2.clone(), api::JobKind::Reach, count); }>
                <rect x=x(1.0) y=by width=bw height="22" rx="6" />
                <text x=x(1.0) + bw / 2.0 y=by + 15.0 text-anchor="middle">"Reach"</text>
                <title>"Reach this assignment's expected state on its hosts: changes them (only this assignment)"</title>
            </g>
            <g class="btn run" on:click=move |ev| { ev.stop_propagation(); run_attribute(t3.clone(), count); }>
                <rect x=x(2.0) y=by width=bw height="22" rx="6" />
                <text x=x(2.0) + bw / 2.0 y=by + 15.0 text-anchor="middle">"Run"</text>
                <title>"Write one attribute by hand and run it once on this assignment's hosts (Assess or Reach)"</title>
            </g>
        }
    });
    // D32: above the card (hovered or selected), Edit / Delete; at its
    // bottom, the handle arrows are drawn from (React Flow like)
    let tools = {
        let reference = group.label.clone();
        let count = group.total;
        let (tw, th, ty) = (64.0, 22.0, top - 28.0);
        move || {
            let (r1, r2) = (reference.clone(), reference.clone());
            if editable.with(|e| e.contains(&reference)) {
                view! {
                    <g class="node-tools">
                        <rect class="tools-bridge" x=left y=ty width=w height=28.0 />
                        <g class="btn tool" on:click=move |ev| { ev.stop_propagation(); edit.run(r1.clone()); }>
                            <rect x=-tw - 3.0 y=ty width=tw height=th rx="6" />
                            <text x=-tw / 2.0 - 3.0 y=ty + 15.0 text-anchor="middle">"Edit"</text>
                            <title>"Edit this assignment: hosts (label selector), priority, expected states"</title>
                        </g>
                        <g class="btn tool danger" on:click=move |ev| { ev.stop_propagation(); remove.run((r2.clone(), count)); }>
                            <rect x="3" y=ty width=tw height=th rx="6" />
                            <text x=tw / 2.0 + 3.0 y=ty + 15.0 text-anchor="middle">"Delete"</text>
                            <title>"Delete this assignment (Delete key)"</title>
                        </g>
                    </g>
                    <circle class="handle" cx="0" cy=top + h r="6">
                        <title>"Drag to a cluster or a host: this assignment selects them too"</title>
                    </circle>
                }
                .into_any()
            } else {
                view! {
                    <g class="node-tools">
                        <text class="tools-note" x="0" y=ty + 15.0 text-anchor="middle">"Read only: defined in Git"</text>
                    </g>
                }
                .into_any()
            }
        }
    };
    let bad_class = if group.bad > 0 {
        "count-bad"
    } else {
        "count-none"
    };
    view! {
        <g class="group node" class:picked=move || picks.with(|p| p.contains(&picked_as)) class:related=related
            class:chosen=move || chosen.with(|c| c.contains(&chosen_as))
            data-group=group.name.clone() transform=transform
            on:mouseenter=move |_| hovered_node.set(Some(enter.clone()))
            on:mouseleave=move |_| if hovered_node.with_untracked(|h| h.as_ref() == Some(&leave)) { hovered_node.set(None) }>
            <rect class="card" x=left y=top width=w height=h rx="10" />
            <rect class="band" x=left + 8.0 y=top width=w - 16.0 height="4" />
            <text class="title" x=left + 12.0 y=top + 24.0>{truncate(&group.label, 20)}</text>
            <text class="sub" x=left + 12.0 y=top + 40.0>{sub}{running_text}</text>
            <circle class="donut-track" cx=dx cy=dy r=dr />
            {(group.ok > 0).then(|| arc(dx, dy, dr, 0.0, ok, "var(--ok)"))}
            {(group.bad > 0).then(|| arc(dx, dy, dr, ok, bad, "var(--bad)"))}
            {(other > 0.001 && group.total > 0).then(|| arc(dx, dy, dr, ok + bad, other, "var(--unknown)"))}
            <text class="pct" x=dx y=dy + 3.5 text-anchor="middle">{pct}</text>
            <text class="count-ok" x=left + 12.0 y=top + 60.0>{format!("{} compliant", group.ok)}</text>
            <text class=bad_class x=left + 12.0 y=top + 75.0>{format!("{} not compliant", group.bad)}</text>
            {buttons}
            {tools}
            <title>{format!("Assignment {} — its hosts' states for it; hover or click it for the statistics of its ribbons; drag to move, Ctrl + click to pick; drag its bottom handle to hosts to make it select them", name_for_title)}</title>
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
    fn rings_never_overlap() {
        let (points, outer) = ring_offsets(500, RING_SPACING);
        assert_eq!(points.len(), 500);
        for (i, a) in points.iter().enumerate() {
            let r = (a.0 * a.0 + a.1 * a.1).sqrt();
            assert!(r >= RING_SPACING - 0.01 && r <= outer + 0.01, "{r}");
            for b in &points[i + 1..] {
                let d = ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
                assert!(d > DOT * 2.5, "{d}");
            }
        }
    }

    fn status(assignment: &str, compliance: &str) -> api::AssignmentStatus {
        api::AssignmentStatus {
            assignment: assignment.into(),
            compliance: compliance.into(),
            state_at: 1,
            state_revision: "r1".into(),
            revision: "r1".into(),
            ..Default::default()
        }
    }

    /// A host with assignments `git/base`, then `git/<kind>` for each kind
    /// (D29: they add up; none for a host without kind), each checked at
    /// its current revision with `compliance` (D30).
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
            assignment_states: assignments.iter().map(|a| status(a, compliance)).collect(),
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

    const WEB: &str = "c:git/base+git/web";
    const DB: &str = "c:git/base+git/db";

    fn clusters(map: &Layout) -> Vec<&Group> {
        map.groups
            .iter()
            .filter(|g| matches!(g.kind, Kind::Cluster { .. }))
            .collect()
    }

    #[test]
    fn hosts_are_drawn_once_by_signature() {
        let hosts = fleet();
        {
            let map = layout(&hosts, &BTreeSet::new(), &selectors());
            let mut names: Vec<(&str, usize)> = clusters(&map)
                .iter()
                .map(|g| (g.name.as_str(), g.total))
                .collect();
            names.sort();
            assert_eq!(
                names,
                [
                    ("c:", 1),
                    ("c:git/base+git/cache", 40),
                    (DB, 40),
                    ("c:git/base+git/db+git/web", 1),
                    (WEB, 40),
                ]
            );
            // every host once
            assert_eq!(map.hosts.len(), 122);
            assert_eq!(map.placements("shared").count(), 1);
            let lonely = map.placements("lonely").next().unwrap();
            assert_eq!(lonely.tone, Tone::Unassigned);
            let none = map.groups.iter().find(|g| g.name == "c:").unwrap();
            assert_eq!(none.label, "(unassigned)");
        }
    }

    #[test]
    fn graph_has_a_node_per_assignment_and_links_per_host() {
        let hosts = fleet();
        let map = layout(&hosts, &BTreeSet::new(), &selectors());
        assert!(map.links);
        let nodes: Vec<&Group> = map.groups.iter().filter(|g| g.kind == Kind::Node).collect();
        assert_eq!(nodes.len(), 4);
        let base = nodes.iter().find(|g| g.label == "git/base").unwrap();
        assert_eq!(base.total, 121);
        assert!(matches!(&base.target, Some(GroupTarget::Assignment(r, s))
            if r == "git/base" && s.assignments == ["git/base"]));
        // the shared host: one link per assignment, to each node
        let shared = map.placements("shared").next().unwrap();
        let linked: BTreeSet<&str> = shared
            .links
            .iter()
            .map(|(i, _)| map.groups[*i].label.as_str())
            .collect();
        assert_eq!(linked, ["git/base", "git/db", "git/web"].into());
        assert!(map.placements("lonely").next().unwrap().links.is_empty());
        // bundles: one per (cluster, assignment)
        assert_eq!(map.bundles.len(), 2 + 2 + 2 + 3);
        let web = map.group_index(WEB).unwrap();
        let web_node = map.group_index("a:git/web").unwrap();
        let bundle = map
            .bundles
            .iter()
            .find(|b| b.cluster == web && b.node == web_node)
            .unwrap();
        assert_eq!(bundle.tones.iter().sum::<usize>(), 40);
        assert!(bundle.tones[Tone::Bad.rank() as usize] > 0);
        // nothing overlaps: nodes and clusters
        for (i, a) in map.groups.iter().enumerate() {
            for b in &map.groups[i + 1..] {
                let d = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
                assert!(d + 1.0 >= a.radius + b.radius, "{} / {}", a.name, b.name);
            }
        }
        // deterministic
        let again = layout(&hosts, &BTreeSet::new(), &selectors());
        assert!(again == map);
    }

    #[test]
    fn a_cluster_is_hovered_inside_and_on_its_caption() {
        let hosts = fleet();
        let map = layout(&hosts, &BTreeSet::new(), &selectors());
        let web = map.group_index(WEB).unwrap();
        let g = &map.groups[web];
        // inside, near the edge (its hosts on rings)
        assert_eq!(map.cluster_at(g.x + g.radius * 0.7, g.y), Some(web));
        // on its caption: above the outline, two assignments and the counts
        assert_eq!(
            map.cluster_at(g.x, g.y - g.radius - 2.5 * CAPTION_LINE),
            Some(web)
        );
        // far away
        let (x, y) = (map.bounds.2 + 1000.0, map.bounds.3 + 1000.0);
        assert_eq!(map.cluster_at(x, y), None);
        // grabbed anywhere on its surface, not on its caption
        assert_eq!(map.cluster_on(g.x + g.radius * 0.7, g.y), Some(web));
        assert_eq!(
            map.cluster_on(g.x, g.y - g.radius - 2.5 * CAPTION_LINE),
            None
        );
    }

    #[test]
    fn assignments_sharing_hosts_lie_close() {
        // cache only ever with web; db alone
        let mut hosts = BTreeMap::new();
        for i in 0..30 {
            for (id, kinds) in [
                (format!("wc{i}"), &["web", "cache"][..]),
                (format!("d{i}"), &["db"][..]),
            ] {
                hosts.insert(id.clone(), host(&id, kinds, "compliant"));
            }
        }
        for i in 0..5 {
            let id = format!("w{i}");
            hosts.insert(id.clone(), host(&id, &["web"], "compliant"));
        }
        let map = layout(&hosts, &BTreeSet::new(), &selectors());
        let at = |r: &str| {
            map.center(map.group_index(&format!("a:git/{r}")).unwrap())
                .unwrap()
        };
        let dist = |a: &str, b: &str| {
            let ((x0, y0), (x1, y1)) = (at(a), at(b));
            ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt()
        };
        assert!(dist("web", "cache") < dist("web", "db"));
        assert!(dist("web", "cache") < dist("cache", "db"));
        // the one everywhere is nearer to each than they are to each other
        assert!(dist("base", "db") < dist("web", "db"));
    }

    #[test]
    fn links_have_the_hosts_state_for_their_assignment() {
        let mut hosts = fleet();
        // compliant with base, not with web: its web link red, the host red
        let h = hosts.get_mut("h0").unwrap();
        h.assignment_states = vec![
            status("git/base", "compliant"),
            status("git/web", "reach_failed"),
        ];
        let map = layout(&hosts, &BTreeSet::new(), &selectors());
        let p = map.placements("h0").next().unwrap();
        let tone_of = |label: &str| {
            p.links
                .iter()
                .find(|(i, _)| map.groups[*i].label == label)
                .unwrap()
                .1
        };
        assert_eq!(tone_of("git/base"), Tone::Ok);
        assert_eq!(tone_of("git/web"), Tone::Bad);
        assert_eq!(p.tone, Tone::Bad);
        // the web node counts it as not compliant, the base node as compliant
        let base = map.groups.iter().find(|g| g.name == "a:git/base").unwrap();
        let web = map.groups.iter().find(|g| g.name == "a:git/web").unwrap();
        let bad_web = hosts
            .values()
            .filter(|h| h.assignments.contains(&"git/web".to_string()))
            .filter(|h| {
                is_bad(crate::pages::link_state(
                    h,
                    h.assignment_states
                        .iter()
                        .find(|s| s.assignment == "git/web")
                        .unwrap(),
                ))
            })
            .count();
        assert_eq!(web.bad, bad_web);
        assert_eq!(base.bad, 18 - 1, "h0 is fine with base");
    }

    #[test]
    fn host_states_follow_assignments() {
        let mut h = host("h", &["web"], "compliant");
        assert_eq!(host_state(&h), "compliant");
        // one assignment changed since its check: outdated (grey), the worst
        // but for a bad one
        h.assignment_states[1].revision = "r2".into();
        assert_eq!(host_state(&h), "outdated");
        h.assignment_states[0].compliance = "not_compliant".into();
        assert_eq!(host_state(&h), "not_compliant");
        // never checked for one assignment
        let mut fresh = host("f", &["web"], "compliant");
        fresh.assignment_states[1] = api::AssignmentStatus {
            assignment: "git/web".into(),
            compliance: "unknown".into(),
            revision: "r1".into(),
            ..Default::default()
        };
        assert_eq!(host_state(&fresh), "unknown");
        h.assignment_error = "same Name at equal priority".into();
        assert_eq!(host_state(&h), "invalid");
        h.assignments.clear();
        assert_eq!(host_state(&h), "unassigned");
        // D28: a proactive host shows its own pass's compliance
        h.agent = "regops".into();
        h.compliance = "compliant".into();
        assert_eq!(host_state(&h), "compliant");
        h.compliance = "not_compliant".into();
        assert_eq!(host_state(&h), "not_compliant");
        // a backend from before D30: one state for the whole effective state
        let mut old = host("o", &["web"], "compliant");
        old.assignment_states.clear();
        assert_eq!(host_state(&old), "compliant");
        old.state_revision = "r0".into();
        assert_eq!(host_state(&old), "outdated");
    }

    #[test]
    fn proactive_left_passive_right() {
        let hosts = mixed_fleet();
        // graph: every host of a mixed cluster on its side of the gap
        let map = layout(&hosts, &BTreeSet::new(), &selectors());
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
            let d = ((p.x - g.x).powi(2) + (p.y - g.y).powi(2)).sqrt();
            assert!(d + DOT <= g.radius, "{}", p.host_id);
        }
        assert!(sides.0 > 0 && sides.1 > 0);
    }

    #[test]
    fn hosts_stay_in_name_order() {
        // states vary across the fleet: places follow names, not states,
        // or every host would move when one changes state
        let hosts = fleet();
        {
            let map = layout(&hosts, &BTreeSet::new(), &selectors());
            for (index, group) in map.groups.iter().enumerate() {
                let ids: Vec<&str> = map
                    .hosts
                    .iter()
                    .filter(|p| p.group == index)
                    .map(|p| p.host_id.as_str())
                    .collect();
                let mut sorted = ids.clone();
                sorted.sort();
                assert_eq!(ids, sorted, "{}", group.name);
            }
        }
    }

    #[test]
    fn folded_clusters_keep_their_bundles() {
        let hosts = fleet();
        let folded: BTreeSet<String> = [WEB.to_string(), DB.to_string()].into();
        let map = layout(&hosts, &folded, &selectors());
        let web = map.group_index(WEB).unwrap();
        assert!(map.folded(web));
        assert!(map.hosts.iter().all(|p| p.group != web));
        assert_eq!(map.placements("h3").count(), 0);
        assert_eq!(map.bundles.iter().filter(|b| b.cluster == web).count(), 2);
        // nodes never fold
        assert!(!map.folded(map.group_index("a:git/web").unwrap()));
    }

    #[test]
    fn moved_groups_and_hosts_stay_where_put() {
        let hosts = fleet();
        {
            let auto = layout(&hosts, &BTreeSet::new(), &selectors());
            let (web_index, web) = auto.group_named(WEB).unwrap();
            let h3 = auto.placed_in(WEB, "h3").unwrap();
            let mut arranged = Arrangement::default();
            arranged
                .groups
                .insert(WEB.into(), (web.x + 1000.0, web.y - 500.0));
            arranged
                .hosts
                .entry(WEB.into())
                .or_default()
                .insert("h3".into(), (-400.0, 30.0));
            let mut map = auto.clone();
            arrange(&mut map, &arranged);
            let (_, moved) = map.group_named(WEB).unwrap();
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
                } else {
                    assert_eq!((a.x, a.y), (b.x, b.y));
                }
            }
            let (x0, y0, x1, y1) = map.bounds;
            assert!(
                map.hosts
                    .iter()
                    .all(|p| p.x >= x0 && p.x <= x1 && p.y >= y0 && p.y <= y1)
            );
            // a drag shows the same as its result
            let picks: Picks = [Grab::Group(WEB.into())].into();
            let dragged = auto.shifted(&picks, 1000.0, -500.0);
            let (_, g) = dragged.group_named(WEB).unwrap();
            assert_eq!((g.x, g.y), (moved.x, moved.y));
            // nothing moved: the automatic layout
            let mut same = auto.clone();
            arrange(&mut same, &Arrangement::default());
            assert!(same == auto);
        }
        // graph: a node moved, its links follow (they point at its center)
        let auto = layout(&hosts, &BTreeSet::new(), &selectors());
        let node = auto.group_index("a:git/web").unwrap();
        let mut arranged = Arrangement::default();
        arranged.groups.insert("a:git/web".into(), (5000.0, 5000.0));
        let mut map = auto.clone();
        arrange(&mut map, &arranged);
        assert_eq!(map.center(node), Some((5000.0, 5000.0)));
    }

    #[test]
    fn picks_move_together() {
        let hosts = fleet();
        let auto = layout(&hosts, &BTreeSet::new(), &selectors());
        let host = |group: &str, host: &str| Grab::Host {
            group: group.into(),
            host: host.into(),
        };
        let picks: Picks = [Grab::Group(WEB.into()), host(WEB, "h3"), host(DB, "h1")].into();
        let mut arranged = Arrangement::default();
        auto.moved(&picks, 50.0, 20.0, &mut arranged);
        let (_, web) = auto.group_named(WEB).unwrap();
        assert_eq!(arranged.groups[WEB], (web.x + 50.0, web.y + 20.0));
        assert_eq!(arranged.hosts.len(), 1);
        let mut map = auto.clone();
        arrange(&mut map, &arranged);
        let dragged = auto.shifted(&picks, 50.0, 20.0);
        for (i, a) in auto.hosts.iter().enumerate() {
            let (b, c) = (&map.hosts[i], &dragged.hosts[i]);
            assert!(
                (b.x - c.x).abs() < 1e-6 && (b.y - c.y).abs() < 1e-6,
                "{}",
                a.host_id
            );
            let moves = auto.groups[a.group].name == WEB
                || (auto.groups[a.group].name == DB && a.host_id == "h1");
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
        let map = layout(&hosts, &BTreeSet::new(), &selectors());
        let (_, web) = map.group_named(WEB).unwrap();
        let r = web.radius + 1.0;
        let picks = map.in_box(web.x + r, web.y + r, web.x - r, web.y - r);
        assert!(picks.contains(&Grab::Group(WEB.into())));
        assert!(
            !picks
                .iter()
                .any(|p| matches!(p, Grab::Host { group, .. } if group == WEB))
        );
        let i = map.placed_in(DB, "h1").unwrap();
        let p = &map.hosts[i];
        let picks = map.in_box(p.x - 2.0, p.y - 2.0, p.x + 2.0, p.y + 2.0);
        assert_eq!(
            picks,
            [Grab::Host {
                group: DB.into(),
                host: "h1".into()
            }]
            .into()
        );
        assert!(map.in_box(-1e6, -1e6, -1e6 + 5.0, -1e6 + 5.0).is_empty());
    }

    #[test]
    fn folding_glides_from_the_layout_before() {
        let hosts = fleet();
        let open = layout(&hosts, &BTreeSet::new(), &selectors());
        let folded: BTreeSet<String> = [WEB.to_string()].into();
        let closed = layout(&hosts, &folded, &selectors());
        let mut motion = Motion::of(&open);
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
        let last = motion.frame(&closed, 1.0);
        let (web_index, web) = closed.group_named(WEB).unwrap();
        for p in &last.hosts[closed.hosts.len()..] {
            assert_eq!((p.group, p.x, p.y), (web_index, web.x, web.y));
        }
        assert!(last.hosts[..closed.hosts.len()] == closed.hosts[..]);
        let mut motion = Motion::of(&closed);
        let first = motion.frame(&open, 0.0);
        let (_, folded_web) = closed.group_named(WEB).unwrap();
        let h3 = open.placed_in(WEB, "h3").unwrap();
        assert_eq!(
            (first.hosts[h3].x, first.hosts[h3].y),
            (folded_web.x, folded_web.y)
        );
    }

    #[test]
    fn jobs_in_progress_count_in_their_cluster() {
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
        let jobs = [
            job("a", "h1", "STARTED", 10),
            job("b", "h2", "PENDING", 10),
            job("c", "h2", "FINISHED", 20),
        ]
        .into_iter()
        .collect();
        let rings = rings(&jobs);
        assert_eq!(rings, [("h1".to_string(), Ring::Started)].into());
        let map = layout(&hosts, &BTreeSet::new(), &selectors());
        assert_eq!(map.running(&rings).iter().sum::<usize>(), 1);
        let h1 = map.placements("h1").next().unwrap();
        assert_eq!(
            map.host_at(h1.x + 2.0, h1.y - 2.0, DOT).unwrap().host_id,
            "h1"
        );
        let (_, web) = map.group_named(WEB).unwrap();
        // D31: no chip in the middle, a host there; nothing past the outline
        assert!(map.host_at(web.x, web.y, DOT).is_some());
        assert!(map.host_at(web.x + web.radius + DOT, web.y, DOT).is_none());
    }

    #[test]
    fn links_of_an_assignment_with_a_job_in_progress() {
        let job = |id: &str, origin: &str, host: &str, state: &str, created_at| {
            let job = api::JobSummary {
                job_id: id.into(),
                origin: origin.into(),
                host_id: host.into(),
                state: state.into(),
                created_at,
                ..Default::default()
            };
            (job.job_id.clone(), job)
        };
        let jobs = [
            job("run:c1:shared:git/web", "c1", "shared", "STARTED", 10),
            job("run:c1:h1:git/db", "c1", "h1", "PENDING", 10),
            // a newer job of the same pair ended: back to normal
            job("run:c2:h1:git/db", "c2", "h1", "FINISHED", 20),
            // one-shot run and recurring job: no assignment in the key
            job("run:c3:h4", "c3", "h4", "STARTED", 10),
            job("rec:o1:r1:5:h7", "o1", "h7", "STARTED", 10),
        ]
        .into_iter()
        .collect();
        let working = working(&jobs);
        assert_eq!(
            working,
            [("shared".to_string(), "git/web".to_string())].into()
        );
        let map = layout(&fleet(), &BTreeSet::new(), &selectors());
        let (ribbons, links) = map.working(&working);
        let web = map.group_index(&node_key("git/web")).unwrap();
        assert_eq!(ribbons.len(), 1);
        let ribbon = &map.bundles[ribbons[0]];
        assert_eq!(ribbon.node, web);
        let shared = map.placements("shared").next().unwrap();
        assert_eq!(ribbon.cluster, shared.group);
        let index = map
            .hosts
            .iter()
            .position(|p| p.host_id == "shared")
            .unwrap();
        assert_eq!(links, [(index, web)].into());
    }

    #[test]
    fn ribbons_start_on_the_outline_and_are_found_under_the_pointer() {
        let map = layout(&fleet(), &BTreeSet::new(), &selectors());
        for b in &map.bundles {
            let c = &map.groups[b.cluster];
            let ((sx, sy), (ex, ey)) = map.bundle_ends(b).unwrap();
            let d = ((sx - c.x).powi(2) + (sy - c.y).powi(2)).sqrt();
            assert!((d - c.radius).abs() < 1e-6, "{d} vs {}", c.radius);
            // a ribbon is found at its middle (that one, or one as near)
            let (mx, my) = ((sx + ex) / 2.0, (sy + ey) / 2.0);
            let found = map.bundle_at(mx, my, 1.0, |_| true).unwrap();
            let names = map.bundle_names(found).unwrap();
            assert_eq!(map.bundle_index(&names), Some(found));
            // nothing drawn, nothing found
            assert!(map.bundle_at(mx, my, 1.0, |_| false).is_none());
        }
    }
}
