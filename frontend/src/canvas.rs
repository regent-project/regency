//! Canvas renderer of the map's hosts and links. With thousands of hosts,
//! one SVG element per dot, link, name and job ring made every update and
//! every zoom step re-render tens of thousands of DOM nodes; the canvas
//! draws them in a few paths instead, and only what is in view: hosts and
//! links on one canvas, redrawn only when something changes. A host with a
//! job in progress is simply drawn in the job colour: no animation, so an
//! idle map costs nothing, however many jobs run.
//! Group cards (tens of them, with buttons) and cluster outlines stay SVG,
//! on top, sharing the same view box.

use std::cell::RefCell;
use std::f64::consts::{FRAC_PI_2, TAU};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use js_sys::wasm_bindgen::closure::Closure;
use js_sys::wasm_bindgen::{JsCast, JsValue};
use web_sys::CanvasRenderingContext2d as Ctx;

use crate::map::{DOT, Layout, Links, Rings, Tone};

/// Height of host names, in map units (a dot is `2 * DOT` wide).
const NAME_FONT: f64 = 4.0;

/// Theme colours, read from the CSS variables of `style.css`.
pub struct Palette {
    ok: String,
    bad: String,
    warn: String,
    unknown: String,
    unassigned: String,
    job: String,
    canvas: String,
    fg: String,
    muted: String,
    accent: String,
}

impl Palette {
    pub fn read() -> Self {
        let style = web_sys::window().and_then(|window| {
            let root = window.document()?.document_element()?;
            window.get_computed_style(&root).ok().flatten()
        });
        let var = |name: &str, fallback: &str| {
            style
                .as_ref()
                .and_then(|s| s.get_property_value(name).ok())
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| fallback.to_string())
        };
        Self {
            ok: var("--ok", "#4e9a74"),
            bad: var("--bad", "#c4625c"),
            warn: var("--warn", "#c39549"),
            unknown: var("--unknown", "#6d7a86"),
            unassigned: var("--unassigned", "#3d4852"),
            job: var("--job", "#7fb2d6"),
            canvas: var("--canvas", "#1a2127"),
            fg: var("--fg", "#d3dbe2"),
            muted: var("--muted", "#8796a3"),
            accent: var("--accent", "#6f9fbd"),
        }
    }

    fn tone(&self, tone: Tone) -> &str {
        match tone {
            Tone::Ok => &self.ok,
            Tone::Bad => &self.bad,
            Tone::Warn => &self.warn,
            Tone::Unknown => &self.unknown,
            Tone::Unassigned => &self.unassigned,
        }
    }
}

/// Links stop this far from a group's center, still under its card (a
/// 236 × 124 rectangle): shorter lines, not all meeting in one point.
const LINK_STOP: f64 = 58.0;

/// The point of segment `a`→`center` at `LINK_STOP` from `center`.
pub fn link_end(ax: f64, ay: f64, cx: f64, cy: f64) -> (f64, f64) {
    let (dx, dy) = (ax - cx, ay - cy);
    let len = (dx * dx + dy * dy).sqrt();
    if len <= LINK_STOP {
        (cx, cy)
    } else {
        (cx + dx / len * LINK_STOP, cy + dy / len * LINK_STOP)
    }
}

/// The part of the map shown: center and size of the view box, in map units.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct View {
    pub cx: f64,
    pub cy: f64,
    pub w: f64,
    pub h: f64,
}

impl View {
    /// Map units per CSS pixel in an element of `width` × `height` pixels,
    /// fitted like SVG's default `preserveAspectRatio` (xMidYMid meet).
    pub fn units_per_pixel(&self, width: f64, height: f64) -> f64 {
        (self.w / width.max(1.0)).max(self.h / height.max(1.0))
    }

    /// Map point under the pixel (`px`, `py`) of such an element.
    pub fn map_point(self, px: f64, py: f64, width: f64, height: f64) -> (f64, f64) {
        let k = self.units_per_pixel(width, height);
        (
            self.cx + (px - width / 2.0) * k,
            self.cy + (py - height / 2.0) * k,
        )
    }
}

/// Everything one frame shows.
pub struct Frame<'a> {
    pub layout: &'a Layout,
    /// Hosts with a job in progress, drawn in the job colour.
    pub rings: &'a Rings,
    /// Hosts whose job was only seen finished, drawn in the job colour for
    /// a moment (`pages::Flashes`).
    pub flashes: &'a crate::pages::Flashes,
    pub view: View,
    pub hovered: Option<&'a str>,
    /// D31: the assignment nodes hovered or selected (indices in the
    /// layout's groups): their ribbons stand out, with their statistics.
    pub focus: &'a [usize],
    /// D31: how links are drawn.
    pub links: Links,
    /// D31: the ribbon under the pointer (index in the layout's bundles):
    /// its hosts' links fan out.
    pub hovered_bundle: Option<usize>,
    /// D31: ribbons opened by a click: their hosts' links stay fanned out.
    pub opened: &'a [usize],
    /// The host shown in the right panel.
    pub selected: Option<&'a str>,
    /// Hosts picked on the map (Ctrl + click, Shift + drag): indices in `layout.hosts`.
    pub picked: &'a [usize],
    /// Draw host names (zoomed in).
    pub names: bool,
}

fn dash(pattern: &[f64]) -> JsValue {
    pattern
        .iter()
        .map(|v| JsValue::from_f64(*v))
        .collect::<js_sys::Array>()
        .into()
}

/// (cluster, node) of the ribbons hovered or opened: their hosts' own links
/// are drawn instead.
fn fanned(f: &Frame) -> Vec<(usize, usize)> {
    f.layout
        .bundles
        .iter()
        .enumerate()
        .filter(|(i, _)| f.hovered_bundle == Some(*i) || f.opened.contains(i))
        .map(|(_, b)| (b.cluster, b.node))
        .collect()
}

/// Whether `bundle` is drawn as a ribbon: grouped links (per host: only
/// zoomed far out or folded), unless its hosts' own links fan out.
fn ribbon_drawn(
    f: &Frame,
    bundle: &crate::map::Bundle,
    tiny: bool,
    fanned: &[(usize, usize)],
) -> bool {
    let folded = f.layout.folded(bundle.cluster);
    if f.links == Links::PerHost && !(tiny || folded) {
        return false;
    }
    folded || !fanned.contains(&(bundle.cluster, bundle.node))
}

/// Pixels between two chevrons of the flow, and their speed (pixels per
/// second): light and slow.
const FLOW_SPACING: f64 = 56.0;
const FLOW_SPEED: f64 = 16.0;

/// D31: the flow along the ribbons, on its own canvas over the hosts' one
/// (redrawn ~30 times a second, the hosts' canvas only when something
/// changes): faint chevrons sliding from each assignment towards the hosts
/// it constrains ("the assignment spreads the truth to its hosts"), `now`
/// in milliseconds. Nothing when zoomed far out.
pub fn draw_flow(
    ctx: &Ctx,
    width: f64,
    height: f64,
    dpr: f64,
    palette: &Palette,
    f: &Frame,
    now: f64,
) {
    let vp = Viewport::setup(ctx, width, height, dpr, f.view);
    let tiny = DOT * vp.k < 3.0;
    if !f.layout.links || tiny {
        return;
    }
    let fanned = fanned(f);
    let focus = !f.focus.is_empty() || f.hovered_bundle.is_some();
    let phase = (now / 1000.0 * FLOW_SPEED) % FLOW_SPACING;
    ctx.set_line_cap("round");
    ctx.set_line_join("round");
    ctx.set_stroke_style_str(&palette.fg);
    ctx.set_line_width(1.4 / vp.k);
    for (i, bundle) in f.layout.bundles.iter().enumerate() {
        if !ribbon_drawn(f, bundle, tiny, &fanned) {
            continue;
        }
        let Some(((cx, cy), (ex, ey))) = f.layout.bundle_ends(bundle) else {
            continue;
        };
        if !vp.segment_seen(cx, cy, ex, ey) {
            continue;
        }
        let total: usize = bundle.tones.iter().sum();
        if total == 0 {
            continue;
        }
        let strong =
            f.focus.contains(&bundle.node) || f.hovered_bundle == Some(i) || f.opened.contains(&i);
        let alpha = match (strong, focus) {
            (true, _) => 0.45,
            (false, false) => 0.22,
            (false, true) => 0.07,
        };
        let pixels = crate::map::ribbon_pixels(total);
        // from the card to the arrowhead's base, in pixels along the ribbon
        let (ux, uy, len) = unit(ex, ey, cx, cy);
        let head = (pixels * 1.1 + 7.0) / vp.k;
        let length = (len - head) * vp.k;
        if length < FLOW_SPACING {
            continue;
        }
        let (half, depth) = ((pixels * 0.45 + 2.5) / vp.k, (pixels * 0.35 + 3.0) / vp.k);
        let mut at = phase;
        while at < length {
            // fade in after the card, out before the arrowhead
            let edge = at.min(length - at);
            let a = alpha * (edge / 18.0).clamp(0.0, 1.0);
            if a > 0.005 {
                let (px, py) = (ex + ux * at / vp.k, ey + uy * at / vp.k);
                ctx.set_global_alpha(a);
                ctx.begin_path();
                ctx.move_to(px - ux * depth - uy * half, py - uy * depth + ux * half);
                ctx.line_to(px, py);
                ctx.line_to(px - ux * depth + uy * half, py - uy * depth - ux * half);
                ctx.stroke();
            }
            at += FLOW_SPACING;
        }
    }
    ctx.set_global_alpha(1.0);
    ctx.set_line_join("miter");
    ctx.set_line_cap("butt");
}

/// The unit vector from (`x0`, `y0`) to (`x1`, `y1`), and the length.
fn unit(x0: f64, y0: f64, x1: f64, y1: f64) -> (f64, f64, f64) {
    let (dx, dy) = (x1 - x0, y1 - y0);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-9 {
        (0.0, 0.0, 0.0)
    } else {
        (dx / len, dy / len, len)
    }
}

/// Adds an arrowhead to the current path: its tip at `tip`, pointing
/// along `u` (a unit vector), `length` long and `half` wide on each side.
fn arrow_head(ctx: &Ctx, tip: (f64, f64), u: (f64, f64), length: f64, half: f64) {
    let (bx, by) = (tip.0 - u.0 * length, tip.1 - u.1 * length);
    ctx.move_to(tip.0, tip.1);
    ctx.line_to(bx - u.1 * half, by + u.0 * half);
    ctx.line_to(bx + u.1 * half, by - u.0 * half);
    ctx.close_path();
}

fn circle(ctx: &Ctx, x: f64, y: f64, r: f64) {
    ctx.move_to(x + r, y);
    let _ = ctx.arc(x, y, r, 0.0, TAU);
}

/// A proactive host's mark (D28): a diamond about as large as a dot.
fn diamond(ctx: &Ctx, x: f64, y: f64, r: f64) {
    let d = r * 1.3;
    ctx.move_to(x, y - d);
    ctx.line_to(x + d, y);
    ctx.line_to(x, y + d);
    ctx.line_to(x - d, y);
    ctx.close_path();
}

/// A canvas set up for drawing in map units, and the part of the map in view.
#[derive(Clone, Copy)]
struct Viewport {
    /// CSS pixels per map unit.
    k: f64,
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
}

impl Viewport {
    /// Clear `ctx` (`width` × `height` CSS pixels, backing store scaled by
    /// `dpr`) and set its transform to map units.
    fn setup(ctx: &Ctx, width: f64, height: f64, dpr: f64, view: View) -> Self {
        let _ = ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        ctx.clear_rect(0.0, 0.0, width * dpr, height * dpr);
        let k = 1.0 / view.units_per_pixel(width, height);
        let _ = ctx.set_transform(
            k * dpr,
            0.0,
            0.0,
            k * dpr,
            dpr * (width / 2.0 - view.cx * k),
            dpr * (height / 2.0 - view.cy * k),
        );
        // room for rings and names
        let margin = DOT + 30.0;
        Self {
            k,
            x0: view.cx - width / 2.0 / k - margin,
            x1: view.cx + width / 2.0 / k + margin,
            y0: view.cy - height / 2.0 / k - margin,
            y1: view.cy + height / 2.0 / k + margin,
        }
    }

    fn seen(&self, x: f64, y: f64) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }

    fn segment_seen(&self, ax: f64, ay: f64, bx: f64, by: f64) -> bool {
        ax.max(bx) >= self.x0
            && ax.min(bx) <= self.x1
            && ay.max(by) >= self.y0
            && ay.min(by) <= self.y1
    }

    /// A line width in map units, never thinner than one pixel (sub-pixel
    /// lines cost twice as much to draw).
    fn line(&self, w: f64) -> f64 {
        w.max(1.0 / self.k)
    }
}

/// Draw the still part of a frame: links, flags, dots, selected and hovered
/// hosts, names.
/// Redrawn only when something changed.
///
/// Measured with 3000 hosts on a software canvas: what costs is pixels, not
/// calls (cached `Path2D`s gain nothing over culling what is out of view);
/// sub-pixel lines cost twice as much as 1 px ones, wide lines (links one
/// map unit wide at high zoom) even more, arcs three times as much as
/// rectangles. Hence: only what is in view, links always 1 px, tiny dots as
/// plain squares.
pub fn draw_hosts(ctx: &Ctx, width: f64, height: f64, dpr: f64, palette: &Palette, f: &Frame) {
    let vp = Viewport::setup(ctx, width, height, dpr, f.view);
    let hosts: Vec<_> = f
        .layout
        .hosts
        .iter()
        .filter(|p| vp.seen(p.x, p.y))
        .collect();
    let solid = dash(&[]);

    // D31 links: host -> each of its assignments' nodes, in the host's
    // colour for that assignment (D30). Grouped (default): one plain
    // ribbon per (cluster, assignment), its width the number of hosts (the
    // hosts show their colours); its statistics only when its assignment
    // is hovered or selected; a host's own links only on demand (hovered
    // host, a ribbon hovered or opened). Per host: every link (ribbons
    // only when zoomed far out, or for folded clusters).
    let tiny = DOT * vp.k < 3.0;
    if f.layout.links {
        let per_host = f.links == Links::PerHost;
        let fanned = fanned(f);
        let highlighted = |p: &crate::map::Placed, node: usize| {
            f.hovered == Some(p.host_id.as_str())
                || f.selected == Some(p.host_id.as_str())
                || fanned.contains(&(p.group, node))
        };
        let tones = [
            Tone::Ok,
            Tone::Bad,
            Tone::Warn,
            Tone::Unknown,
            Tone::Unassigned,
        ];
        if per_host && !tiny {
            ctx.set_line_width(1.0 / vp.k);
            ctx.set_global_alpha(0.35);
            for tone in tones {
                ctx.set_stroke_style_str(palette.tone(tone));
                ctx.begin_path();
                for p in &f.layout.hosts {
                    for &(node, t) in &p.links {
                        if t != tone {
                            continue;
                        }
                        let Some((nx, ny)) = f.layout.center(node) else {
                            continue;
                        };
                        let (ex, ey) = link_end(p.x, p.y, nx, ny);
                        if vp.segment_seen(p.x, p.y, ex, ey) {
                            ctx.move_to(p.x, p.y);
                            ctx.line_to(ex, ey);
                        }
                    }
                }
                ctx.stroke();
            }
            ctx.set_global_alpha(1.0);
        }
        // ribbons; something hovered: the others fade
        let focus = !f.focus.is_empty() || f.hovered_bundle.is_some();
        ctx.set_line_cap("butt");
        for (i, bundle) in f.layout.bundles.iter().enumerate() {
            if !ribbon_drawn(f, bundle, tiny, &fanned) {
                continue;
            }
            let Some(((cx, cy), (ex, ey))) = f.layout.bundle_ends(bundle) else {
                continue;
            };
            if !vp.segment_seen(cx, cy, ex, ey) {
                continue;
            }
            let total: usize = bundle.tones.iter().sum();
            if total == 0 {
                continue;
            }
            let strong = f.focus.contains(&bundle.node)
                || f.hovered_bundle == Some(i)
                || f.opened.contains(&i);
            let pixels = crate::map::ribbon_pixels(total);
            ctx.set_line_width((if strong { pixels + 1.0 } else { pixels }) / vp.k);
            ctx.set_global_alpha(match (strong, focus) {
                (true, _) => 0.55,
                (false, false) => 0.3,
                (false, true) => 0.1,
            });
            let colour = if strong { &palette.fg } else { &palette.muted };
            ctx.set_stroke_style_str(colour);
            // oriented: from the assignment, an arrow into its hosts'
            // outline (the assignment constrains these hosts)
            let (ux, uy, len) = unit(ex, ey, cx, cy);
            let head = (pixels * 1.1 + 7.0) / vp.k;
            let arrow = !tiny && len > 2.0 * head;
            let (bx, by) = if arrow {
                (cx - ux * head, cy - uy * head)
            } else {
                (cx, cy)
            };
            ctx.begin_path();
            ctx.move_to(ex, ey);
            ctx.line_to(bx, by);
            ctx.stroke();
            if arrow {
                ctx.set_fill_style_str(colour);
                ctx.begin_path();
                arrow_head(ctx, (cx, cy), (ux, uy), head, (pixels * 0.9 + 4.0) / vp.k);
                ctx.fill();
            }
        }
        ctx.set_global_alpha(1.0);
        // highlighted links (hovered / selected host, fanned ribbons),
        // drawn over the others, even when zoomed out; oriented like the
        // ribbons: a small arrow at the host
        ctx.set_line_width(vp.line(2.0 / vp.k).max(1.0 / vp.k));
        let (head, half) = (7.0 / vp.k, 3.5 / vp.k);
        for tone in tones {
            ctx.set_stroke_style_str(palette.tone(tone));
            ctx.set_fill_style_str(palette.tone(tone));
            let mut arrows = Vec::new();
            ctx.begin_path();
            for p in &f.layout.hosts {
                for &(node, t) in &p.links {
                    if t != tone || !highlighted(p, node) {
                        continue;
                    }
                    let Some((nx, ny)) = f.layout.center(node) else {
                        continue;
                    };
                    let (ex, ey) = link_end(p.x, p.y, nx, ny);
                    if vp.segment_seen(p.x, p.y, ex, ey) {
                        let (ux, uy, len) = unit(ex, ey, p.x, p.y);
                        let tip = (p.x - ux * (DOT + 1.0), p.y - uy * (DOT + 1.0));
                        let room = len > DOT + 1.0 + 2.0 * head && !tiny;
                        let to = if room {
                            arrows.push((tip, (ux, uy)));
                            (tip.0 - ux * head, tip.1 - uy * head)
                        } else {
                            (p.x, p.y)
                        };
                        ctx.move_to(ex, ey);
                        ctx.line_to(to.0, to.1);
                    }
                }
            }
            ctx.stroke();
            if !arrows.is_empty() {
                ctx.begin_path();
                for (tip, u) in arrows {
                    arrow_head(ctx, tip, u, head, half);
                }
                ctx.fill();
            }
        }
    }

    // flags (under the dots)
    if hosts.iter().any(|p| p.flag) {
        ctx.set_stroke_style_str(&palette.warn);
        ctx.set_line_width(vp.line(2.0));
        let _ = ctx.set_line_dash(&dash(&[3.0, 2.0]));
        ctx.begin_path();
        for p in hosts.iter().filter(|p| p.flag) {
            circle(ctx, p.x, p.y, DOT + 3.0);
        }
        ctx.stroke();
        let _ = ctx.set_line_dash(&solid);
    }
    // D28: proactive hosts whose repository is behind its regency source
    if hosts.iter().any(|p| p.behind) {
        ctx.set_stroke_style_str(palette.tone(Tone::Warn));
        ctx.set_line_width(vp.line(2.0));
        ctx.begin_path();
        for p in hosts.iter().filter(|p| p.behind) {
            diamond(ctx, p.x, p.y, DOT + 2.5);
        }
        ctx.stroke();
    }

    // dots, one path per colour (a job in progress overrides the
    // compliance colour); a few pixels wide (zoomed out), squares without
    // outline look the same
    ctx.set_stroke_style_str(&palette.canvas);
    ctx.set_line_width(1.5);
    let busy = |p: &crate::map::Placed| {
        f.rings.contains_key(&p.host_id) || f.flashes.contains_key(&p.host_id)
    };
    let fills = [
        Tone::Ok,
        Tone::Bad,
        Tone::Warn,
        Tone::Unknown,
        Tone::Unassigned,
    ]
    .map(Some)
    .into_iter()
    .chain([None]);
    for fill in fills {
        let mut of_fill = hosts
            .iter()
            .filter(|p| match fill {
                Some(tone) => p.tone == tone && !busy(p),
                None => busy(p),
            })
            .peekable();
        if of_fill.peek().is_none() {
            continue;
        }
        ctx.set_fill_style_str(fill.map_or(&palette.job, |tone| palette.tone(tone)));
        if tiny {
            for p in of_fill {
                ctx.fill_rect(p.x - DOT, p.y - DOT, 2.0 * DOT, 2.0 * DOT);
            }
        } else {
            ctx.begin_path();
            for p in of_fill {
                if p.proactive {
                    diamond(ctx, p.x, p.y, DOT);
                } else {
                    circle(ctx, p.x, p.y, DOT);
                }
            }
            ctx.fill();
            ctx.stroke();
        }
    }
    if let Some(id) = f.selected {
        // an outer ring, at least a few pixels wide even when zoomed out
        ctx.set_stroke_style_str(&palette.accent);
        ctx.set_line_width(vp.line(2.0 / vp.k).max(1.5));
        ctx.begin_path();
        for p in f.layout.placements(id) {
            circle(ctx, p.x, p.y, (DOT + 4.0).max(7.0 / vp.k));
        }
        ctx.stroke();
    }
    // picked (Ctrl + click, Shift + drag): a dashed ring
    if !f.picked.is_empty() {
        ctx.set_stroke_style_str(&palette.accent);
        ctx.set_line_width(vp.line(2.0 / vp.k).max(1.5));
        let _ = ctx.set_line_dash(&dash(&[3.0, 2.0]));
        ctx.begin_path();
        for p in f.picked.iter().filter_map(|i| f.layout.hosts.get(*i)) {
            if vp.seen(p.x, p.y) {
                circle(ctx, p.x, p.y, (DOT + 4.0).max(6.0 / vp.k));
            }
        }
        ctx.stroke();
        let _ = ctx.set_line_dash(&solid);
    }
    if let Some(id) = f.hovered {
        ctx.set_stroke_style_str(&palette.fg);
        ctx.set_line_width(vp.line(2.5));
        ctx.begin_path();
        for p in f.layout.placements(id) {
            circle(ctx, p.x, p.y, DOT);
        }
        ctx.stroke();
    }

    // D31: statistics of the ribbons of the hovered or selected
    // assignments, at their middle, over everything else
    if f.layout.links && f.links == Links::Grouped && !f.focus.is_empty() {
        for bundle in f
            .layout
            .bundles
            .iter()
            .filter(|b| f.focus.contains(&b.node))
        {
            let Some(((cx, cy), (ex, ey))) = f.layout.bundle_ends(bundle) else {
                continue;
            };
            let (mx, my) = ((cx + ex) / 2.0, (cy + ey) / 2.0);
            if vp.seen(mx, my) {
                ribbon_label(ctx, palette, vp.k, mx, my, &bundle.tones);
            }
        }
    }

    // names, in map units like the dots (they grow and shrink with them),
    // in full; shown once readable
    if f.names && NAME_FONT * vp.k >= 7.0 {
        ctx.set_fill_style_str(&palette.muted);
        ctx.set_font(&format!("{NAME_FONT}px system-ui, sans-serif"));
        ctx.set_text_align("center");
        for p in &hosts {
            let _ = ctx.fill_text(&p.host_id, p.x, p.y + DOT + 1.0 + NAME_FONT);
        }
    }
}

/// A ribbon's statistics: a pill at (`x`, `y`) with its hosts per state,
/// problems first, each a number in its state's colour after a dot of it; the same size in
/// pixels at every zoom (`k`: pixels per map unit).
fn ribbon_label(ctx: &Ctx, palette: &Palette, k: f64, x: f64, y: f64, tones: &[usize; 5]) {
    let parts = ribbon_parts(tones);
    if parts.is_empty() {
        return;
    }
    let px = 1.0 / k;
    let (dot, gap, sep, pad, height) = (3.5 * px, 4.0 * px, 10.0 * px, 8.0 * px, 20.0 * px);
    ctx.set_font(&format!("600 {}px system-ui, sans-serif", 11.0 * px));
    let widths: Vec<f64> = parts
        .iter()
        .map(|(_, text)| ctx.measure_text(text).map(|m| m.width()).unwrap_or(0.0))
        .collect();
    let width = widths.iter().map(|w| 2.0 * dot + gap + w).sum::<f64>()
        + sep * (parts.len() - 1) as f64
        + 2.0 * pad;
    let (left, top) = (x - width / 2.0, y - height / 2.0);
    let r = height / 2.0;
    ctx.begin_path();
    ctx.move_to(left + r, top);
    ctx.line_to(left + width - r, top);
    let _ = ctx.arc(left + width - r, y, r, -FRAC_PI_2, FRAC_PI_2);
    ctx.line_to(left + r, top + height);
    let _ = ctx.arc(left + r, y, r, FRAC_PI_2, 3.0 * FRAC_PI_2);
    ctx.close_path();
    ctx.set_global_alpha(0.92);
    ctx.set_fill_style_str(&palette.canvas);
    ctx.fill();
    ctx.set_global_alpha(1.0);
    ctx.set_line_width(px);
    ctx.set_stroke_style_str(&palette.muted);
    ctx.stroke();
    ctx.set_text_align("left");
    ctx.set_text_baseline("middle");
    let mut at = left + pad;
    for ((tone, text), w) in parts.iter().zip(widths) {
        ctx.set_fill_style_str(palette.tone(*tone));
        ctx.begin_path();
        circle(ctx, at + dot, y, dot);
        ctx.fill();
        at += 2.0 * dot + gap;
        let _ = ctx.fill_text(text, at, y + 0.5 * px);
        at += w + sep;
    }
    ctx.set_text_baseline("alphabetic");
}

/// What a ribbon's label says: its hosts per state, as bare numbers
/// (their colour says the state), problems first, states without hosts
/// left out.
pub fn ribbon_parts(tones: &[usize; 5]) -> Vec<(Tone, String)> {
    [
        Tone::Bad,
        Tone::Warn,
        Tone::Unknown,
        Tone::Ok,
        Tone::Unassigned,
    ]
    .into_iter()
    .filter_map(|tone| {
        let n = tones[tone.rank() as usize];
        (n > 0).then(|| (tone, n.to_string()))
    })
    .collect()
}

/// Call `render` on every animation frame until `alive` turns false.
pub fn animate(alive: Arc<AtomicBool>, mut render: impl FnMut(f64) + 'static) {
    type Callback = Closure<dyn FnMut(f64)>;
    let slot: Rc<RefCell<Option<Callback>>> = Rc::new(RefCell::new(None));
    let again = slot.clone();
    let request = |callback: &Callback| {
        if let Some(window) = web_sys::window() {
            let _ = window.request_animation_frame(callback.as_ref().unchecked_ref());
        }
    };
    *slot.borrow_mut() = Some(Closure::new(move |now: f64| {
        if !alive.load(Ordering::Relaxed) {
            // drops this closure: the loop ends
            let _ = again.borrow_mut().take();
            return;
        }
        render(now);
        if let Some(callback) = again.borrow().as_ref() {
            request(callback);
        }
    }));
    if let Some(callback) = slot.borrow().as_ref() {
        request(callback);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ribbon_labels_put_problems_first() {
        // indexed by rank: ok, bad, warn, unknown, unassigned
        let parts: Vec<String> = ribbon_parts(&[40, 3, 0, 2, 0])
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        assert_eq!(parts, ["3", "2", "40"]);
        assert!(ribbon_parts(&[0; 5]).is_empty());
    }

    #[test]
    fn view_maps_pixels_like_svg_meet() {
        // 1000 × 500 map units in a 400 × 400 element: width limits, 2.5 units/px
        let view = View {
            cx: 0.0,
            cy: 0.0,
            w: 1000.0,
            h: 500.0,
        };
        assert_eq!(view.units_per_pixel(400.0, 400.0), 2.5);
        assert_eq!(view.map_point(200.0, 200.0, 400.0, 400.0), (0.0, 0.0));
        assert_eq!(view.map_point(0.0, 0.0, 400.0, 400.0), (-500.0, -500.0));
    }
}
