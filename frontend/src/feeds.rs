//! App-wide live feeds, followed while the user is logged in, whatever the
//! page: every host (WatchHosts) and the jobs of the last hour (WatchJobs).
//! The masthead draws the fleet counts and the jobs-in-progress chart from
//! them; the Live view (map) and the Hosts page use them instead of opening
//! their own streams. A feed that ends (backend restart, network) is
//! restarted after `RETRY_MS`.

use std::collections::BTreeMap;
use std::time::Duration;

use leptos::prelude::*;

use crate::client::api;
use crate::pages::{host_state, watch_hosts, watch_jobs};

/// Jobs kept: those of the last hour (the chart's longest range), plus any
/// still in progress.
const JOBS_HOURS: u32 = 1;
const RETRY_MS: u64 = 5_000;

#[derive(Clone, Copy)]
pub struct Feeds {
    pub hosts: RwSignal<BTreeMap<String, api::HostSummary>>,
    /// The hosts' snapshot has arrived.
    pub ready: RwSignal<bool>,
    pub jobs: RwSignal<BTreeMap<String, api::JobSummary>>,
    /// Why a feed ended; cleared when it restarts.
    pub error: RwSignal<Option<String>>,
}

impl Feeds {
    pub fn new() -> Self {
        Self {
            hosts: RwSignal::new(BTreeMap::new()),
            ready: RwSignal::new(false),
            jobs: RwSignal::new(BTreeMap::new()),
            error: RwSignal::new(None),
        }
    }

    /// Hosts per state (`pages::host_state`), in state order.
    pub fn fleet(&self) -> Vec<(String, usize)> {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        self.hosts.with(|hosts| {
            for host in hosts.values() {
                *counts.entry(host_state(host).to_string()).or_default() += 1;
            }
        });
        counts.into_iter().collect()
    }
}

/// Runs the feeds while mounted (inside the logged-in part of the app).
#[component]
pub fn FeedsRunner() -> impl IntoView {
    let feeds = expect_context::<Feeds>();
    let generation = RwSignal::new(0u32);
    // a feed ended: start both again a little later (once, even when both
    // ended)
    let retry_pending = StoredValue::new(false);
    Effect::new(move |_| {
        if feeds.error.with(Option::is_some) && !retry_pending.get_value() {
            retry_pending.set_value(true);
            set_timeout(
                move || {
                    if generation.try_update(|g| *g += 1).is_some() {
                        retry_pending.set_value(false);
                        let _ = feeds.error.try_set(None);
                    }
                },
                Duration::from_millis(RETRY_MS),
            );
        }
    });
    // forget finished jobs once they are out of the chart's range
    if let Ok(handle) = set_interval_with_handle(
        move || {
            let horizon = js_sys::Date::now() as i64 - JOBS_HOURS as i64 * 3_600_000;
            let _ = feeds.jobs.try_update(|jobs| {
                jobs.retain(|_, j| crate::live::is_running(&j.state) || j.updated_at >= horizon)
            });
        },
        Duration::from_secs(60),
    ) {
        on_cleanup(move || handle.clear());
    }
    // each generation's streams belong to this closure's run: a new run
    // cancels the previous one's
    move || {
        generation.track();
        untrack(|| {
            feeds.hosts.set(BTreeMap::new());
            feeds.ready.set(false);
            feeds.jobs.set(BTreeMap::new());
            watch_hosts(
                api::HostSelector::default(),
                feeds.hosts,
                feeds.ready,
                feeds.error,
            );
            watch_jobs(JOBS_HOURS, feeds.jobs, feeds.error);
        });
    }
}
