//! Pages: login, live hosts, host detail (with its expected state explorer),
//! live jobs.

use std::collections::{BTreeMap, HashMap};

use futures::future::{AbortHandle, Abortable};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_params_map, use_query_map};

use crate::Session;
use crate::client::api::{self, host_event, job_event};
use crate::client::{authed, client, command_id, format_ms, save_token};
use crate::expected::{HostStateExplorer, job_label};
use crate::labels::{HostLabels, LabelSelector};

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
    // D28: a proactive host checks its own state: its colour is its last
    // pass's, assigned or not
    if !host.agent.is_empty() {
        return &host.compliance;
    }
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
    let text = if host.assignments.is_empty() && !host.agent.is_empty() {
        "none: it applies its own Git state".to_string()
    } else if host.assignments.is_empty() {
        "none: nothing is checked on this host".to_string()
    } else {
        // D29: they add up
        host.assignments.join(" + ")
    };
    let error = host.assignment_error.clone();
    view! {
        <span>{text}</span>
        {(!error.is_empty()).then(|| view! { <div class="error">{error}</div> })}
    }
}

/// D28: a proactive host's agent, where its expected state comes from and
/// how that compares with regency (`dt`/`dd` pairs of a host's `dl`).
pub fn agent_lines(host: &api::HostSummary) -> impl IntoView + use<> {
    let proactive = !host.agent.is_empty();
    let short = |sha: &str| sha.chars().take(10).collect::<String>();
    let source = [
        host.agent_repo.as_str(),
        host.agent_branch.as_str(),
        host.agent_path.as_str(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(" · ");
    let (status, status_title) = match host.repo_status.as_str() {
        "up_to_date" => (
            "up to date",
            format!("Same commit as regency's Git source {}", host.repo_source),
        ),
        "behind" => (
            "behind",
            format!(
                "regency's Git source {} is at {}: the host has not pulled it yet",
                host.repo_source,
                short(&host.repo_head)
            ),
        ),
        _ => (
            "unknown",
            "No regency Git source on this repository and branch: its head is not known"
                .to_string(),
        ),
    };
    let revision = short(&host.agent_revision);
    let drift = host.state_drift.clone();
    let agent = format!("{} {}", host.agent, host.agent_version);
    proactive.then(|| {
        view! {
            <dt>"Proactive"</dt><dd>{agent}</dd>
            <dt>"Its state"</dt><dd>{if source.is_empty() { "—".to_string() } else { source }}</dd>
            <dt>"Commit"</dt><dd>
                {if revision.is_empty() { "—".to_string() } else { revision }} " "
                <span class=format!("badge repo-{}", host.repo_status) title=status_title>{status}</span>
            </dd>
            {(!drift.is_empty()).then(|| view! {
                <dt>"Drift"</dt><dd class="drift" title="Its Git expected state differs from its assignment's final state">{format!("⚠ {drift}")}</dd>
            })}
        }
    })
}

pub fn badge(state: String) -> impl IntoView {
    let class = format!("badge {state}");
    view! { <span class=class>{humanize(&state)}</span> }
}

/// A state as shown in a pill: `NOT_COMPLIANT` / `not_compliant` → `Not compliant`.
pub fn humanize(state: &str) -> String {
    let text = state.replace('_', " ").to_lowercase();
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
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
        <div class="login-page">
        <form class="login" on:submit=move |ev| { ev.prevent_default(); submit(); }>
            <div class="login-brand"><img src="/regent-logo.png" alt="" /><span>"regency"</span></div>
            <h1>"Sign in"</h1>
            <p class="muted">"Enter your username and password to sign in."</p>
            <label>"Username"
                <input placeholder="Username" autocomplete="username"
                    prop:value=move || username.get() on:input=move |ev| username.set(event_target_value(&ev)) />
            </label>
            <label>"Password"
                <input placeholder="Password" type="password" autocomplete="current-password"
                    prop:value=move || password.get() on:input=move |ev| password.set(event_target_value(&ev)) />
            </label>
            <button class="primary" type="submit" disabled=move || busy.get()>"Sign in"</button>
            <Show when=move || error.get().is_some()>
                <p class="error">{move || error.get()}</p>
            </Show>
        </form>
        <aside class="login-aside">
            <div class="login-logo"><img src="/regent-logo.png" alt="" /></div>
            <div class="login-name">"regency"</div>
            <p>"Configuration management for your whole fleet"</p>
        </aside>
        </div>
    }
}

/// A Cedar principal (`User::"alice"`) as a name to show (`alice`).
fn display_name(principal: &str) -> String {
    match principal.split_once("::") {
        Some((_, name)) => name.trim_matches('"').to_string(),
        None => principal.to_string(),
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
    // avatar: the first letter of the name
    let initial = move || {
        display_name(&principal.get())
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_default()
    };
    view! {
        <span class="avatar" aria-hidden="true">{initial}</span>
        <span class="user-name">{move || display_name(&principal.get())}</span>
    }
}

/// Live updates are applied in batches: the first message of a batch, then
/// whatever else arrives within this delay. Thousands of hosts (the initial
/// snapshot, a recurring order firing on a whole assignment) then cost a few
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
    let session = expect_context::<Session>();
    // D25: the "Add a host" dialog
    let adding = RwSignal::new(false);

    // filters, all optional and combined (AND):
    // - plain search: words anywhere in the host's id, state, labels,
    //   assignments or error, no syntax;
    let search = RwSignal::new(String::new());
    // - state, kept in the URL (`?state=`, e.g. from the header's counts);
    let query = use_query_map();
    let state = Memo::new(move |_| query.with(|q| q.get("state")).filter(|s| !s.is_empty()));
    let navigate = leptos_router::hooks::use_navigate();
    // - assignment (`(unassigned)` for hosts without one);
    let assignment = RwSignal::new(String::new());
    // - labels: a Loki-style selector (D26), built or written
    let selector_text = RwSignal::new(String::new());
    let selector_code = RwSignal::new(false);
    // bumped to rebuild the builder from the text (mode switch, clear)
    let builder_epoch = RwSignal::new(0u32);
    let selector = Memo::new(move |_| {
        selector_text.with(|text| {
            let text = text.trim();
            if text.is_empty() || text == "{}" {
                Ok(None)
            } else {
                LabelSelector::parse(text).map(Some)
            }
        })
    });
    // the sources defining each host, for the built-in `source` label
    let hostdefs = RwSignal::new(Vec::<crate::config::Entry>::new());
    crate::config::load_catalog_with(session, "hostdef", hostdefs, RwSignal::new(None));
    let sources = Memo::new(move |_| {
        hostdefs.with(|defs| {
            let mut sources: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for def in defs {
                sources
                    .entry(crate::config::str_of(&def.item, "host_id"))
                    .or_default()
                    .push(crate::config::str_of(&def.item, "source_id"));
            }
            sources
        })
    });
    let labelled = Memo::new(move |_| {
        hosts.with(|hosts| {
            sources.with(|sources| {
                hosts
                    .values()
                    .map(|h| (h.host_id.clone(), labels_of(h, sources.get(&h.host_id))))
                    .collect::<Vec<_>>()
            })
        })
    });
    let assignment_names = Memo::new(move |_| {
        hosts.with(|hosts| {
            let mut names: Vec<String> = hosts
                .values()
                .flat_map(|h| h.assignments.iter().cloned())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            if hosts.values().any(|h| h.assignments.is_empty()) {
                names.push(UNASSIGNED.to_string());
            }
            names
        })
    });
    let state_counts = Memo::new(move |_| {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        hosts.with(|hosts| {
            for host in hosts.values() {
                *counts.entry(host_state(host).to_string()).or_default() += 1;
            }
        });
        counts.into_iter().collect::<Vec<_>>()
    });
    let filtering = move || {
        !search.with(|s| s.trim().is_empty())
            || state.with(Option::is_some)
            || !assignment.with(String::is_empty)
            || selector.with(|s| !matches!(s, Ok(None)))
    };
    let clear = {
        let navigate = navigate.clone();
        move |_| {
            search.set(String::new());
            assignment.set(String::new());
            selector_text.set(String::new());
            builder_epoch.update(|e| *e += 1);
            navigate("/hosts", Default::default());
        }
    };

    let visible = move || {
        let words = search_words(&search.get());
        let state = state.get();
        let assignment = assignment.get();
        // an invalid selector filters nothing (its error is shown)
        let selector = selector.get().ok().flatten();
        let sources = sources.get();
        hosts.with(|hosts| {
            hosts
                .values()
                .filter(|h| state.as_deref().is_none_or(|s| host_state(h) == s))
                .filter(|h| match assignment.as_str() {
                    "" => true,
                    UNASSIGNED => h.assignments.is_empty(),
                    name => h.assignments.iter().any(|a| a == name),
                })
                .filter(|h| {
                    selector
                        .as_ref()
                        .is_none_or(|s| s.matches(&labels_of(h, sources.get(&h.host_id))))
                })
                .filter(|h| search_matches(h, &words))
                .cloned()
                .collect::<Vec<_>>()
        })
    };
    let label_body = move || {
        builder_epoch.track();
        if selector_code.get() {
            view! {
                <input class="mono selector-code" spellcheck="false" placeholder=r#"{env="prod", role=~"web|api"}"#
                    prop:value=move || selector_text.get()
                    on:input=move |ev| selector_text.set(event_target_value(&ev)) />
            }
            .into_any()
        } else {
            view! {
                <crate::labelpicker::SelectorBuilder
                    selector=Signal::derive(move || selector_text.get())
                    set_selector=Callback::new(move |text: String| selector_text.set(text))
                    hosts=Signal::derive(move || labelled.get()) />
            }
            .into_any()
        }
    };
    view! {
        <h1>"Hosts"</h1>
        <Show when=move || adding.get()>
            <crate::onboarding::AddHostDialog on_close=Callback::new(move |()| adding.set(false)) />
        </Show>
        <div class="card filters">
            <div class="filter-row">
                <label class="filter">"Search"
                    <input type="search" placeholder="Host, label, assignment…" class="filter-search"
                        prop:value=move || search.get() on:input=move |ev| search.set(event_target_value(&ev)) />
                </label>
                <label class="filter">"State"
                    <select on:change={
                        let navigate = navigate.clone();
                        move |ev| {
                            let value = event_target_value(&ev);
                            let to = if value.is_empty() {
                                "/hosts".to_string()
                            } else {
                                format!("/hosts?state={}", js_sys::encode_uri_component(&value))
                            };
                            navigate(&to, Default::default());
                        }
                    }>
                        <option value="" selected=move || state.with(Option::is_none)>"All states"</option>
                        {move || state_counts.get().into_iter().map(|(name, n)| {
                            let current = state.get().as_deref() == Some(name.as_str());
                            view! { <option value=name.clone() selected=current>{format!("{} ({n})", humanize(&name))}</option> }
                        }).collect::<Vec<_>>()}
                    </select>
                </label>
                <label class="filter">"Assignment"
                    <select on:change=move |ev| assignment.set(event_target_value(&ev))>
                        <option value="" selected=move || assignment.with(String::is_empty)>"All assignments"</option>
                        {move || assignment_names.get().into_iter().map(|name| {
                            let current = assignment.get() == name;
                            let text = if name == UNASSIGNED { "Unassigned".to_string() } else { name.clone() };
                            view! { <option value=name selected=current>{text}</option> }
                        }).collect::<Vec<_>>()}
                    </select>
                </label>
                <span class="spacer"></span>
                <button class="primary" on:click=move |_| adding.set(true)>"Add host"</button>
            </div>
            <div class="filter-labels">
                <div class="field-head">
                    <span class="field-label">"Labels (Loki-style selector, all filters must hold)"</span>
                    <div class="modes" role="tablist">
                        <button type="button" role="tab" class:active=move || !selector_code.get()
                            title="Build the selector from the hosts' labels"
                            on:click=move |_| if selector_code.get_untracked() {
                                selector_code.set(false);
                                builder_epoch.update(|e| *e += 1);
                            }>"Builder"</button>
                        <button type="button" role="tab" class:active=move || selector_code.get()
                            title="Write the selector as text"
                            on:click=move |_| selector_code.set(true)>"Code"</button>
                    </div>
                </div>
                {label_body}
                {move || selector.get().err().map(|e| view! { <p class="error">{format!("Label selector: {e}")}</p> })}
            </div>
        </div>
        <div class="toolbar">
            <span class="muted">{move || if !ready.get() {
                "loading…".to_string()
            } else if filtering() {
                format!("{} of {} hosts, live", visible().len(), hosts.with(|h| h.len()))
            } else {
                format!("{} hosts, live", hosts.with(|h| h.len()))
            }}</span>
            <Show when=filtering>
                <button class="small" on:click=clear.clone()>"Clear filters"</button>
            </Show>
        </div>
        <Show when=move || error.get().is_some()>
            <p class="error">{move || error.get()}</p>
        </Show>
        <table>
            <thead><tr><th>"Host"</th><th>"State"</th><th>"Last outcome"</th><th>"Last seen"</th><th>"Labels"</th><th>"Assignments"</th></tr></thead>
            <tbody>
                <For each=visible key=|h| (h.host_id.clone(), host_state(h).to_string(), h.state_at, h.last_seen, h.labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<BTreeMap<_, _>>(), h.assignments.clone()) let:host>
                    <tr>
                        <td><a href=format!("/hosts/{}", host.host_id)>{host.host_id.clone()}</a>
                            {host.conflicting.then(|| view! { " " <span class="badge error">"conflicting definitions"</span> })}
                            {(!host.pending_fingerprints.is_empty()).then(|| view! { " " <span class="badge aborted">"certificate pending"</span> })}
                        </td>
                        <td>{badge(host_state(&host).to_string())}
                            {(!host.error.is_empty()).then(|| view! { <div class="muted">{host.error.clone()}</div> })}</td>
                        <td><span class="ts">{format_ms(host.state_at)}</span></td>
                        <td><span class="ts">{format_ms(host.last_seen)}</span></td>
                        <td>{host.labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")}</td>
                        <td>{host.assignments.join(", ")}</td>
                    </tr>
                </For>
            </tbody>
        </table>
    }
}

/// The Hosts page's choice for hosts without assignment.
const UNASSIGNED: &str = "(unassigned)";

/// A host's labels for label selectors (D26): its own (agent labels
/// included, D28), built-in `host` and `source` (its definitions' sources,
/// plus `regops` for a reporting agent).
fn labels_of(host: &api::HostSummary, sources: Option<&Vec<String>>) -> HostLabels {
    let labels: BTreeMap<String, String> = host
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let agent = (!host.agent.is_empty()).then_some("regops");
    let sources = sources
        .into_iter()
        .flatten()
        .map(String::as_str)
        .chain(agent);
    HostLabels::new(&host.host_id, sources, &labels)
}

/// The words of a plain search, lower case.
fn search_words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_lowercase).collect()
}

/// Plain search: every word appears (case-insensitive) in the host's id,
/// state, a label (`key=value`), an assignment or its error.
fn search_matches(host: &api::HostSummary, words: &[String]) -> bool {
    if words.is_empty() {
        return true;
    }
    let state = host_state(host);
    let mut haystack = format!(
        "{} {} {} {}",
        host.host_id,
        state,
        humanize(state),
        host.error
    );
    for (k, v) in &host.labels {
        haystack.push_str(&format!(" {k}={v}"));
    }
    for a in &host.assignments {
        haystack.push(' ');
        haystack.push_str(a);
    }
    let haystack = haystack.to_lowercase();
    words.iter().all(|w| haystack.contains(w.as_str()))
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

    // a host defined in the catalog but never run has no host doc yet: its
    // effective state still comes from the catalog, so it can be assessed
    let explored = Signal::derive({
        let host_id = host_id.clone();
        move || {
            summary.get().or_else(|| {
                ready.get().then(|| api::HostSummary {
                    host_id: host_id.clone(),
                    ..Default::default()
                })
            })
        }
    });

    view! {
        <h1>{host_id.clone()}</h1>
        <Show when=move || ready.get() && summary.with(Option::is_none)>
            <p class="muted">"No outcome yet for this host: it appears on the map and in the tables after its first job."</p>
        </Show>
        <Show when=move || error.get().is_some()>
            <p class="error">{move || error.get()}</p>
        </Show>
        {move || summary.get().map(|host| {
            let approve = approve.clone();
            view! {
                <dl>
                    <dt>"State"</dt><dd>{badge(host_state(&host).to_string())} " " <span class="muted">{host.error.clone()}</span></dd>
                    <dt>"Assignments"</dt><dd>{assignments_line(&host)}</dd>
                    {agent_lines(&host)}
                    <dt>"Last outcome"</dt><dd><span class="ts">{format_ms(host.state_at)}</span></dd>
                    <dt>"First seen"</dt><dd><span class="ts">{format_ms(host.first_seen)}</span></dd>
                    <dt>"Last seen"</dt><dd><span class="ts">{format_ms(host.last_seen)}</span></dd>
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
        <HostStateExplorer host=explored />
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
                                <td><span class="ts">{format_ms(outcome.finished_at)}</span></td>
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

/// Follow WatchJobs (last `since_hours`) into `jobs`, and flash the hosts of
/// jobs only seen finished ([`Flashes`]).
pub fn watch_jobs(
    since_hours: u32,
    jobs: RwSignal<BTreeMap<String, api::JobSummary>>,
    flashes: RwSignal<Flashes>,
    error: RwSignal<Option<String>>,
) {
    follow_jobs(None, since_hours, jobs, Some(flashes), error)
}

/// Follow WatchJobs with a filter (job id, origin, hosts) into `jobs`.
pub fn watch_jobs_of(
    filter: Option<api::watch_jobs_request::Filter>,
    since_hours: u32,
    jobs: RwSignal<BTreeMap<String, api::JobSummary>>,
    error: RwSignal<Option<String>>,
) {
    follow_jobs(filter, since_hours, jobs, None, error)
}

/// Hosts drawn in the job colour for a moment although no job of theirs is
/// in progress: host id -> until when (ms since the epoch).
pub type Flashes = HashMap<String, f64>;

/// How long a host whose job was only seen finished stays in the job colour.
pub const FLASH_MS: f64 = 1_500.0;

/// Jobs that appeared in a batch already finished: their host was never
/// drawn as running (the job was quick, its PENDING / STARTED updates were
/// merged by CouchDB's `_changes` or by the backend for a slow client, or
/// fell in the same batch). Their hosts, to flash. Only jobs new after the
/// snapshot count; regops passes never have a running state (D7), they do
/// not flash.
fn finished_unseen(
    jobs: &mut BTreeMap<String, api::JobSummary>,
    upserts: Vec<(api::JobSummary, bool)>,
) -> Vec<String> {
    let fresh: Vec<String> = upserts
        .iter()
        .filter(|(job, live)| *live && !jobs.contains_key(&job.job_id))
        .map(|(job, _)| job.job_id.clone())
        .collect();
    for (job, _) in upserts {
        jobs.insert(job.job_id.clone(), job);
    }
    let mut hosts: Vec<String> = fresh
        .iter()
        .filter_map(|id| jobs.get(id))
        .filter(|job| !crate::live::is_running(&job.state) && job.origin != ORIGIN_REGOPS)
        .map(|job| job.host_id.clone())
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// `origin` of regops passes (regencylib's `messages::ORIGIN_REGOPS`).
const ORIGIN_REGOPS: &str = "regops";

fn follow_jobs(
    filter: Option<api::watch_jobs_request::Filter>,
    since_hours: u32,
    jobs: RwSignal<BTreeMap<String, api::JobSummary>>,
    flashes: Option<RwSignal<Flashes>>,
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
        let mut snapshot_done = false;
        loop {
            let (batch, end) = next_batch(&mut stream).await;
            // each upsert, and whether it came after the snapshot
            let mut upserts = Vec::with_capacity(batch.len());
            for message in batch {
                match message.event {
                    Some(job_event::Event::Upsert(job)) => upserts.push((job, snapshot_done)),
                    Some(job_event::Event::SnapshotDone(_)) => snapshot_done = true,
                    None => {}
                }
            }
            if !upserts.is_empty() {
                let hosts = jobs
                    .try_update(|map| finished_unseen(map, upserts))
                    .unwrap_or_default();
                if let Some(flashes) = flashes
                    && !hosts.is_empty()
                {
                    flash(flashes, hosts);
                }
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

/// Draw `hosts` in the job colour for [`FLASH_MS`].
fn flash(flashes: RwSignal<Flashes>, hosts: Vec<String>) {
    let until = js_sys::Date::now() + FLASH_MS;
    let _ = flashes.try_update(|f| {
        for host in hosts {
            f.insert(host, until);
        }
    });
    set_timeout(
        move || {
            let now = js_sys::Date::now();
            // only notify when something expired (each flash has its timer)
            if flashes.with_untracked(|f| f.values().any(|t| *t <= now)) {
                let _ = flashes.try_update(|f| f.retain(|_, t| *t > now));
            }
        },
        std::time::Duration::from_millis(FLASH_MS as u64 + 20),
    );
}

#[component]
pub fn Jobs() -> impl IntoView {
    let session = expect_context::<Session>();
    let jobs = RwSignal::new(BTreeMap::<String, api::JobSummary>::new());
    let error = RwSignal::new(None::<String>);
    watch_jobs_of(None, 24, jobs, error);
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
                                <td><span class="ts">{format_ms(job.created_at)}</span></td>
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

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, host: &str, state: &str, origin: &str) -> api::JobSummary {
        api::JobSummary {
            job_id: id.into(),
            host_id: host.into(),
            state: state.into(),
            origin: origin.into(),
            ..Default::default()
        }
    }

    #[test]
    fn hosts_of_jobs_only_seen_finished_flash() {
        let mut jobs: BTreeMap<String, api::JobSummary> = [
            ("seen".to_string(), job("seen", "h1", "STARTED", "c")),
            ("old".to_string(), job("old", "h2", "FINISHED", "c")),
        ]
        .into();
        let hosts = finished_unseen(
            &mut jobs,
            vec![
                // in the snapshot: never flashes
                (job("snap", "h0", "FINISHED", "c"), false),
                // drawn running before: no flash
                (job("seen", "h1", "FINISHED", "c"), true),
                // already known finished (resent): no flash
                (job("old", "h2", "FINISHED", "c"), true),
                // PENDING and FINISHED in one batch: never drawn running
                (job("quick", "h3", "PENDING", "c"), true),
                (job("quick", "h3", "FINISHED", "c"), true),
                // only seen finished
                (job("merged", "h4", "FINISHED", "c"), true),
                // still running: drawn running anyway
                (job("new", "h5", "STARTED", "c"), true),
                // regops passes never run visibly
                (job("pass", "h6", "FINISHED", "regops"), true),
            ],
        );
        assert_eq!(hosts, ["h3", "h4"]);
        assert_eq!(jobs.len(), 7);
        assert_eq!(jobs["seen"].state, "FINISHED");
    }

    fn host() -> api::HostSummary {
        api::HostSummary {
            host_id: "web-01".into(),
            compliance: "not_compliant".into(),
            labels: [("env".to_string(), "prod".to_string())]
                .into_iter()
                .collect(),
            assignments: vec!["git/web".into()],
            effective_revision: "r".into(),
            state_revision: "r".into(),
            ..Default::default()
        }
    }

    #[test]
    fn plain_search_needs_no_syntax() {
        let h = host();
        let ok = |text: &str| search_matches(&h, &search_words(text));
        assert!(ok(""));
        assert!(ok("WEB"));
        assert!(ok("prod web-01"));
        assert!(ok("env=prod"));
        assert!(ok("not compliant"));
        assert!(ok("git/web"));
        assert!(!ok("staging"));
        assert!(!ok("web staging"));
    }

    #[test]
    fn label_selectors_see_built_in_labels() {
        let h = host();
        let sources = vec!["dataset".to_string()];
        let labels = labels_of(&h, Some(&sources));
        let sel = |text: &str| LabelSelector::parse(text).unwrap().matches(&labels);
        assert!(sel(r#"{env="prod"}"#));
        assert!(sel(r#"{host=~"web-.*", source="dataset"}"#));
        assert!(!sel(r#"{env!="prod"}"#));
        let mut agent = host();
        agent.agent = "regops".into();
        assert!(
            LabelSelector::parse(r#"{source="regops"}"#)
                .unwrap()
                .matches(&labels_of(&agent, None))
        );
    }
}
