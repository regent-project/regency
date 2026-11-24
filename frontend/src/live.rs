//! Live view of jobs (D24): what a running job does, as it happens
//! (`WatchLive`), and the log kept with a finished job (`GetRunLog`).
//!
//! Everything live is marked as such (red "LIVE" dot); a finished job's log is
//! marked "FINISHED", with a warning when it is incomplete (cropped to the
//! `run_log_max_bytes` setting, or updates lost while running).

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::Session;
use crate::client::api::{self, live_event, watch_live_request};
use crate::client::{authed, client, format_ms};
use crate::expected::job_label;
use crate::pages::{next_batch, spawn_scoped};

/// Entries kept per feed in the browser (the oldest go first).
const MAX_ENTRIES: usize = 3000;

/// A live feed: what `WatchLive` sent so far.
#[derive(Clone, Default, PartialEq)]
pub struct Feed {
    pub entries: Vec<api::LiveEntry>,
    /// Updates this view missed (server buffer limit, slow connection).
    pub lost: u64,
    /// The buffered past has arrived.
    pub ready: bool,
    pub error: Option<String>,
}

impl Feed {
    /// The attribute being handled, from the newest entry naming one.
    pub fn current_attribute(&self) -> Option<(i64, String)> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.attribute_index >= 0)
            .map(|e| (e.attribute_index, e.attribute.clone()))
    }
}

/// Follow `WatchLive` for `target` until the calling component is dropped.
pub fn watch_live(target: watch_live_request::Target) -> RwSignal<Feed> {
    let feed = RwSignal::new(Feed::default());
    let session = expect_context::<Session>();
    spawn_scoped(async move {
        let request = api::WatchLiveRequest {
            target: Some(target),
        };
        let mut stream = match client().watch_live(authed(request, &session.token())).await {
            Ok(response) => response.into_inner(),
            Err(status) => {
                session.check(&status);
                let _ = feed.try_update(|f| f.error = Some(status.message().to_string()));
                return;
            }
        };
        loop {
            let (batch, end) = next_batch(&mut stream).await;
            if !batch.is_empty() {
                let _ = feed.try_update(|f| {
                    for message in batch {
                        match message.event {
                            Some(live_event::Event::Entry(entry)) => f.entries.push(entry),
                            Some(live_event::Event::SnapshotDone(_)) => f.ready = true,
                            Some(live_event::Event::Lost(n)) => f.lost += n,
                            None => {}
                        }
                    }
                    if f.entries.len() > MAX_ENTRIES {
                        let excess = f.entries.len() - MAX_ENTRIES;
                        f.entries.drain(..excess);
                        f.lost += excess as u64;
                    }
                });
            }
            match end {
                None => {}
                Some(Ok(())) => return,
                Some(Err(status)) => {
                    session.check(&status);
                    let _ = feed.try_update(|f| f.error = Some(status.message().to_string()));
                    return;
                }
            }
        }
    });
    feed
}

/// `1 MiB`, `512 KiB`, `300 B`.
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// A remediation or outcome (SDK JSON) in one short line: its variant and
/// content, truncated.
fn short_json(text: &str) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Object(map)) if map.len() == 1 => {
            let (variant, content) = map.into_iter().next().expect("one entry");
            format!("{variant} {content}")
        }
        Ok(value) => value.to_string(),
        Err(_) => text.to_string(),
    };
    crate::map::truncate(&compact, 200)
}

/// A `remediation_finished` entry: the remediation was applied
/// (`{"Success": …}`), not `{"Failure": …}` and not missing (never tried).
pub fn remediation_succeeded(entry: &api::LiveEntry) -> bool {
    match serde_json::from_str::<serde_json::Value>(&entry.text) {
        Ok(serde_json::Value::Object(map)) => !map.contains_key("Failure"),
        Ok(_) => true,
        Err(_) => !entry.text.is_empty(),
    }
}

/// How one entry reads in a log: its CSS class and text.
pub fn describe(entry: &api::LiveEntry) -> (&'static str, String) {
    match entry.kind.as_str() {
        "attribute_started" => (
            "step attr",
            format!("▸ #{} {}", entry.attribute_index + 1, entry.attribute),
        ),
        "attribute_assessed" => match entry.verdict.as_str() {
            "compliant" => ("step ok", "  ✓ compliant".to_string()),
            "non_compliant" => (
                "step bad",
                format!(
                    "  ✗ not compliant: {} remediation{}",
                    entry.count,
                    if entry.count == 1 { "" } else { "s" }
                ),
            ),
            "non_compliant_fatal" => (
                "step bad",
                format!("  ✗ not compliant, cannot be fixed: {}", entry.text),
            ),
            "failed" => ("step bad", format!("  ✗ assessment failed: {}", entry.text)),
            other => ("step", format!("  ? {other}")),
        },
        "remediation_started" => (
            "step remediation",
            format!("  ⚙ {}", short_json(&entry.text)),
        ),
        "remediation_finished" if entry.text.is_empty() => (
            "step bad",
            "  ⚙ remediation could not be applied".to_string(),
        ),
        "remediation_finished" if !remediation_succeeded(entry) => (
            "step bad",
            format!("  ⚙ failed: {}", short_json(&entry.text)),
        ),
        "remediation_finished" => ("step", format!("  ⚙ done: {}", short_json(&entry.text))),
        "command_started" => ("cmd", format!("  $ {}", entry.text)),
        "output" if entry.stream == "stderr" => ("out err", format!("    {}", entry.text)),
        "output" => ("out", format!("    {}", entry.text)),
        "command_finished" => match entry.return_code {
            Some(0) => ("rc ok", "  ↳ exit 0".to_string()),
            Some(code) => ("rc bad", format!("  ↳ exit {code}")),
            None => ("rc bad", "  ↳ could not run".to_string()),
        },
        "aborted" => ("step warn", "■ aborted".to_string()),
        "dropped" => (
            "step warn",
            format!("… {} updates lost (live view overloaded)", entry.count),
        ),
        _ => ("step", "? (event from a newer regent-sdk)".to_string()),
    }
}

/// A log: one line per entry. `tail`: only the last lines, without a scroll
/// box (hover tip); otherwise a scroll box following new lines while the
/// reader is at its bottom.
#[component]
pub fn LogLines(
    #[prop(into)] entries: Signal<Vec<api::LiveEntry>>,
    #[prop(optional)] tail: Option<usize>,
) -> impl IntoView {
    let lines = move || {
        entries.with(|entries| {
            let start = tail.map_or(0, |n| entries.len().saturating_sub(n));
            entries[start..]
                .iter()
                .map(|e| {
                    let (class, text) = describe(e);
                    (format!("{}:{}", e.job_id, e.seq), class, text)
                })
                .collect::<Vec<_>>()
        })
    };
    let rows = move || {
        view! {
            <For each=lines key=|(key, _, _)| key.clone() let:line>
                <div class=line.1>{line.2}</div>
            </For>
        }
    };
    if tail.is_some() {
        return view! { <div class="live-log tail">{rows}</div> }.into_any();
    }
    let node = NodeRef::<leptos::html::Div>::new();
    let stick = RwSignal::new(true);
    Effect::new(move |_| {
        entries.track();
        if stick.get_untracked()
            && let Some(el) = node.get()
        {
            el.set_scroll_top(el.scroll_height());
        }
    });
    view! {
        <div class="live-log" node_ref=node on:scroll=move |_| {
            if let Some(el) = node.get_untracked() {
                stick.set(el.scroll_height() - el.scroll_top() - el.client_height() < 40);
            }
        }>
            {rows}
            <Show when=move || entries.with(Vec::is_empty)>
                <div class="muted">"Nothing yet."</div>
            </Show>
        </div>
    }
    .into_any()
}

/// The red "LIVE" mark.
pub fn live_mark() -> impl IntoView {
    view! { <span class="live-mark" title="Live: shown as it happens"><i></i>"LIVE"</span> }
}

/// The grey "FINISHED" mark.
pub fn finished_mark() -> impl IntoView {
    view! { <span class="finished-mark" title="Over: what was recorded">"FINISHED"</span> }
}

/// A running job, live: its mode and state, the attribute being handled,
/// then its log. `tail`: the last lines only (hover tip).
#[component]
pub fn LiveJob(job: api::JobSummary, #[prop(optional)] tail: Option<usize>) -> impl IntoView {
    let feed = watch_live(watch_live_request::Target::JobId(job.job_id.clone()));
    let entries = Signal::derive(move || feed.with(|f| f.entries.clone()));
    let current = move || {
        feed.with(|f| match f.current_attribute() {
            Some((index, name)) => format!("attribute #{} {}", index + 1, name),
            None if f.ready => "connecting / waiting for the host lock…".to_string(),
            None => "…".to_string(),
        })
    };
    view! {
        <div class="live-job" class:compact=tail.is_some()>
            <div class="live-head">
                {live_mark()}
                <b>{job_label(job.job)}</b>
                {crate::pages::badge(job.state.clone())}
                <span class="muted">{move || current()}</span>
            </div>
            <Show when=move || feed.with(|f| f.lost > 0)>
                <p class="warn">{move || format!("⚠ {} earlier updates not shown (buffer limit)", feed.with(|f| f.lost))}</p>
            </Show>
            <Show when=move || feed.with(|f| f.error.is_some())>
                <p class="error">{move || feed.with(|f| f.error.clone())}</p>
            </Show>
            {match tail {
                Some(n) => view! { <LogLines entries=entries tail=n /> }.into_any(),
                None => view! { <LogLines entries=entries /> }.into_any(),
            }}
        </div>
    }
}

/// Warnings of an incomplete log.
fn log_warnings(info: &api::RunLogInfo) -> Vec<String> {
    let mut warnings = Vec::new();
    if info.omitted_updates > 0 {
        warnings.push(format!(
            "⚠ Log cropped at {} (run_log_max_bytes setting): {} later update{} not kept.",
            format_bytes(info.limit_bytes),
            info.omitted_updates,
            if info.omitted_updates == 1 { "" } else { "s" }
        ));
    }
    if info.dropped_updates > 0 {
        warnings.push(format!(
            "⚠ {} update{} lost while the job ran (live view overloaded).",
            info.dropped_updates,
            if info.dropped_updates == 1 { "" } else { "s" }
        ));
    }
    warnings
}

/// Mark for a history row whose outcome kept a log (⚠ when incomplete).
pub fn log_badge(info: Option<&api::RunLogInfo>) -> impl IntoView + use<> {
    let info = info.cloned();
    info.map(|info| {
        let warnings = log_warnings(&info);
        let title = if warnings.is_empty() {
            format!("{} updates, {}", info.updates, format_bytes(info.size_bytes))
        } else {
            warnings.join(" ")
        };
        let cropped = !warnings.is_empty();
        view! { <span class="tag" class:warn=cropped title=title>{if cropped { "log ⚠" } else { "log" }}</span> }
    })
}

/// The log kept with a finished job's outcome. `finished_at`: shown in the
/// header when known.
#[component]
pub fn RunLogView(
    job_id: String,
    month: String,
    #[prop(optional)] finished_at: Option<i64>,
) -> impl IntoView {
    let session = expect_context::<Session>();
    let log = RwSignal::new(None::<Result<api::GetRunLogResponse, String>>);
    {
        let (job_id, month) = (job_id.clone(), month.clone());
        spawn_local(async move {
            let request = api::GetRunLogRequest { job_id, month };
            let result = match client()
                .get_run_log(authed(request, &session.token()))
                .await
            {
                Ok(response) => Ok(response.into_inner()),
                Err(status) => {
                    session.check(&status);
                    Err(status.message().to_string())
                }
            };
            let _ = log.try_set(Some(result));
        });
    }
    let entries = Signal::derive(move || {
        log.with(|l| match l {
            Some(Ok(response)) => response.entries.clone(),
            _ => Vec::new(),
        })
    });
    view! {
        <div class="run-log">
            <div class="live-head">
                {finished_mark()}
                <span class="muted">{finished_at.map(|at| format!("finished {}", format_ms(at)))}</span>
            </div>
            {move || log.with(|l| match l {
                None => view! { <p class="muted">"Loading the log…"</p> }.into_any(),
                Some(Err(e)) => view! { <p class="error">{e.clone()}</p> }.into_any(),
                Some(Ok(response)) if !response.found => view! {
                    <p class="muted">"No log kept for this job (run_log_max_bytes = 0, or produced by an older runner / regops)."</p>
                }.into_any(),
                Some(Ok(response)) => {
                    let warnings = response.info.as_ref().map(log_warnings).unwrap_or_default();
                    view! {
                        {warnings.into_iter().map(|w| view! { <p class="warn">{w}</p> }).collect::<Vec<_>>()}
                    }.into_any()
                }
            })}
            <Show when=move || log.with(|l| matches!(l, Some(Ok(r)) if r.found))>
                <LogLines entries=entries />
            </Show>
        </div>
    }
}

/// A finished job's log split per attribute (D24), to show next to each
/// attribute of a host's state what the job found about it.
#[derive(Clone, PartialEq, Default)]
pub struct LastRun {
    pub job: i32,
    pub finished_at: i64,
    /// Lines about no attribute (e.g. the run being aborted).
    pub general: Vec<api::LiveEntry>,
    /// Per attribute index of the run: its name there and its lines.
    pub attributes: std::collections::BTreeMap<i64, (String, Vec<api::LiveEntry>)>,
    pub aborted: bool,
    /// Every update of the run was kept (nothing cropped or dropped).
    pub complete: bool,
    /// Why the log is incomplete, if it is.
    pub warnings: Vec<String>,
}

impl LastRun {
    pub fn from_log(job: i32, finished_at: i64, log: &api::GetRunLogResponse) -> Self {
        let mut run = Self {
            job,
            finished_at,
            complete: true,
            ..Default::default()
        };
        if let Some(info) = &log.info {
            run.warnings = log_warnings(info);
            run.complete = run.warnings.is_empty();
        }
        for entry in &log.entries {
            match entry.kind.as_str() {
                "aborted" => run.aborted = true,
                "dropped" => run.complete = false,
                _ => {}
            }
            if entry.attribute_index < 0 {
                run.general.push(entry.clone());
            } else {
                run.attributes
                    .entry(entry.attribute_index)
                    .or_insert_with(|| (entry.attribute.clone(), Vec::new()))
                    .1
                    .push(entry.clone());
            }
        }
        run
    }

    /// What the run found about attribute `index`, whose name in a run is
    /// `name` (its `Name`, or its kind when unnamed): its outcome
    /// ([`attribute_outcome`]) and its lines. Found at the same place, or by
    /// its name elsewhere when the run checked an earlier state (attributes
    /// added or removed since moved the others); `not_checked` when the run
    /// did not have it. `None` when it cannot be told: several attributes of
    /// the run have that name (unnamed ones of one kind) and none is at its
    /// place.
    pub fn attribute(
        &self,
        index: usize,
        name: &str,
    ) -> Option<(&'static str, Vec<api::LiveEntry>)> {
        let found = match self.attributes.get(&(index as i64)) {
            Some((ran, entries)) if ran == name => Some(entries),
            _ => {
                let mut same = self.attributes.values().filter(|(ran, _)| ran == name);
                match (same.next(), same.next()) {
                    (Some((_, entries)), None) => Some(entries),
                    (Some(_), Some(_)) => return None,
                    _ => None,
                }
            }
        };
        match found {
            Some(entries) => Some((
                attribute_outcome(self.job, entries, self.aborted),
                entries.clone(),
            )),
            None if self.complete => Some(("not_checked", Vec::new())),
            None => Some(("unknown", Vec::new())),
        }
    }
}

/// One attribute's outcome in a run, from its log lines: `compliant`,
/// `fixed` (Reach), `not_compliant`, `fix_failed`, `cannot_fix`, `failed`
/// (the assessment itself), `aborted` or `unknown`.
pub fn attribute_outcome(job: i32, entries: &[api::LiveEntry], aborted: bool) -> &'static str {
    let reach = job == api::JobKind::Reach as i32;
    let verdict = entries
        .iter()
        .find(|e| e.kind == "attribute_assessed")
        .map(|e| e.verdict.as_str());
    let started = entries
        .iter()
        .filter(|e| e.kind == "remediation_started")
        .count();
    let finished = entries.iter().filter(|e| e.kind == "remediation_finished");
    let (done, refused) = finished.fold((0, 0), |(done, refused), e| {
        if remediation_succeeded(e) {
            (done + 1, refused)
        } else {
            (done, refused + 1)
        }
    });
    match verdict {
        None if aborted => "aborted",
        None => "unknown",
        Some("compliant") => "compliant",
        Some("failed") => "failed",
        Some("non_compliant_fatal") => "cannot_fix",
        Some("non_compliant") if !reach => "not_compliant",
        Some("non_compliant") if refused > 0 => "fix_failed",
        Some("non_compliant") if started > 0 && done == started => "fixed",
        Some("non_compliant") if aborted => "aborted",
        Some("non_compliant") => "not_compliant",
        Some(_) => "unknown",
    }
}

/// How an attribute outcome reads.
pub fn outcome_label(outcome: &str) -> &'static str {
    match outcome {
        "compliant" => "Compliant",
        "fixed" => "Fixed",
        "not_compliant" => "Not compliant",
        "fix_failed" => "Fix failed",
        "cannot_fix" => "Cannot be fixed",
        "failed" => "Check failed",
        "aborted" => "Aborted",
        "not_checked" => "Not checked",
        _ => "Unknown",
    }
}

/// Log lines, rendered once (a finished run's).
pub fn static_lines(entries: &[api::LiveEntry]) -> impl IntoView + use<> {
    let lines = entries
        .iter()
        .map(|e| {
            let (class, text) = describe(e);
            view! { <div class=class>{text}</div> }
        })
        .collect::<Vec<_>>();
    view! { <div class="live-log static">{lines}</div> }
}

/// Running jobs (newest first) of a host, from a jobs map.
pub fn running_jobs(
    jobs: &std::collections::BTreeMap<String, api::JobSummary>,
    host_id: &str,
) -> Vec<api::JobSummary> {
    let mut running: Vec<_> = jobs
        .values()
        .filter(|job| job.host_id == host_id && is_running(&job.state))
        .cloned()
        .collect();
    running.sort_by_key(|job| std::cmp::Reverse(job.created_at));
    running
}

pub fn is_running(state: &str) -> bool {
    matches!(state, "PENDING" | "STARTED" | "ABORT_REQUIRED")
}

/// A host's live section: one live feed per running job, or a note.
#[component]
pub fn HostLive(#[prop(into)] running: Signal<Vec<api::JobSummary>>) -> impl IntoView {
    view! {
        <For each=move || running.get() key=|job| job.job_id.clone() let:job>
            <LiveJob job=job />
        </For>
        <Show when=move || running.with(Vec::is_empty)>
            <p class="muted">"No job running on this host."</p>
        </Show>
    }
}

/// The hover tip of a map host: live when a job runs, else its last outcome.
#[component]
pub fn HostTip(host: api::HostSummary, running: Option<api::JobSummary>) -> impl IntoView {
    let state = crate::map::map_state(crate::pages::host_state(&host)).to_string();
    view! {
        <div class="tip-head"><b>{host.host_id.clone()}</b> " " {crate::pages::badge(state)}</div>
        {match running {
            Some(job) => view! { <LiveJob job=job tail=8 /> }.into_any(),
            None => view! {
                <div class="tip-finished">
                    <div class="live-head">
                        {finished_mark()}
                        <span class="muted">"last outcome " {format_ms(host.state_at)}</span>
                    </div>
                    {(!host.error.is_empty()).then(|| view! { <p class="error">{crate::map::truncate(&host.error, 200)}</p> })}
                    <p class="muted">"No job running. Click the host for its last known state."</p>
                </div>
            }.into_any(),
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str) -> api::LiveEntry {
        api::LiveEntry {
            job_id: "j".into(),
            kind: kind.into(),
            attribute_index: 2,
            attribute: "nginx".into(),
            ..Default::default()
        }
    }

    #[test]
    fn entries_read_as_a_log() {
        assert_eq!(describe(&entry("attribute_started")).1, "▸ #3 nginx");
        let mut line = entry("output");
        line.text = "hello".into();
        line.stream = "stderr".into();
        assert_eq!(describe(&line), ("out err", "    hello".to_string()));
        let mut done = entry("command_finished");
        assert_eq!(describe(&done).1, "  ↳ could not run");
        done.return_code = Some(3);
        assert_eq!(describe(&done), ("rc bad", "  ↳ exit 3".to_string()));
        let mut remediation = entry("remediation_started");
        remediation.text = r#"{"AptInstall":{"package":"nginx"}}"#.into();
        assert_eq!(
            describe(&remediation).1,
            r#"  ⚙ AptInstall {"package":"nginx"}"#
        );
        assert_eq!(
            describe(&entry("whatever")).1,
            "? (event from a newer regent-sdk)"
        );
    }

    #[test]
    fn bytes_and_warnings() {
        assert_eq!(format_bytes(1024 * 1024), "1 MiB");
        assert_eq!(format_bytes(1536 * 1024), "1.5 MiB");
        assert_eq!(format_bytes(2048), "2 KiB");
        let info = api::RunLogInfo {
            limit_bytes: 1024 * 1024,
            omitted_updates: 12,
            ..Default::default()
        };
        assert_eq!(
            log_warnings(&info),
            ["⚠ Log cropped at 1 MiB (run_log_max_bytes setting): 12 later updates not kept."]
        );
        assert!(log_warnings(&api::RunLogInfo::default()).is_empty());
    }

    #[test]
    fn feed_knows_the_current_attribute() {
        let mut feed = Feed::default();
        assert_eq!(feed.current_attribute(), None);
        feed.entries.push(entry("attribute_started"));
        let mut out = entry("output");
        out.attribute_index = -1;
        feed.entries.push(out);
        assert_eq!(feed.current_attribute(), Some((2, "nginx".into())));
    }

    fn at(kind: &str, index: i64, name: &str) -> api::LiveEntry {
        api::LiveEntry {
            kind: kind.into(),
            attribute_index: index,
            attribute: name.into(),
            ..Default::default()
        }
    }

    fn assessed(index: i64, name: &str, verdict: &str) -> api::LiveEntry {
        api::LiveEntry {
            verdict: verdict.into(),
            ..at("attribute_assessed", index, name)
        }
    }

    fn finished(index: i64, name: &str, text: &str) -> api::LiveEntry {
        api::LiveEntry {
            text: text.into(),
            ..at("remediation_finished", index, name)
        }
    }

    #[test]
    fn attributes_get_their_outcome_from_the_log() {
        let reach = api::JobKind::Reach as i32;
        let assess = api::JobKind::Assess as i32;
        let fixed = [
            assessed(0, "a", "non_compliant"),
            at("remediation_started", 0, "a"),
            finished(0, "a", "{}"),
        ];
        assert_eq!(attribute_outcome(reach, &fixed, false), "fixed");
        assert_eq!(
            attribute_outcome(assess, &fixed[..1], false),
            "not_compliant"
        );
        let refused = [
            assessed(0, "a", "non_compliant"),
            at("remediation_started", 0, "a"),
            finished(0, "a", ""),
        ];
        assert_eq!(attribute_outcome(reach, &refused, false), "fix_failed");
        // a remediation tried and failed: its outcome says so
        let failed = [
            assessed(0, "a", "non_compliant"),
            at("remediation_started", 0, "a"),
            finished(0, "a", r#"{"Failure":"Permission denied"}"#),
        ];
        assert_eq!(attribute_outcome(reach, &failed, false), "fix_failed");
        assert_eq!(
            describe(&failed[2]),
            (
                "step bad",
                r#"  ⚙ failed: Failure "Permission denied""#.to_string()
            )
        );
        let fixed_json = [
            assessed(0, "a", "non_compliant"),
            at("remediation_started", 0, "a"),
            finished(0, "a", r#"{"Success":null}"#),
        ];
        assert_eq!(attribute_outcome(reach, &fixed_json, false), "fixed");
        assert_eq!(attribute_outcome(reach, &refused[..2], true), "aborted");
        assert_eq!(
            attribute_outcome(assess, &[assessed(0, "a", "compliant")], false),
            "compliant"
        );
        assert_eq!(
            attribute_outcome(assess, &[assessed(0, "a", "failed")], false),
            "failed"
        );
        assert_eq!(
            attribute_outcome(assess, &[at("attribute_started", 0, "a")], true),
            "aborted"
        );

        let log = api::GetRunLogResponse {
            found: true,
            info: Some(api::RunLogInfo::default()),
            entries: vec![
                at("attribute_started", 0, "Apt"),
                assessed(0, "Apt", "compliant"),
                at("attribute_started", 1, "sshd"),
                assessed(1, "sshd", "non_compliant"),
            ],
        };
        let run = LastRun::from_log(assess, 1, &log);
        assert_eq!(run.attribute(0, "Apt").map(|r| r.0), Some("compliant"));
        assert_eq!(
            run.attribute(1, "sshd").map(|(o, lines)| (o, lines.len())),
            Some(("not_compliant", 2))
        );
        // an earlier state: found by name elsewhere, or not checked at all
        assert_eq!(run.attribute(3, "sshd").map(|r| r.0), Some("not_compliant"));
        assert_eq!(run.attribute(1, "nginx").map(|r| r.0), Some("not_checked"));
        // two unnamed attributes of one kind, neither at that place
        let mut twice = run.clone();
        twice.attributes.insert(5, ("Apt".into(), Vec::new()));
        assert_eq!(twice.attribute(4, "Apt"), None);
        assert_eq!(twice.attribute(5, "Apt").map(|r| r.0), Some("unknown"));
        assert_eq!(run.attribute(2, "x").map(|r| r.0), Some("not_checked"));
        let cropped = api::GetRunLogResponse {
            info: Some(api::RunLogInfo {
                omitted_updates: 3,
                limit_bytes: 10,
                ..Default::default()
            }),
            ..log
        };
        assert_eq!(
            LastRun::from_log(assess, 1, &cropped)
                .attribute(2, "x")
                .map(|r| r.0),
            Some("unknown")
        );
    }
}
