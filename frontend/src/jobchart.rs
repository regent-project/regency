//! Jobs in progress over time, in the map's left panel: pending and running
//! jobs over the last 2 min / 10 min / 30 min / 1 h. Client-side only: built
//! from the jobs the map already follows (WatchJobs, last hour), each job's
//! `states_json` giving when it became pending, started and ended, so the
//! whole last hour is there as soon as the page has loaded. The scale
//! follows what is shown (peak + 10 %, gridlines at round values); hovering
//! the chart shows the time slot and its values.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use leptos::prelude::*;

use js_sys::wasm_bindgen::JsCast;

use crate::client::{api, format_ms};

/// Time ranges offered, in minutes.
const WINDOWS: [(u32, &str); 4] = [(2, "2 min"), (10, "10 min"), (30, "30 min"), (60, "1 h")];
/// Points of the curve, whatever the range.
const BUCKETS: usize = 120;
const W: f64 = 280.0;
const H: f64 = 72.0;
const KEY: &str = "regency.map.jobs_window";

/// When a job was pending and running, in Unix milliseconds; `None` as an
/// end: still in that state.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Span {
    pub pending: Option<(i64, Option<i64>)>,
    pub running: Option<(i64, Option<i64>)>,
}

impl Span {
    /// From a job's `states_json` (`{"PENDING": "<RFC 3339>", …}`, D7).
    /// Pending lasts until it starts or ends (aborted or expired in the
    /// queue); running, from STARTED until a terminal state. ABORT_REQUIRED
    /// changes neither: the job is still pending or running.
    pub fn of(states_json: &str) -> Span {
        let states: BTreeMap<String, String> =
            serde_json::from_str(states_json).unwrap_or_default();
        let at = |state: &str| states.get(state).and_then(|t| parse_rfc3339_ms(t));
        let end = ["FINISHED", "ABORTED", "EXPIRED", "FAILED"]
            .into_iter()
            .filter_map(at)
            .min();
        let started = at("STARTED");
        let until = |start: i64, end: Option<i64>| (start, end.map(|e| e.max(start)));
        Span {
            pending: at("PENDING")
                .map(|start| until(start, [started, end].into_iter().flatten().min())),
            running: started.map(|start| until(start, end)),
        }
    }
}

/// Average number of pending and running jobs in each of `buckets` equal
/// slices of `[now - window, now]` (time-weighted: a job pending for half a
/// slice counts 0.5 there), so short jobs are never missed whatever the
/// range. Open spans last until `now`.
pub fn series<'a>(
    spans: impl Iterator<Item = &'a Span>,
    now: i64,
    window: i64,
    buckets: usize,
) -> (Vec<f64>, Vec<f64>) {
    let mut pending = Bucketed::new(now - window, window, buckets);
    let mut running = Bucketed::new(now - window, window, buckets);
    for span in spans {
        if let Some((start, end)) = span.pending {
            pending.add(start, end.unwrap_or(now).min(now));
        }
        if let Some((start, end)) = span.running {
            running.add(start, end.unwrap_or(now).min(now));
        }
    }
    (pending.values(), running.values())
}

/// Sum of overlaps per slice, with a difference array for the slices an
/// interval covers whole: linear in jobs + slices.
struct Bucketed {
    start: i64,
    slice: f64,
    partial: Vec<f64>,
    whole: Vec<f64>,
}

impl Bucketed {
    fn new(start: i64, window: i64, buckets: usize) -> Self {
        Bucketed {
            start,
            slice: window as f64 / buckets as f64,
            partial: vec![0.0; buckets],
            whole: vec![0.0; buckets + 1],
        }
    }

    fn add(&mut self, from: i64, to: i64) {
        let n = self.partial.len();
        let end = self.start as f64 + self.slice * n as f64;
        let from = (from as f64).max(self.start as f64);
        let to = (to as f64).min(end);
        if to <= from {
            return;
        }
        let a = (from - self.start as f64) / self.slice;
        let b = (to - self.start as f64) / self.slice;
        // a < n since from < end; b == n when the interval reaches the end
        let (ia, ib) = (a.floor() as usize, (b.floor() as usize).min(n));
        if ia == ib {
            self.partial[ia] += b - a;
            return;
        }
        self.partial[ia] += (ia + 1) as f64 - a;
        self.whole[ia + 1] += 1.0;
        self.whole[ib] -= 1.0;
        if ib < n {
            self.partial[ib] += b - ib as f64;
        }
    }

    fn values(self) -> Vec<f64> {
        let mut level = 0.0;
        self.partial
            .iter()
            .enumerate()
            .map(|(i, p)| {
                level += self.whole[i];
                p + level
            })
            .collect()
    }
}

/// `2026-11-08T20:41:14.018677Z` / `…+01:00` → Unix milliseconds (chrono's
/// RFC 3339 output; no date library in the client).
pub fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> { text.get(from..from + len)?.parse().ok() };
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, minute, second) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    let mut rest = &text[19..];
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        let ms: String = fraction[..digits]
            .chars()
            .chain("000".chars())
            .take(3)
            .collect();
        millis = ms.parse().ok()?;
        rest = &fraction[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hours: i64 = rest.get(1..3)?.parse().ok()?;
            let minutes: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (hours * 60 + minutes)
        }
    };
    // days from civil (Howard Hinnant's algorithm)
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset * 60;
    Some(seconds * 1000 + millis)
}

fn saved_window() -> u32 {
    crate::client::storage()
        .and_then(|s| s.get_item(KEY).ok().flatten())
        .and_then(|v| v.parse().ok())
        .filter(|m| WINDOWS.iter().any(|(w, _)| w == m))
        .unwrap_or(10)
}

/// Stacked areas (running at the bottom, pending above) of one series pair.
fn area_paths(pending: &[f64], running: &[f64], top: f64) -> (String, String) {
    let n = pending.len().max(1);
    let x = |i: usize| i as f64 * W / n as f64;
    let y = |v: f64| H - v / top * H;
    let mut low = format!("M0 {H}");
    let mut high = String::new();
    for i in 0..n {
        let (r, p) = (running[i], running[i] + pending[i]);
        low.push_str(&format!(
            " L{:.1} {:.1} L{:.1} {:.1}",
            x(i),
            y(r),
            x(i + 1),
            y(r)
        ));
        high.push_str(&format!(
            " L{:.1} {:.1} L{:.1} {:.1}",
            x(i),
            y(p),
            x(i + 1),
            y(p)
        ));
    }
    low.push_str(&format!(" L{W} {H} Z"));
    // pending: between the running line and the total
    let mut back = String::new();
    for i in (0..n).rev() {
        back.push_str(&format!(
            " L{:.1} {:.1} L{:.1} {:.1}",
            x(i + 1),
            y(running[i]),
            x(i),
            y(running[i])
        ));
    }
    (low, format!("M0 {H}{high}{back} Z"))
}

/// Top of the scale: the peak of what is shown plus 10 % headroom (at
/// least 1), so the curve always uses most of the height and the scale
/// follows the data as it changes.
fn scale_top(peak: f64) -> f64 {
    (peak * 1.1).max(1.0)
}

/// Gridlines below `top`: multiples of a round step (1, 2 or 5 × 10^n,
/// at least 1), up to four of them.
fn ticks(top: f64) -> Vec<f64> {
    let rough = top / 4.0;
    let base = 10f64.powf(rough.log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|m| m * base)
        .find(|v| *v >= rough)
        .unwrap_or(rough)
        .max(1.0);
    (1..)
        .map(|i| i as f64 * step)
        .take_while(|v| *v < top * 0.97)
        .collect()
}

/// A value of the chart: averages under 10 keep one decimal.
fn format_value(v: f64) -> String {
    if v >= 9.95 || v == 0.0 {
        format!("{}", v.round())
    } else {
        format!("{v:.1}")
    }
}

/// What the chart draws for one range.
#[derive(Clone, PartialEq)]
struct Chart {
    pending: Vec<f64>,
    running: Vec<f64>,
    top: f64,
    ticks: Vec<f64>,
    running_path: String,
    pending_path: String,
    /// start of the range and length of a slot, Unix milliseconds
    start: i64,
    slot: f64,
}

#[component]
pub fn JobChart(jobs: RwSignal<BTreeMap<String, api::JobSummary>>) -> impl IntoView {
    let window = RwSignal::new(saved_window());
    let now = RwSignal::new(js_sys::Date::now() as i64);
    if let Ok(handle) = set_interval_with_handle(
        move || now.set(js_sys::Date::now() as i64),
        Duration::from_secs(1),
    ) {
        on_cleanup(move || handle.clear());
    }
    // parsed spans, reparsed only for jobs whose document changed
    let cache = StoredValue::new(HashMap::<String, (i64, Span)>::new());
    let spans = Memo::new(move |_| {
        jobs.with(|jobs| {
            cache.update_value(|cache| {
                cache.retain(|id, _| jobs.contains_key(id));
                for (id, job) in jobs {
                    match cache.get(id) {
                        Some((at, _)) if *at == job.updated_at => {}
                        _ => {
                            cache.insert(id.clone(), (job.updated_at, Span::of(&job.states_json)));
                        }
                    }
                }
            });
            cache.with_value(|cache| cache.values().map(|(_, s)| *s).collect::<Vec<_>>())
        })
    });
    let chart = Memo::new(move |_| {
        let window_ms = window.get() as i64 * 60_000;
        let now = now.get();
        spans.with(|spans| {
            let (pending, running) = series(spans.iter(), now, window_ms, BUCKETS);
            let peak = pending
                .iter()
                .zip(&running)
                .map(|(p, r)| p + r)
                .fold(0.0, f64::max);
            let top = scale_top(peak);
            let (running_path, pending_path) = area_paths(&pending, &running, top);
            Chart {
                ticks: ticks(top),
                pending,
                running,
                top,
                running_path,
                pending_path,
                start: now - window_ms,
                slot: window_ms as f64 / BUCKETS as f64,
            }
        })
    });
    // slot under the cursor
    let hover = RwSignal::new(None::<usize>);
    let on_move = move |ev: web_sys::MouseEvent| {
        let Some(target) = ev
            .current_target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        let rect = target.get_bounding_client_rect();
        if rect.width() <= 0.0 {
            return;
        }
        let fraction = ((ev.client_x() as f64 - rect.left()) / rect.width()).clamp(0.0, 0.9999);
        hover.set(Some((fraction * BUCKETS as f64) as usize));
    };
    let tooltip = move || {
        let index = hover.get()?;
        chart.with(|c| {
            let (pending, running) = (*c.pending.get(index)?, *c.running.get(index)?);
            let from = c.start + (index as f64 * c.slot) as i64;
            let to = c.start + ((index + 1) as f64 * c.slot) as i64;
            let clock = |ms: i64| format_ms(ms).get(11..).unwrap_or_default().to_string();
            let left = (index as f64 + 0.5) / BUCKETS as f64 * 100.0;
            // keep the box inside the chart: right of the line, or left of it
            let side = if left < 55.0 {
                format!("left: calc({left:.2}% + 6px)")
            } else {
                format!("right: calc({:.2}% + 6px)", 100.0 - left)
            };
            Some(view! {
                <div class="cursor" style=format!("left: {left:.2}%")></div>
                <div class="tip" style=side>
                    <div class="muted">{format!("{} – {}", clock(from), clock(to))}</div>
                    <div><i class="pending"></i>"pending " <b>{format_value(pending)}</b></div>
                    <div><i class="running"></i>"running " <b>{format_value(running)}</b></div>
                </div>
            })
        })
    };
    let label = move || {
        let minutes = window.get();
        WINDOWS
            .iter()
            .find(|(m, _)| *m == minutes)
            .map(|(_, l)| *l)
            .unwrap_or("")
    };
    view! {
        <div class="job-chart">
            <div class="job-chart-head">
                <span class="windows">
                    {WINDOWS.iter().map(|(minutes, name)| {
                        let minutes = *minutes;
                        view! {
                            <button class="small" class:active=move || window.get() == minutes
                                title=format!("Last {name}")
                                on:click=move |_| {
                                    window.set(minutes);
                                    if let Some(storage) = crate::client::storage() {
                                        let _ = storage.set_item(KEY, &minutes.to_string());
                                    }
                                }>{*name}</button>
                        }
                    }).collect::<Vec<_>>()}
                </span>
            </div>
            <div class="job-chart-box" on:mousemove=on_move on:mouseleave=move |_| hover.set(None)>
                <svg viewBox=format!("0 0 {W} {H}") preserveAspectRatio="none" class="job-chart-plot"
                    role="img" aria-label=move || format!("Pending and running jobs, last {}", label())>
                    <path class="pending" d=move || chart.with(|c| c.pending_path.clone()) />
                    <path class="running" d=move || chart.with(|c| c.running_path.clone()) />
                    {move || chart.with(|c| c.ticks.iter().map(|t| {
                        let y = H - t / c.top * H;
                        view! { <line class="grid" x1="0" x2=W y1=y y2=y vector-effect="non-scaling-stroke" /> }
                    }).collect::<Vec<_>>())}
                </svg>
                // tick labels in HTML: SVG text would be stretched with the plot
                {move || chart.with(|c| c.ticks.iter().map(|t| view! {
                    <span class="tick" style=format!("bottom: {:.2}%", t / c.top * 100.0)>{format_value(*t)}</span>
                }).collect::<Vec<_>>())}
                {tooltip}
            </div>
            <div class="job-chart-axis muted">
                <span>{move || format!("−{}", label())}</span>
                <span class="chart-legend"><i class="pending"></i>"pending" <i class="running"></i>"running"</span>
                <span>"now"</span>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "2026-11-08T20:00:00Z";

    fn at(seconds: i64) -> i64 {
        parse_rfc3339_ms(T0).unwrap() + seconds * 1000
    }

    #[test]
    fn parses_chrono_timestamps() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2026-11-08T20:41:14.018677Z"),
            Some(1_794_170_474_018)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-11-08T21:41:14.5+01:00"),
            Some(1_794_170_474_500)
        );
        assert_eq!(
            parse_rfc3339_ms("2024-02-29T00:00:00Z"),
            Some(1_709_164_800_000)
        );
        assert_eq!(parse_rfc3339_ms("yesterday"), None);
    }

    #[test]
    fn spans_follow_the_lifecycle() {
        let done = Span::of(&format!(
            r#"{{"PENDING":"{T0}","STARTED":"2026-11-08T20:00:10Z","FINISHED":"2026-11-08T20:00:30Z"}}"#
        ));
        assert_eq!(done.pending, Some((at(0), Some(at(10)))));
        assert_eq!(done.running, Some((at(10), Some(at(30)))));
        // still queued; abort requested changes nothing
        let queued = Span::of(&format!(
            r#"{{"PENDING":"{T0}","ABORT_REQUIRED":"2026-11-08T20:00:05Z"}}"#
        ));
        assert_eq!(queued.pending, Some((at(0), None)));
        assert_eq!(queued.running, None);
        // expired in the queue: pending until then, never running
        let expired = Span::of(&format!(
            r#"{{"PENDING":"{T0}","EXPIRED":"2026-11-08T20:05:00Z"}}"#
        ));
        assert_eq!(expired.pending, Some((at(0), Some(at(300)))));
        assert_eq!(expired.running, None);
        // regops passes: only FINISHED, never in progress
        assert_eq!(
            Span::of(r#"{"FINISHED":"2026-11-08T20:00:00Z"}"#),
            Span::default()
        );
        // events recorded out of order: an end before the start is cut to it
        let odd = Span::of(&format!(
            r#"{{"PENDING":"2026-11-08T20:00:20Z","STARTED":"{T0}","FINISHED":"2026-11-08T20:00:10Z"}}"#
        ));
        assert_eq!(odd.pending, Some((at(20), Some(at(20)))));
        assert_eq!(odd.running, Some((at(0), Some(at(10)))));
    }

    #[test]
    fn series_are_time_weighted() {
        // 10 slices of 10 s over [0, 100 s]
        let now = at(100);
        let spans = [
            // pending 0–25 s, running 25–55 s
            Span {
                pending: Some((at(0), Some(at(25)))),
                running: Some((at(25), Some(at(55)))),
            },
            // running since 95 s, still running
            Span {
                pending: None,
                running: Some((at(95), None)),
            },
            // short job inside one slice: 2 s of 10
            Span {
                pending: Some((at(71), Some(at(73)))),
                running: None,
            },
            // before the window
            Span {
                pending: Some((at(-50), Some(at(-10)))),
                running: None,
            },
        ];
        let (pending, running) = series(spans.iter(), now, 100_000, 10);
        let close = |a: &[f64], b: &[f64]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9);
        assert!(
            close(
                &pending,
                &[1.0, 1.0, 0.5, 0.0, 0.0, 0.0, 0.0, 0.2, 0.0, 0.0]
            ),
            "{pending:?}"
        );
        assert!(
            close(
                &running,
                &[0.0, 0.0, 0.5, 1.0, 1.0, 0.5, 0.0, 0.0, 0.0, 0.5]
            ),
            "{running:?}"
        );
    }

    #[test]
    fn spans_across_the_whole_window_count_once_per_slice() {
        let spans = [Span {
            pending: Some((at(-1000), None)),
            running: None,
        }];
        let (pending, _) = series(spans.iter(), at(60), 60_000, 6);
        assert!(
            pending.iter().all(|v| (v - 1.0).abs() < 1e-9),
            "{pending:?}"
        );
    }

    #[test]
    fn scale_follows_the_data() {
        assert_eq!(scale_top(0.0), 1.0);
        assert!((scale_top(1941.0) - 2135.1).abs() < 1e-9);
        // gridlines at round values, below the top
        assert_eq!(ticks(scale_top(1941.0)), vec![1000.0, 2000.0]);
        assert_eq!(ticks(scale_top(9.0)), vec![5.0]);
        assert_eq!(ticks(scale_top(300.0)), vec![100.0, 200.0, 300.0]);
        assert_eq!(ticks(1.0), Vec::<f64>::new());
        assert_eq!(ticks(scale_top(2.0)), vec![1.0, 2.0]);
    }

    #[test]
    fn values_are_readable() {
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(0.42), "0.4");
        assert_eq!(format_value(9.96), "10");
        assert_eq!(format_value(9.94), "9.9");
        assert_eq!(format_value(953.4), "953");
    }
}
