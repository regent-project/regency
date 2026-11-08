//! Canvas renderer of the map's hosts and links. With thousands of hosts,
//! one SVG element per dot, link, name and job ring made every update and
//! every zoom step re-render tens of thousands of DOM nodes; the canvas
//! draws them in a few paths instead, and only what is in view: hosts and
//! links on one canvas, redrawn only when something changes. A host with a
//! job in progress is simply drawn in the job colour: no animation, so an
//! idle map costs nothing, however many jobs run.
//! Group cards (tens of them, with buttons) stay SVG, on top, sharing the
//! same view box; the areas of the areas display are drawn on the hosts'
//! canvas, under the dots.

use std::cell::RefCell;
use std::f64::consts::TAU;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use js_sys::wasm_bindgen::closure::Closure;
use js_sys::wasm_bindgen::{JsCast, JsValue};
use web_sys::CanvasRenderingContext2d as Ctx;

use crate::map::{DOT, Layout, Rings, Tone, truncate};

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

/// Links, opaque: #50626f at 45 % (dashed ones: 30 %) over `--canvas`.
const LINK: &str = "#323e47";
const EXTRA_LINK: &str = "#2a353d";
/// Areas: background and border (opaque).
const AREA: &str = "#1f2830";
const AREA_LINE: &str = "#3a4c59";
/// Links stop this far from a group's center, still under its card (a
/// 236 × 124 rectangle): shorter lines, not all meeting in one point.
const LINK_STOP: f64 = 58.0;

/// The point of segment `a`→`center` at `LINK_STOP` from `center`.
fn link_end(ax: f64, ay: f64, cx: f64, cy: f64) -> (f64, f64) {
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
    pub view: View,
    pub hovered: Option<&'a str>,
    /// The host shown in the right panel.
    pub selected: Option<&'a str>,
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

fn circle(ctx: &Ctx, x: f64, y: f64, r: f64) {
    ctx.move_to(x + r, y);
    let _ = ctx.arc(x, y, r, 0.0, TAU);
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

    // areas (areas display): a lighter background and a border, dashed
    // when folded
    let areas: Vec<_> = f
        .layout
        .areas
        .iter()
        .filter(|a| vp.segment_seen(a.x, a.y, a.x + a.w, a.y + a.h))
        .collect();
    if !areas.is_empty() {
        let radius = 12.0;
        let rounded = |a: &crate::map::Area| {
            let _ = ctx.round_rect_with_f64(a.x, a.y, a.w, a.h, radius);
        };
        ctx.set_fill_style_str(AREA);
        ctx.begin_path();
        for a in &areas {
            rounded(a);
        }
        ctx.fill();
        ctx.set_line_width(vp.line(1.5));
        ctx.set_stroke_style_str(AREA_LINE);
        for folded in [false, true] {
            let mut of_kind = areas.iter().filter(|a| a.folded == folded).peekable();
            if of_kind.peek().is_none() {
                continue;
            }
            if folded {
                let _ = ctx.set_line_dash(&dash(&[6.0 / vp.k, 4.0 / vp.k]));
            }
            ctx.begin_path();
            for a in of_kind {
                rounded(a);
            }
            ctx.stroke();
            let _ = ctx.set_line_dash(&solid);
        }
    }

    // links: host -> its group, then dashed links to its other inventories
    if f.layout.links {
        ctx.set_stroke_style_str(LINK);
        ctx.set_line_width(1.0 / vp.k);
        ctx.begin_path();
        for p in &f.layout.hosts {
            let (ex, ey) = link_end(p.x, p.y, p.gx, p.gy);
            if vp.segment_seen(p.x, p.y, ex, ey) {
                ctx.move_to(p.x, p.y);
                ctx.line_to(ex, ey);
            }
        }
        ctx.stroke();
    }
    if !f.layout.extra_links.is_empty() {
        ctx.set_stroke_style_str(EXTRA_LINK);
        let _ = ctx.set_line_dash(&dash(&[4.0 / vp.k, 4.0 / vp.k]));
        ctx.begin_path();
        for [ax, ay, bx, by] in &f.layout.extra_links {
            let (ex, ey) = link_end(*ax, *ay, *bx, *by);
            if vp.segment_seen(*ax, *ay, ex, ey) {
                ctx.move_to(*ax, *ay);
                ctx.line_to(ex, ey);
            }
        }
        ctx.stroke();
        let _ = ctx.set_line_dash(&solid);
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

    // dots, one path per colour (a job in progress overrides the
    // compliance colour); a few pixels wide (zoomed out), squares without
    // outline look the same
    let tiny = DOT * vp.k < 3.0;
    ctx.set_stroke_style_str(&palette.canvas);
    ctx.set_line_width(1.5);
    let busy = |p: &crate::map::Placed| f.rings.contains_key(&p.host_id);
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
                circle(ctx, p.x, p.y, DOT);
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
    if let Some(id) = f.hovered {
        ctx.set_stroke_style_str(&palette.fg);
        ctx.set_line_width(vp.line(2.5));
        ctx.begin_path();
        for p in f.layout.placements(id) {
            circle(ctx, p.x, p.y, DOT);
        }
        ctx.stroke();
    }

    // names, in map units like the dots (they grow and shrink with them),
    // cut to the space between two hosts (24 map units in both displays;
    // a character is about 0.61 em wide); shown once readable
    if f.names && NAME_FONT * vp.k >= 7.0 {
        let room = (24.0 / (NAME_FONT * 0.61)) as usize;
        ctx.set_fill_style_str(&palette.muted);
        ctx.set_font(&format!("{NAME_FONT}px system-ui, sans-serif"));
        ctx.set_text_align("center");
        for p in &hosts {
            let _ = ctx.fill_text(
                &truncate(&p.host_id, room),
                p.x,
                p.y + DOT + 1.0 + NAME_FONT,
            );
        }
    }
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
