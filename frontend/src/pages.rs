//! Pages: login, live hosts, host detail (with its expected state explorer),
//! live jobs.

use std::collections::BTreeMap;

use futures::future::{AbortHandle, Abortable};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_params_map, use_query_map};

use crate::Session;
use crate::client::api::{self, host_event, job_event};
use crate::client::{authed, client, command_id, format_ms, save_token};
use crate::expected::{HostStateExplorer, job_label};

/// Run `task` until the component is dropped.
pub fn spawn_scoped(task: impl std::future::Future<Output = ()> + 'static) {
    let (abort, registration) = AbortHandle::new_pair();
    on_cleanup(move || abort.abort());
    spawn_local(async move {
        let _ = Abortable::new(task, registration).await;
    });
}

/// What a host's colour means (D23): its compliance against its effective
/// expected state, or why there is none to trust: `unassigned` (no
/// assignment), `invalid` (assignments that cannot be composed),
/// `outdated` (checked against an older effective state).
pub fn host_state(host: &api::HostSummary) -> &str {
    if host.assignments.is_empty() {
        "unassigned"
    } else if !host.assignment_error.is_empty() {
        "invalid"
    } else if host.compliance != "unknown" && host.state_revision != host.effective_revision {
        "outdated"
    } else {
        &host.compliance
    }
}

/// The host's assignments, lowest priority first, and why they fail.
pub fn assignments_line(host: &api::HostSummary) -> impl IntoView + use<> {
    let text = if host.assignments.is_empty() {
        "none: nothing is checked on this host".to_string()
    } else {
        host.assignments.join(" → ")
    };
    let error = host.assignment_error.clone();
    view! {
        <span>{text}</span>
        {(!error.is_empty()).then(|| view! { <div class="error">{error}</div> })}
    }
}

pub fn badge(state: String) -> impl IntoView {
    let class = format!("badge {state}");
    view! { <span class=class>{state.replace('_', " ")}</span> }
}

#[component]
pub fn Login() -> impl IntoView {
    let session = expect_context::<Session>();
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let submit = move || {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let request = api::LoginRequest {
                username: username.get_untracked(),
                password: password.get_untracked(),
            };
            match client().login(request).await {
                Ok(response) => {
                    let token = response.into_inner().token;
                    save_token(Some(&token));
                    session.token.set(Some(token));
                }
                Err(status) => error.set(Some(status.message().to_string())),
            }
            busy.set(false);
        });
    };
    view! {
        <form class="login" on:submit=move |ev| { ev.prevent_default(); submit(); }>
            <img src="/regent-logo.png" alt="" />
            <h1>"regency"</h1>
            <input placeholder="Username" autocomplete="username"
                prop:value=move || username.get() on:input=move |ev| username.set(event_target_value(&ev)) />
            <input placeholder="Password" type="password" autocomplete="current-password"
                prop:value=move || password.get() on:input=move |ev| password.set(event_target_value(&ev)) />
            <button class="primary" type="submit" disabled=move || busy.get()>"Log in"</button>
            <Show when=move || error.get().is_some()>
                <p class="error">{move || error.get()}</p>
            </Show>
        </form>
    }
}

#[component]
pub fn WhoAmI() -> impl IntoView {
    let session = expect_context::<Session>();
    let principal = RwSignal::new(String::new());
    spawn_scoped(async move {
        match client()
            .who_am_i(authed(api::WhoAmIRequest {}, &session.token()))
            .await
        {
            Ok(me) => {
                let _ = principal.try_set(me.into_inner().principal);
            }
            Err(status) => session.check(&status),
        }
    });
    view! { <span class="muted">{move || principal.get()}</span> }
}

/// Live updates are applied in batches: the first message of a batch, then
/// whatever else arrives within this delay. Thousands of hosts (the initial
/// snapshot, a recurring order firing on a whole inventory) then cost a few
/// signal updates instead of one per message.
const BATCH_MS: i32 = 150;

/// Resolves after `ms` milliseconds.
pub async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// The next batch of a stream's messages (see [`BATCH_MS`]), and how the
/// stream ended if it did: `Some(Ok(()))` closed, `Some(Err(_))` failed.
pub(crate) async fn next_batch<T>(
    stream: &mut tonic::Streaming<T>,
) -> (Vec<T>, Option<Result<(), tonic::Status>>) {
    use futures::StreamExt;
    use futures::future::{Either, select};
    let mut batch = Vec::new();
    match stream.next().await {
        Some(Ok(message)) => batch.push(message),
        Some(Err(status)) => return (batch, Some(Err(status))),
        None => return (batch, Some(Ok(()))),
    }
    let mut timer = Box::pin(sleep_ms(BATCH_MS));
    loop {
        // `Streaming` keeps its state in itself: dropping `next()` loses nothing
        match select(stream.next(), timer.as_mut()).await {
            Either::Left((Some(Ok(message)), _)) => batch.push(message),
            Either::Left((Some(Err(status)), _)) => return (batch, Some(Err(status))),
            Either::Left((None, _)) => return (batch, Some(Ok(()))),
            Either::Right(_) => return (batch, None),
        }
    }
}

/// Follow WatchHosts into `hosts`.
pub fn watch_hosts(
    selector: api::HostSelector,
    hosts: RwSignal<BTreeMap<String, api::HostSummary>>,
    ready: RwSignal<bool>,
    error: RwSignal<Option<String>>,
) {
    let session = expect_context::<Session>();
    spawn_scoped(async move {
        let request = api::WatchHostsRequest {
            selector: Some(selector),
        };
        let mut stream = match client()
            .watch_hosts(authed(request, &session.token()))
            .await
        {
            Ok(response) => response.into_inner(),
            Err(status) => {
                session.check(&status);
                let _ = error.try_set(Some(status.message().to_string()));
                return;
            }
        };
        loop {
            let (batch, end) = next_batch(&mut stream).await;
            let mut snapshot_done = false;
            let changes: Vec<_> = batch
                .into_iter()
                .filter_map(|message| match message.event? {
                    host_event::Event::SnapshotDone(_) => {
                        snapshot_done = true;
                        None
                    }
                    event => Some(event),
                })
                .collect();
            if !changes.is_empty() {
                let _ = hosts.try_update(|map| {
                    for event in changes {
                        match event {
                            host_event::Event::Upsert(host) => {
                                map.insert(host.host_id.clone(), host);
                            }
                            host_event::Event::Removed(host_id) => {
                                map.remove(&host_id);
                            }
                            host_event::Event::SnapshotDone(_) => {}
                        }
                    }
                });
            }
            if snapshot_done {
                let _ = ready.try_set(true);
            }
            match end {
                None => {}
                Some(Ok(())) => {
                    let _ = error.try_set(Some(
                        "live view closed by the server, reload the page".into(),
                    ));
                    return;
                }
                Some(Err(status)) => {
                    session.check(&status);
                    let _ = error.try_set(Some(status.message().to_string()));
                    return;
                }
            }
        }
    });
}

#[component]
pub fn Hosts() -> impl IntoView {
    // every host: the app-wide feed (crate::feeds)
    let crate::feeds::Feeds {
        hosts,
        ready,
        error,
        ..
    } = expect_context::<crate::feeds::Feeds>();
    let filter = RwSignal::new(String::new());
    // `?state=<state>`: hosts in exactly that state (`host_state`), e.g. from
    // the masthead's fleet counts
    let query = use_query_map();
    let state = Memo::new(move |_| query.with(|q| q.get("state")).filter(|s| !s.is_empty()));

    let visible = move || {
        let needle = filter.get().to_lowercase();
        let state = state.get();
        hosts.with(|hosts| {
            hosts
                .values()
                .filter(|h| state.as_deref().is_none_or(|s| host_state(h) == s))
                .filter(|h| {
                    needle.is_empty()
                        || h.host_id.to_lowercase().contains(&needle)
                        || host_state(h).contains(&needle)
                        || h.labels
                            .iter()
                            .any(|(k, v)| format!("{k}={v}").contains(&needle))
                        || h.inventories.iter().any(|i| i.contains(&needle))
                })
                .cloned()
                .collect::<Vec<_>>()
        })
    };
    let counts = move || {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        hosts.with(|hosts| {
            for host in hosts.values() {
                *counts.entry(host_state(host).to_string()).or_default() += 1;
            }
        });
        counts.into_iter().collect::<Vec<_>>()
    };
    view! {
        <h1>"Hosts"</h1>
        <div class="counts">
            <For each=counts key=|c| c.clone() let:count>
                {badge(count.0.clone())} <span>{count.1}</span>
            </For>
        </div>
        <div class="toolbar">
            {move || state.get().map(|s| view! {
                <span class="filter-chip">"State: " {badge(s)}
                    <a href="/hosts" title="Show every state" aria-label="Remove the state filter">"×"</a>
                </span>
            })}
            <input placeholder="Filter: host, state, label=value, inventory" style="min-width: 320px"
                prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev)) />
            <span class="muted">{move || if !ready.get() {
                "loading…".to_string()
            } else if state.with(Option::is_some) || !filter.with(String::is_empty) {
                format!("{} of {} hosts, live", visible().len(), hosts.with(|h| h.len()))
            } else {
                format!("{} hosts, live", hosts.with(|h| h.len()))
            }}</span>
        </div>
        <Show when=move || error.get().is_some()>
            <p class="error">{move || error.get()}</p>
        </Show>
        <table>
            <thead><tr><th>"Host"</th><th>"State"</th><th>"Last outcome"</th><th>"Last seen"</th><th>"Labels"</th><th>"Inventories"</th></tr></thead>
            <tbody>
                <For each=visible key=|h| (h.host_id.clone(), host_state(h).to_string(), h.state_at, h.last_seen, h.labels.len()) let:host>
                    <tr>
                        <td><a href=format!("/hosts/{}", host.host_id)>{host.host_id.clone()}</a>
                            {host.conflicting.then(|| view! { " " <span class="badge error">"conflicting definitions"</span> })}
                            {(!host.pending_fingerprints.is_empty()).then(|| view! { " " <span class="badge aborted">"certificate pending"</span> })}
                        </td>
                        <td>{badge(host_state(&host).to_string())}
                            {(!host.error.is_empty()).then(|| view! { <div class="muted">{host.error.clone()}</div> })}</td>
                        <td>{format_ms(host.state_at)}</td>
                        <td>{format_ms(host.last_seen)}</td>
                        <td>{host.labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")}</td>
                        <td>{host.inventories.join(", ")}</td>
                    </tr>
                </For>
            </tbody>
        </table>
    }
}

#[component]
pub fn HostPage() -> impl IntoView {
    let session = expect_context::<Session>();
    let params = use_params_map();
    let host_id = params.read_untracked().get("id").unwrap_or_default();
    let hosts = RwSignal::new(BTreeMap::<String, api::HostSummary>::new());
    let ready = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    watch_hosts(
        api::HostSelector {
            hosts: vec![host_id.clone()],
            ..Default::default()
        },
        hosts,
        ready,
        error,
    );
    let summary = Memo::new({
        let host_id = host_id.clone();
        move |_| hosts.with(|hosts| hosts.get(&host_id).cloned())
    });

    // D24: the host's running jobs, live
    let host_jobs = RwSignal::new(BTreeMap::<String, api::JobSummary>::new());
    watch_jobs_of(
        Some(api::watch_jobs_request::Filter::Hosts(api::HostSelector {
            hosts: vec![host_id.clone()],
            ..Default::default()
        })),
        24,
        host_jobs,
        error,
    );
    let running = Signal::derive({
        let host_id = host_id.clone();
        move || host_jobs.with(|jobs| crate::live::running_jobs(jobs, &host_id))
    });
    // history rows whose log is open (D24)
    let logs_open = RwSignal::new(std::collections::BTreeSet::<String>::new());

    // history
    let outcomes = RwSignal::new(Vec::<api::OutcomeSummary>::new());
    // expanded history rows: job id -> its outcome (None while loading),
    // kept when collapsed so reopening needs no request
    let expanded = RwSignal::new(BTreeMap::<String, Option<String>>::new());
    let fetched = RwSignal::new(BTreeMap::<String, String>::new());
    let load_history = {
        let host_id = host_id.clone();
        move || {
            let host_id = host_id.clone();
            spawn_local(async move {
                let request = api::ListOutcomesRequest {
                    filter: Some(api::list_outcomes_request::Filter::HostId(host_id)),
                    page_size: 50,
                    page_token: String::new(),
                };
                match client()
                    .list_outcomes(authed(request, &session.token()))
                    .await
                {
                    Ok(response) => {
                        let _ = outcomes.try_set(response.into_inner().outcomes);
                    }
                    Err(status) => {
                        session.check(&status);
                        let _ = error.try_set(Some(status.message().to_string()));
                    }
                }
            });
        }
    };
    load_history();
    let toggle_outcome = move |job_id: String, month: String| {
        if expanded.with_untracked(|e| e.contains_key(&job_id)) {
            expanded.update(|e| {
                e.remove(&job_id);
            });
            return;
        }
        if let Some(json) = fetched.with_untracked(|f| f.get(&job_id).cloned()) {
            expanded.update(|e| {
                e.insert(job_id, Some(json));
            });
            return;
        }
        expanded.update(|e| {
            e.insert(job_id.clone(), None);
        });
        spawn_local(async move {
            let key = job_id.clone();
            match client()
                .get_outcome(authed(
                    api::GetOutcomeRequest { job_id, month },
                    &session.token(),
                ))
                .await
            {
                Ok(response) => {
                    let json = response.into_inner().result_json;
                    let pretty = serde_json::from_str::<serde_json::Value>(&json)
                        .and_then(|v| serde_json::to_string_pretty(&v))
                        .unwrap_or(json);
                    let _ = fetched.try_update(|f| {
                        f.insert(key.clone(), pretty.clone());
                    });
                    // only if still open (the user may have collapsed it)
                    let _ = expanded.try_update(|e| {
                        if let Some(slot) = e.get_mut(&key) {
                            *slot = Some(pretty);
                        }
                    });
                }
                Err(status) => {
                    let _ = expanded.try_update(|e| {
                        e.remove(&key);
                    });
                    let _ = error.try_set(Some(status.message().to_string()));
                }
            }
        });
    };

    let message = RwSignal::new(None::<String>);
    let approve = {
        let host_id = host_id.clone();
        move |fingerprint: String| {
            let host_id = host_id.clone();
            spawn_local(async move {
                let request = api::ApproveCertificateRequest {
                    command_id: command_id("approve"),
                    host_id,
                    fingerprint,
                };
                let text = match client()
                    .approve_certificate(authed(request, &session.token()))
                    .await
                {
                    Ok(_) => "certificate approval accepted".to_string(),
                    Err(status) => format!("refused: {}", status.message()),
                };
                let _ = message.try_set(Some(text));
            });
        }
    };
    // D8: manual deletion of the host and its jobs/outcomes
    let delete_host = {
        let host_id = host_id.clone();
        move || {
            let host_id = host_id.clone();
            let confirmed = web_sys::window()
                .and_then(|w| {
                    w.confirm_with_message(&format!(
                        "Delete {host_id} and all its jobs and outcomes? It comes back if it reports again."
                    ))
                    .ok()
                })
                .unwrap_or(false);
            if !confirmed {
                return;
            }
            spawn_local(async move {
                let request = api::DeleteHostRequest {
                    command_id: command_id("delete-host"),
                    host_id,
                };
                let text = match client()
                    .delete_host(authed(request, &session.token()))
                    .await
                {
                    Ok(_) => "deletion accepted".to_string(),
                    Err(status) => format!("refused: {}", status.message()),
                };
                let _ = message.try_set(Some(text));
            });
        }
    };

    view! {
        <h1>{host_id.clone()}</h1>
        <Show when=move || error.get().is_some()>
            <p class="error">{move || error.get()}</p>
        </Show>
        {move || summary.get().map(|host| {
            let approve = approve.clone();
            view! {
                <dl>
                    <dt>"State"</dt><dd>{badge(host_state(&host).to_string())} " " <span class="muted">{host.error.clone()}</span></dd>
                    <dt>"Assignments"</dt><dd>{assignments_line(&host)}</dd>
                    <dt>"Last outcome"</dt><dd>{format_ms(host.state_at)}</dd>
                    <dt>"First seen"</dt><dd>{format_ms(host.first_seen)}</dd>
                    <dt>"Last seen"</dt><dd>{format_ms(host.last_seen)}</dd>
                    <dt>"Inventories"</dt><dd>{host.inventories.join(", ")}</dd>
                    <dt>"Labels"</dt><dd>{host.labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")}</dd>
                    <dt>"Certificate"</dt><dd>
                        {if host.pinned_fingerprint.is_empty() { "—".to_string() } else { format!("pinned {}", host.pinned_fingerprint) }}
                        {host.pending_fingerprints.iter().map(|fp| {
                            let fp = fp.clone();
                            let approve = approve.clone();
                            view! { <div>"pending " <code>{fp.clone()}</code> " "
                                <button on:click=move |_| approve(fp.clone())>"Approve"</button></div> }
                        }).collect::<Vec<_>>()}
                    </dd>
                </dl>
                <h2>"Remediations / actions"</h2>
                <pre>{serde_json::from_str::<serde_json::Value>(&host.items_json).and_then(|v| serde_json::to_string_pretty(&v)).unwrap_or_default()}</pre>
            }
        })}
        <h2>"Live"</h2>
        <crate::live::HostLive running=running />
        <h2>"Effective expected state"</h2>
        <HostStateExplorer host=summary />
        <p class="muted">{move || message.get()}</p>

        <h2>"History " <button on:click=move |_| load_history()>"Refresh"</button>
            " " <button on:click=move |_| delete_host()>"Delete host data"</button></h2>
        <table>
            <thead><tr><th>"Finished"</th><th>"Job"</th><th>"Result"</th><th>"Origin"</th><th></th></tr></thead>
            <tbody>
                <For each=move || outcomes.get() key=|o| o.job_id.clone() let:outcome>
                    {
                        let (job_id, month) = (outcome.job_id.clone(), outcome.month.clone());
                        let toggle = toggle_outcome;
                        let open = Memo::new({
                            let job_id = job_id.clone();
                            move |_| expanded.with(|e| e.contains_key(&job_id))
                        });
                        let body = Memo::new({
                            let job_id = job_id.clone();
                            move |_| expanded.with(|e| e.get(&job_id).cloned().flatten())
                        });
                        let log_open = Memo::new({
                            let job_id = job_id.clone();
                            move |_| logs_open.with(|l| l.contains(&job_id))
                        });
                        let has_log = outcome.run_log.is_some();
                        let toggle_log = {
                            let job_id = job_id.clone();
                            move |_| logs_open.update(|l| {
                                if !l.remove(&job_id) {
                                    l.insert(job_id.clone());
                                }
                            })
                        };
                        let (log_job, log_month, finished_at) = (job_id.clone(), month.clone(), outcome.finished_at);
                        view! {
                            <tr class:expanded=open>
                                <td>{format_ms(outcome.finished_at)}</td>
                                <td>{job_label(outcome.job)} {outcome.one_shot.then(|| view! { <span class="tag" title="One-shot attribute: kept in history, does not set the host's state">"one-shot"</span> })}</td>
                                <td>{badge(outcome.compliance.clone())} " " <span class="muted">{outcome.error.clone()}</span></td>
                                <td class="muted">{outcome.origin.clone()}</td>
                                <td><button aria-expanded=move || open.get().to_string() on:click=move |_| toggle(job_id.clone(), month.clone())>
                                    {move || if open.get() { "Hide" } else { "Details" }}</button>
                                    {has_log.then(|| view! {
                                        " " <button aria-expanded=move || log_open.get().to_string() on:click=toggle_log.clone()>
                                            {move || if log_open.get() { "Hide log" } else { "Log" }}</button>
                                    })}
                                    " " {crate::live::log_badge(outcome.run_log.as_ref())}</td>
                            </tr>
                            <Show when=move || log_open.get()>
                                <tr class="outcome-detail">
                                    <td colspan="5">
                                        <crate::live::RunLogView job_id=log_job.clone() month=log_month.clone() finished_at=finished_at />
                                    </td>
                                </tr>
                            </Show>
                            <Show when=move || open.get()>
                                <tr class="outcome-detail">
                                    <td colspan="5">
                                        {move || match body.get() {
                                            Some(json) => view! { <pre>{json}</pre> }.into_any(),
                                            None => view! { <p class="muted">"Loading…"</p> }.into_any(),
                                        }}
                                    </td>
                                </tr>
                            </Show>
                        }
                    }
                </For>
            </tbody>
        </table>
    }
}

/// Follow WatchJobs (last `since_hours`) into `jobs`.
pub fn watch_jobs(
    since_hours: u32,
    jobs: RwSignal<BTreeMap<String, api::JobSummary>>,
    error: RwSignal<Option<String>>,
) {
    watch_jobs_of(None, since_hours, jobs, error)
}

/// Follow WatchJobs with a filter (job id, origin, hosts) into `jobs`.
pub fn watch_jobs_of(
    filter: Option<api::watch_jobs_request::Filter>,
    since_hours: u32,
    jobs: RwSignal<BTreeMap<String, api::JobSummary>>,
    error: RwSignal<Option<String>>,
) {
    let session = expect_context::<Session>();
    spawn_scoped(async move {
        let request = api::WatchJobsRequest {
            filter,
            since_hours,
        };
        let mut stream = match client().watch_jobs(authed(request, &session.token())).await {
            Ok(response) => response.into_inner(),
            Err(status) => {
                session.check(&status);
                let _ = error.try_set(Some(status.message().to_string()));
                return;
            }
        };
        loop {
            let (batch, end) = next_batch(&mut stream).await;
            let upserts: Vec<_> = batch
                .into_iter()
                .filter_map(|message| match message.event? {
                    job_event::Event::Upsert(job) => Some(job),
                    _ => None,
                })
                .collect();
            if !upserts.is_empty() {
                let _ = jobs.try_update(|map| {
                    for job in upserts {
                        map.insert(job.job_id.clone(), job);
                    }
                });
            }
            match end {
                None => {}
                Some(Ok(())) => return,
                Some(Err(status)) => {
                    session.check(&status);
                    let _ = error.try_set(Some(status.message().to_string()));
                    return;
                }
            }
        }
    });
}

#[component]
pub fn Jobs() -> impl IntoView {
    let session = expect_context::<Session>();
    let jobs = RwSignal::new(BTreeMap::<String, api::JobSummary>::new());
    let error = RwSignal::new(None::<String>);
    watch_jobs(24, jobs, error);
    let sorted = move || {
        let mut list: Vec<_> = jobs.with(|jobs| jobs.values().cloned().collect());
        list.sort_by_key(|job| std::cmp::Reverse(job.created_at));
        list.truncate(500);
        list
    };
    let abort = move |job_id: String| {
        spawn_local(async move {
            let request = api::AbortJobRequest {
                command_id: command_id("abort"),
                job_id,
            };
            if let Err(status) = client().abort_job(authed(request, &session.token())).await {
                let _ = error.try_set(Some(format!("abort refused: {}", status.message())));
            }
        });
    };
    // D24: rows whose live view / log is open
    let open = RwSignal::new(std::collections::BTreeSet::<String>::new());
    view! {
        <h1>"Jobs " <span class="muted">"(last 24 h, live)"</span></h1>
        <Show when=move || error.get().is_some()>
            <p class="error">{move || error.get()}</p>
        </Show>
        <table>
            <thead><tr><th>"Created"</th><th>"Host"</th><th>"Job"</th><th>"State"</th><th>"Result"</th><th>"Origin"</th><th></th></tr></thead>
            <tbody>
                <For each=sorted key=|j| (j.job_id.clone(), j.state.clone(), j.updated_at) let:job>
                    {
                        let running = job.state == "PENDING" || job.state == "STARTED";
                        let live = crate::live::is_running(&job.state);
                        let has_outcome = !job.outcome_month.is_empty();
                        let job_id = job.job_id.clone();
                        let is_open = Memo::new({
                            let job_id = job_id.clone();
                            move |_| open.with(|o| o.contains(&job_id))
                        });
                        let toggle = {
                            let job_id = job_id.clone();
                            move |_| open.update(|o| {
                                if !o.remove(&job_id) {
                                    o.insert(job_id.clone());
                                }
                            })
                        };
                        let detail_job = job.clone();
                        view! {
                            <tr>
                                <td>{format_ms(job.created_at)}</td>
                                <td><a href=format!("/hosts/{}", job.host_id)>{job.host_id.clone()}</a></td>
                                <td>{job_label(job.job)}</td>
                                <td>{badge(job.state.clone())}</td>
                                <td>{(!job.compliance.is_empty()).then(|| badge(job.compliance.clone()))} " " <span class="muted">{job.error.clone()}</span></td>
                                <td class="muted">{job.origin.clone()}</td>
                                <td>
                                    {(live || has_outcome).then(|| view! {
                                        <button aria-expanded=move || is_open.get().to_string() on:click=toggle.clone()>
                                            {move || match (is_open.get(), live) {
                                                (true, _) => "Hide",
                                                (false, true) => "Live",
                                                (false, false) => "Log",
                                            }}</button> " "
                                    })}
                                    {running.then(|| view! { <button on:click=move |_| abort(job_id.clone())>"Abort"</button> })}
                                </td>
                            </tr>
                            <Show when=move || is_open.get()>
                                <tr class="outcome-detail">
                                    <td colspan="7">
                                        {
                                            let job = detail_job.clone();
                                            if live {
                                                view! { <crate::live::LiveJob job=job /> }.into_any()
                                            } else {
                                                view! { <crate::live::RunLogView job_id=job.job_id.clone() month=job.outcome_month.clone() finished_at=job.updated_at /> }.into_any()
                                            }
                                        }
                                    </td>
                                </tr>
                            </Show>
                        }
                    }
                </For>
            </tbody>
        </table>
    }
}
