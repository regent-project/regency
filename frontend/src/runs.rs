//! A run started from the Live view (Assess / Reach of an assignment):
//! the right panel following its jobs in real time, from the moment the
//! button is clicked (jobs of the run = jobs whose origin is its command
//! id, D10).

use std::collections::BTreeMap;

use leptos::prelude::*;

use crate::client::api;
use crate::client::format_ms;
use crate::expected::job_label;
use crate::pages::{badge, watch_jobs_of};

/// Where the run's command stands.
#[derive(Clone, PartialEq, Debug)]
pub enum RunStatus {
    /// Sent to backend, no answer yet.
    Sending,
    /// Accepted (published to the scheduler).
    Accepted,
    /// Refused by backend (the reason).
    Refused(String),
}

/// A run asked from the Live view.
#[derive(Clone)]
pub struct ActiveRun {
    pub command_id: String,
    pub job: api::JobKind,
    /// What it runs on, e.g. "assignment dataset/web".
    pub target: String,
    /// Hosts the map showed for it (the scheduler resolves them again).
    pub expected: usize,
    /// When it was asked (Unix ms).
    pub started_at: i64,
    pub status: RwSignal<RunStatus>,
}

/// The run's jobs counted by where they stand.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Progress {
    pub jobs: usize,
    pub pending: usize,
    /// Started, or abort requested.
    pub running: usize,
    /// Finished: compliant / reach success.
    pub ok: usize,
    /// Finished: not compliant, reach failed, error.
    pub bad: usize,
    /// Finished otherwise (aborted run, unknown), or never ran (aborted,
    /// expired, failed).
    pub other: usize,
}

impl Progress {
    pub fn of(jobs: &BTreeMap<String, api::JobSummary>) -> Self {
        let mut p = Progress {
            jobs: jobs.len(),
            ..Default::default()
        };
        for job in jobs.values() {
            match Stage::of(job) {
                Stage::Running => p.running += 1,
                Stage::Pending => p.pending += 1,
                Stage::Bad => p.bad += 1,
                Stage::Other => p.other += 1,
                Stage::Ok => p.ok += 1,
            }
        }
        p
    }

    pub fn done(&self) -> usize {
        self.ok + self.bad + self.other
    }

    /// Every job of the run has ended (none yet = not finished).
    pub fn finished(&self) -> bool {
        self.jobs > 0 && self.pending == 0 && self.running == 0
    }
}

/// Where one job stands, in the order the panel lists them (what needs
/// attention first).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Stage {
    Running,
    Pending,
    Bad,
    Other,
    Ok,
}

impl Stage {
    fn of(job: &api::JobSummary) -> Self {
        match job.state.as_str() {
            "PENDING" => Stage::Pending,
            "STARTED" | "ABORT_REQUIRED" => Stage::Running,
            "FINISHED" => match job.compliance.as_str() {
                "compliant" | "reach_success" => Stage::Ok,
                "not_compliant" | "reach_failed" | "error" => Stage::Bad,
                _ => Stage::Other,
            },
            _ => Stage::Other,
        }
    }
}

/// Jobs listed in the panel at most (the counts cover them all).
const LISTED: usize = 200;

/// The run's jobs in panel order: running, pending, failing, other, fine;
/// then by host.
fn ordered(jobs: &BTreeMap<String, api::JobSummary>) -> Vec<api::JobSummary> {
    let mut list: Vec<&api::JobSummary> = jobs.values().collect();
    list.sort_by(|a, b| {
        Stage::of(a)
            .cmp(&Stage::of(b))
            .then_with(|| a.host_id.cmp(&b.host_id))
    });
    list.into_iter().cloned().collect()
}

/// A share of the progress bar, in percent.
fn pct(n: usize, total: usize) -> String {
    format!("width: {:.3}%", n as f64 * 100.0 / total.max(1) as f64)
}

/// The right panel of a run: its status, progress and jobs, live.
#[component]
pub fn RunPanel(
    run: ActiveRun,
    on_close: Callback<()>,
    on_host: Callback<String>,
) -> impl IntoView {
    let jobs = RwSignal::new(BTreeMap::<String, api::JobSummary>::new());
    let error = RwSignal::new(None::<String>);
    watch_jobs_of(
        Some(api::watch_jobs_request::Filter::Origin(
            run.command_id.clone(),
        )),
        1,
        jobs,
        error,
    );
    let progress = Memo::new(move |_| jobs.with(Progress::of));
    let status = run.status;
    let expected = run.expected;
    let title = format!("{} {}", job_label(run.job as i32), run.target);
    let summary = move || {
        let p = progress.get();
        if p.jobs == 0 {
            return match status.get() {
                RunStatus::Sending => "Sending the command…".to_string(),
                RunStatus::Accepted => format!(
                    "Accepted, waiting for the scheduler to create the jobs ({expected} host{} on the map)…",
                    if expected == 1 { "" } else { "s" }
                ),
                RunStatus::Refused(_) => String::new(),
            };
        }
        format!(
            "{} of {} job{} done",
            p.done(),
            p.jobs,
            if p.jobs == 1 { "" } else { "s" }
        )
    };
    view! {
        <div class="host-head">
            <h2>{title}</h2>
            <button title="Close" aria-label="Close" on:click=move |_| on_close.run(())>"×"</button>
        </div>
        <div class="run-head">
            {move || match status.get() {
                RunStatus::Refused(_) => view! { <span class="badge error">"Refused"</span> }.into_any(),
                _ if progress.with(Progress::finished) => view! { <span class="finished-mark">"FINISHED"</span> }.into_any(),
                _ => view! { <span class="live-mark"><i></i>"LIVE"</span> }.into_any(),
            }}
            <span class="muted">"started " <span class="ts">{format_ms(run.started_at)}</span></span>
        </div>
        {move || match status.get() {
            RunStatus::Refused(reason) => Some(view! { <p class="error">{reason}</p> }),
            _ => None,
        }}
        <p class="run-summary">{summary}</p>
        // progress: fine, failing, other, running, pending (left to right)
        <div class="run-bar" role="img" aria-label=summary>
            {move || {
                let p = progress.get();
                let total = p.jobs;
                view! {
                    <span class="ok" style=pct(p.ok, total)></span>
                    <span class="bad" style=pct(p.bad, total)></span>
                    <span class="other" style=pct(p.other, total)></span>
                    <span class="running" style=pct(p.running, total)></span>
                    <span class="pending" style=pct(p.pending, total)></span>
                }
            }}
        </div>
        <ul class="run-counts">
            {move || {
                let p = progress.get();
                [
                    ("running", "Running", p.running),
                    ("pending", "Pending", p.pending),
                    ("ok", if run.job == api::JobKind::Reach { "Success" } else { "Compliant" }, p.ok),
                    ("bad", if run.job == api::JobKind::Reach { "Failed" } else { "Not compliant" }, p.bad),
                    ("other", "Other", p.other),
                ]
                .into_iter()
                .filter(|(class, _, n)| *n > 0 || matches!(*class, "ok" | "bad"))
                .map(|(class, label, n)| view! {
                    <li><i class=class></i>{label}<b>{n}</b></li>
                })
                .collect::<Vec<_>>()
            }}
        </ul>
        <Show when=move || error.get().is_some()>
            <p class="error">{move || error.get()}</p>
        </Show>
        <Show when=move || progress.with(|p| p.jobs > 0)>
            <h3>"Hosts"</h3>
            <table class="run-jobs">
                <tbody>
                    {move || jobs.with(ordered).into_iter().take(LISTED).map(|job| {
                        let host = job.host_id.clone();
                        let state = job.state.clone();
                        let result = (state == "FINISHED" && !job.compliance.is_empty()).then(|| job.compliance.clone());
                        view! {
                            <tr>
                                <td><a href="#" title="Show this host" on:click=move |ev| {
                                    ev.prevent_default();
                                    on_host.run(host.clone());
                                }>{job.host_id.clone()}</a></td>
                                <td>{match result {
                                    Some(result) => badge(result).into_any(),
                                    None => badge(state).into_any(),
                                }}</td>
                            </tr>
                        }
                    }).collect::<Vec<_>>()}
                </tbody>
            </table>
            {move || {
                let n = progress.with(|p| p.jobs);
                (n > LISTED).then(|| view! { <p class="muted">{format!("and {} more (counted above)", n - LISTED)}</p> })
            }}
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(host: &str, state: &str, compliance: &str) -> (String, api::JobSummary) {
        let job = api::JobSummary {
            job_id: format!("run:c:{host}"),
            host_id: host.into(),
            state: state.into(),
            compliance: compliance.into(),
            ..Default::default()
        };
        (job.job_id.clone(), job)
    }

    #[test]
    fn progress_counts_and_order() {
        let jobs: BTreeMap<_, _> = [
            job("a", "FINISHED", "compliant"),
            job("b", "FINISHED", "not_compliant"),
            job("c", "STARTED", ""),
            job("d", "PENDING", ""),
            job("e", "EXPIRED", ""),
            job("f", "FINISHED", "reach_success"),
        ]
        .into_iter()
        .collect();
        let p = Progress::of(&jobs);
        assert_eq!(
            p,
            Progress {
                jobs: 6,
                pending: 1,
                running: 1,
                ok: 2,
                bad: 1,
                other: 1
            }
        );
        assert_eq!(p.done(), 4);
        assert!(!p.finished());
        let hosts: Vec<String> = ordered(&jobs).into_iter().map(|j| j.host_id).collect();
        assert_eq!(hosts, ["c", "d", "b", "e", "a", "f"]);
    }

    #[test]
    fn finished_once_every_job_ended() {
        assert!(!Progress::of(&BTreeMap::new()).finished());
        let jobs: BTreeMap<_, _> = [job("a", "FINISHED", "compliant"), job("b", "ABORTED", "")]
            .into_iter()
            .collect();
        assert!(Progress::of(&jobs).finished());
    }
}
