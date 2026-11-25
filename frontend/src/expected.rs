//! Expected states and assignments of the catalog (D11, D23), and the
//! explorer of a host's effective expected state (host page and map's right
//! panel).

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value;

use crate::Session;
use crate::client::api;
use crate::client::{authed, client, command_id, format_ms};
use crate::live::{LastRun, finished_mark, outcome_label, static_lines};
use crate::pages::spawn_scoped;

/// An expected state to display: a final state (D27) with the blame of
/// each attribute and the suspected conflicts.
#[derive(Clone, PartialEq, Default)]
pub struct StateEntry {
    pub reference: String,
    pub revision: String,
    /// The `ExpectedState`'s attributes (regent-sdk serde JSON).
    pub attributes: Vec<Value>,
    /// The expected state each attribute comes from.
    pub origins: Vec<String>,
    /// The earlier states each attribute overrides (same Name), oldest
    /// first.
    pub overrides: Vec<Vec<String>>,
    /// The assignment each attribute comes from (D29; empty for a stack
    /// composed alone).
    pub assignments: Vec<String>,
    /// Later assignments declaring the same attribute, applied once (D29).
    pub also_in: Vec<Vec<String>>,
    /// Suspected conflicts: message, positions of the attributes involved.
    pub warnings: Vec<(String, Vec<usize>)>,
}

impl StateEntry {
    /// From the parts of a `GetEffectiveState` / `ComposeStates` reply.
    pub fn composed(
        state_json: &str,
        revision: &str,
        blame: &[api::AttributeBlame],
        warnings: &[api::CompositionWarning],
    ) -> Option<Self> {
        if state_json.is_empty() {
            return None;
        }
        let value: Value = serde_json::from_str(state_json).ok()?;
        Some(Self {
            reference: String::new(),
            revision: revision.to_string(),
            attributes: value["Attributes"].as_array().cloned().unwrap_or_default(),
            origins: blame.iter().map(|b| b.origin.clone()).collect(),
            overrides: blame.iter().map(|b| b.overrides.clone()).collect(),
            assignments: blame.iter().map(|b| b.assignment.clone()).collect(),
            also_in: blame.iter().map(|b| b.also_in.clone()).collect(),
            warnings: warnings
                .iter()
                .map(|w| {
                    (
                        w.message.clone(),
                        w.attributes.iter().map(|&i| i as usize).collect(),
                    )
                })
                .collect(),
        })
    }
}

/// An assignment of the catalog (D23, D27, D29): the hosts its label
/// selector matches + a stack of expected states (base first, last wins).
#[derive(Clone, Debug, PartialEq)]
pub struct AssignmentEntry {
    /// `{source_id}/{name}`
    pub reference: String,
    pub source_id: String,
    pub name: String,
    pub expected_states: Vec<String>,
    /// The hosts it runs (its own reference, D29).
    pub selector: api::HostSelector,
    /// Its label selector, as text.
    pub hosts: String,
    pub priority: i32,
    pub revision: String,
}

impl AssignmentEntry {
    pub fn from_catalog(entry: &api::CatalogEntry) -> Option<Self> {
        let item: Value = serde_json::from_str(&entry.item_json).ok()?;
        if item["type"] != "assignment" {
            return None;
        }
        let source_id = item["source_id"].as_str()?.to_string();
        let name = item["name"].as_str()?.to_string();
        let reference = format!("{source_id}/{name}");
        Some(Self {
            selector: api::HostSelector {
                assignments: vec![reference.clone()],
                ..Default::default()
            },
            reference,
            source_id,
            name,
            expected_states: stack_of(&item),
            hosts: item["selector"].as_str().unwrap_or_default().to_string(),
            priority: item["priority"].as_i64().unwrap_or(0) as i32,
            revision: item["revision"].as_str().unwrap_or_default().to_string(),
        })
    }
}

/// The expected states of an assignment item, base first (D27; a document
/// written before it has one `expected_state`).
pub fn stack_of(item: &Value) -> Vec<String> {
    match item["expected_states"].as_array() {
        Some(list) if !list.is_empty() => list
            .iter()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect(),
        _ => item["expected_state"]
            .as_str()
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
    }
}

pub fn describe_selector(selector: &api::HostSelector) -> String {
    let mut parts = Vec::new();
    if !selector.hosts.is_empty() {
        parts.push(format!("hosts {}", selector.hosts.join(", ")));
    }
    if !selector.assignments.is_empty() {
        parts.push(format!("assignments {}", selector.assignments.join(", ")));
    }
    if !selector.selector.is_empty() {
        parts.push(format!("hosts {}", selector.selector));
    }
    if !selector.labels.is_empty() {
        let mut labels: Vec<String> = selector
            .labels
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        labels.sort();
        parts.push(format!("labels {}", labels.join(", ")));
    }
    parts.join(" · ")
}

/// What a map group stands for: the hosts its Assess / Reach run on.
#[derive(Clone, Debug, PartialEq)]
pub enum GroupTarget {
    /// An assignment (D23) and its host selector.
    Assignment(String, api::HostSelector),
}

impl GroupTarget {
    pub fn selector(&self) -> api::HostSelector {
        match self {
            GroupTarget::Assignment(_, selector) => selector.clone(),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            GroupTarget::Assignment(reference, _) => format!("assignment {reference}"),
        }
    }
}

pub fn job_label(job: i32) -> &'static str {
    if job == api::JobKind::Reach as i32 {
        "Reach"
    } else {
        "Assess"
    }
}

/// Attribute type (the `Detail` variant) and its user-defined name.
pub fn attribute_title(attribute: &Value) -> (String, Option<String>) {
    let kind = match &attribute["Detail"] {
        Value::Object(map) => map.keys().next().cloned(),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
    .unwrap_or_else(|| "Attribute".into());
    let name = attribute["Name"]
        .as_str()
        .filter(|n| !n.trim().is_empty())
        .map(str::to_string);
    (kind, name)
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("~".into()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if s.is_empty() => Some("\"\"".into()),
        Value::String(s) if !s.contains('\n') => Some(s.clone()),
        Value::Array(a) if a.is_empty() => Some("[]".into()),
        Value::Object(o) if o.is_empty() => Some("{}".into()),
        _ => None,
    }
}

/// YAML-like rendering of a JSON value (read-only display, nulls skipped).
pub fn yamlish(value: &Value) -> String {
    fn walk(value: &Value, indent: usize, out: &mut String) {
        let pad = " ".repeat(indent);
        match value {
            Value::Object(map) => {
                for (key, item) in map.iter().filter(|(_, v)| !v.is_null()) {
                    entry(&format!("{pad}{key}:"), item, indent, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    entry(&format!("{pad}-"), item, indent, out);
                }
            }
            other => {
                out.push_str(&pad);
                out.push_str(&scalar(other).unwrap_or_default());
                out.push('\n');
            }
        }
    }
    fn entry(head: &str, item: &Value, indent: usize, out: &mut String) {
        out.push_str(head);
        if let Some(s) = scalar(item) {
            out.push(' ');
            out.push_str(&s);
            out.push('\n');
        } else if let Value::String(s) = item {
            out.push_str(" |\n");
            for line in s.lines() {
                out.push_str(&" ".repeat(indent + 2));
                out.push_str(line);
                out.push('\n');
            }
        } else {
            out.push('\n');
            walk(item, indent + 2, out);
        }
    }
    let mut out = String::new();
    walk(value, 0, &mut out);
    out
}

/// A host's effective expected state (D23, D27): its assignment and the
/// stack of expected states (base first), Assess / Reach now with it, and
/// its attributes with the state each one comes from.
#[component]
pub fn HostStateExplorer(
    #[prop(into)] host: Signal<Option<api::HostSummary>>,
    /// Colour the attributes with the host's last finished job (its log).
    #[prop(optional)]
    with_last_run: bool,
) -> impl IntoView {
    let session = expect_context::<Session>();
    // reload when the host or its effective state changes
    let key = Memo::new(move |_| {
        host.with(|h| {
            h.as_ref().map(|h| {
                (
                    h.host_id.clone(),
                    h.effective_revision.clone(),
                    h.assignments.clone(),
                )
            })
        })
    });
    let effective = RwSignal::new(None::<api::GetEffectiveStateResponse>);
    let load_error = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        let Some((host_id, _, _)) = key.get() else {
            effective.set(None);
            return;
        };
        spawn_scoped(async move {
            let request = api::GetEffectiveStateRequest { host_id };
            match client()
                .get_effective_state(authed(request, &session.token()))
                .await
            {
                Ok(response) => {
                    let _ = effective.try_set(Some(response.into_inner()));
                    let _ = load_error.try_set(None);
                }
                Err(status) => {
                    session.check(&status);
                    let _ = load_error.try_set(Some(status.message().to_string()));
                }
            }
        });
    });
    let state = Signal::derive(move || {
        effective.with(|e| {
            let e = e.as_ref()?;
            StateEntry::composed(&e.state_json, &e.revision, &e.blame, &e.warnings)
        })
    });
    // the last finished job (D24 log), reloaded when another one finishes
    let last_job = Memo::new(move |_| {
        host.with(|h| {
            h.as_ref()
                .filter(|h| with_last_run && !h.last_job.is_empty())
                .map(|h| (h.last_job.clone(), h.last_job_month.clone(), h.state_at))
        })
    });
    // None: loading or no job; Some(Err): a note instead of colours
    let last_run = RwSignal::new(None::<Result<LastRun, String>>);
    Effect::new(move |_| {
        let Some((job_id, month, at)) = last_job.get() else {
            last_run.set(None);
            return;
        };
        spawn_scoped(async move {
            let request = api::GetRunLogRequest { job_id, month };
            let result = match client()
                .get_run_log(authed(request, &session.token()))
                .await
            {
                Ok(response) => {
                    let log = response.into_inner();
                    let job = log.entries.first().map(|e| e.job).unwrap_or_default();
                    if log.found {
                        Ok(LastRun::from_log(job, at, &log))
                    } else {
                        Err("No log kept for the last job (run_log_max_bytes = 0, or an older runner / regops): outcomes per attribute unknown.".to_string())
                    }
                }
                Err(status) => {
                    session.check(&status);
                    Err(status.message().to_string())
                }
            };
            let _ = last_run.try_set(Some(result));
        });
    });
    let run_ok = Signal::derive(move || {
        last_run.with(|r| r.as_ref().and_then(|r| r.as_ref().ok()).cloned())
    });
    let last_head = move || {
        if !with_last_run {
            return None;
        }
        let compliance = host.with(|h| h.as_ref().map(|h| h.compliance.clone()));
        Some(match (last_job.get(), last_run.get()) {
            (None, _) => view! { <p class="muted">"No finished job within retention: the attributes' outcomes are unknown."</p> }.into_any(),
            (Some(_), None) => view! { <p class="muted">"Loading the last job…"</p> }.into_any(),
            (Some(_), Some(Err(note))) => view! { <p class="muted">{note}</p> }.into_any(),
            (Some(_), Some(Ok(run))) => view! {
                <div class="live-head">
                    {finished_mark()}
                    <b>{job_label(run.job)}</b>
                    {compliance.map(crate::pages::badge)}
                    <span class="muted">{format!("finished {}", format_ms(run.finished_at))}</span>
                </div>
                {run.warnings.iter().map(|w| view! { <p class="warn">{w.clone()}</p> }).collect::<Vec<_>>()}
                {(!run.general.is_empty()).then(|| static_lines(&run.general))}
            }.into_any(),
        })
    };
    let runnable = move || state.with(Option::is_some);
    let message = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        key.track();
        message.set(None);
    });
    let run = move |job: api::JobKind| {
        let Some((host_id, _, _)) = key.get_untracked() else {
            return;
        };
        spawn_local(async move {
            let request = api::RunNowRequest {
                command_id: command_id("run"),
                selector: Some(api::HostSelector {
                    hosts: vec![host_id],
                    ..Default::default()
                }),
                job: job as i32,
                pool: String::new(),
                one_shot_yaml: String::new(),
            };
            let text = match client().run_now(authed(request, &session.token())).await {
                Ok(_) => format!("{} accepted", job_label(job as i32)),
                Err(status) => {
                    session.check(&status);
                    format!("refused: {}", status.message())
                }
            };
            let _ = message.try_set(Some(text));
        });
    };
    // D29: every assignment selecting the host, in order, with its stack
    let layers = move || {
        effective.with(|e| {
            let Some(e) = e.as_ref() else {
                return Vec::new();
            };
            let mut groups: Vec<(String, i32, Vec<String>)> = Vec::new();
            for l in &e.layers {
                match groups.last_mut() {
                    Some((a, _, states)) if *a == l.assignment => states.push(l.expected_state.clone()),
                    _ => groups.push((l.assignment.clone(), l.priority, vec![l.expected_state.clone()])),
                }
            }
            groups
                .into_iter()
                .map(|(assignment, priority, states)| view! {
                    <li><b>{assignment}</b> <span class="muted small">{format!(" priority {priority}")}</span>
                        ": " {states.join(" → ")}</li>
                })
                .collect::<Vec<_>>()
        })
    };
    let unassigned = move || effective.with(|e| e.as_ref().is_some_and(|e| e.layers.is_empty()));
    let error = move || {
        effective.with(|e| {
            e.as_ref()
                .map(|e| e.error.clone())
                .filter(|e| !e.is_empty())
        })
    };

    view! {
        <div class="toolbar">
            <button disabled=move || !runnable() on:click=move |_| run(api::JobKind::Assess)>"Assess"</button>
            <button class="primary" disabled=move || !runnable() on:click=move |_| run(api::JobKind::Reach)>"Reach"</button>
            <span class="muted">{move || message.get()}</span>
        </div>
        {move || load_error.get().map(|e| view! { <p class="error">{e}</p> })}
        <Show when=move || unassigned() && error().is_none()>
            <p class="muted">"No assignment selects this host: it has no expected state, nothing is checked on it."</p>
        </Show>
        <ol class="layers" title="Assignments selecting the host, in the order they are applied (priority, then name), each with its expected states (base first, the last wins)">{layers}</ol>
        {move || error().map(|e| view! { <p class="error">{e}</p> })}
        {last_head}
        <Show when=runnable>
            <ExpectedStateView state=state last_run=run_ok />
        </Show>
    }
}

/// The attributes of an expected state, one folded row each.
#[component]
pub fn ExpectedStateView(
    #[prop(into)] state: Signal<Option<StateEntry>>,
    /// The host's last finished job: each attribute is coloured with its
    /// outcome there and shows its lines when unfolded.
    #[prop(optional, into)]
    last_run: Option<Signal<Option<LastRun>>>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    move || match state.get() {
        None => view! { <p class="muted">"No expected state selected."</p> }.into_any(),
        Some(entry) => {
            let count = entry.attributes.len();
            let rows = entry
                .attributes
                .iter()
                .enumerate()
                .map(|(i, attribute)| {
                    let origin = entry.origins.get(i).cloned();
                    let overrides = entry.overrides.get(i).cloned().unwrap_or_default();
                    let from = entry.assignments.get(i).cloned().filter(|a| !a.is_empty());
                    let also_in = entry.also_in.get(i).cloned().unwrap_or_default();
                    let warned: Vec<String> = entry
                        .warnings
                        .iter()
                        .filter(|(_, at)| at.contains(&i))
                        .map(|(m, _)| m.clone())
                        .collect();
                    let (kind, name) = attribute_title(attribute);
                    let ran = last_run.and_then(|run| {
                        run.with(|run| {
                            run.as_ref()
                                .and_then(|run| run.attribute(i, name.as_deref().unwrap_or(&kind)))
                        })
                    });
                    let class = ran.as_ref().map(|(outcome, _)| format!("run-{outcome}"));
                    let detail = match &attribute["Detail"] {
                        Value::Object(map) if map.len() == 1 => {
                            map.values().next().cloned().unwrap_or_default()
                        }
                        other => other.clone(),
                    };
                    let mut extra = Vec::new();
                    if let Some(privilege) = scalar(&attribute["Privilege"])
                        .or_else(|| serde_json::to_string(&attribute["Privilege"]).ok())
                    {
                        extra.push(format!("privilege: {privilege}"));
                    }
                    if let Some(timeout) = attribute["Timeout"].as_str() {
                        extra.push(format!("timeout: {timeout}"));
                    }
                    let pill = ran.as_ref().map(|(outcome, _)| view! {
                        <span class=format!("run-pill run-{outcome}") title="Outcome in the last job">{outcome_label(outcome)}</span>
                    });
                    let last = ran.map(|(outcome, lines)| view! {
                        <div class="attr-last-run">
                            <span class="muted">"Last job: "</span>
                            <span class=format!("run-pill run-{outcome}")>{outcome_label(outcome)}</span>
                            {(!lines.is_empty()).then(|| static_lines(&lines))}
                        </div>
                    });
                    view! {
                        <li class=class>
                            <details prop:open=move || open.get()>
                                <summary>
                                    <span class="index">{i + 1}</span>
                                    <span class="attr-type">{kind}</span>
                                    <span class="attr-name">{name.unwrap_or_default()}</span>
                                    {pill}
                                    {(!warned.is_empty()).then(|| view! { <span class="attr-warning" title=warned.join("\n")>"⚠"</span> })}
                                    {origin.map(|o| {
                                        let title = match &from {
                                            Some(a) => format!("Expected state this attribute comes from, in assignment {a}"),
                                            None => "Expected state this attribute comes from".to_string(),
                                        };
                                        view! { <span class="attr-origin" title=title>{o}</span> }
                                    })}
                                    {(!also_in.is_empty()).then(|| view! {
                                        <span class="attr-overrides" title="Later assignments declaring the same attribute (same content): it is applied once">
                                            {format!("also in {}", also_in.join(", "))}
                                        </span>
                                    })}
                                    {(!overrides.is_empty()).then(|| view! {
                                        <span class="attr-overrides" title="Earlier expected states whose attribute of the same Name this one replaces">
                                            {format!("overrides {}", overrides.join(", "))}
                                        </span>
                                    })}
                                </summary>
                                <div class="attr-body">
                                    <span class="muted">{extra.join(" · ")}</span>
                                    <pre>{yamlish(&detail)}</pre>
                                    {last}
                                </div>
                            </details>
                        </li>
                    }
                })
                .collect::<Vec<_>>();
            let warnings = entry
                .warnings
                .iter()
                .map(|(message, at)| {
                    let positions: Vec<String> = at.iter().map(|i| format!("#{}", i + 1)).collect();
                    view! { <li title=format!("Attributes {}", positions.join(", "))>{message.clone()}</li> }
                })
                .collect::<Vec<_>>();
            let has_warnings = !warnings.is_empty();
            let summary = if entry.revision.is_empty() {
                format!("{count} attributes")
            } else {
                format!("{count} attributes, revision {}", entry.revision)
            };
            view! {
                {has_warnings.then(|| view! { <ul class="composition-warnings">{warnings}</ul> })}
                <div class="toolbar">
                    <span class="muted">{summary}</span>
                    <button on:click=move |_| open.set(true)>"Expand all"</button>
                    <button on:click=move |_| open.set(false)>"Collapse all"</button>
                </div>
                {if count == 0 {
                    view! { <p class="muted">"This expected state has no attributes."</p> }.into_any()
                } else {
                    view! { <ul class="attributes">{rows}</ul> }.into_any()
                }}
            }
            .into_any()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn titles() {
        let attribute = json!({"Name": "nginx", "Privilege": "None",
            "Detail": {"Apt": {"Package": "nginx", "State": "Present"}}, "Timeout": null});
        assert_eq!(
            attribute_title(&attribute),
            ("Apt".to_string(), Some("nginx".to_string()))
        );
        assert_eq!(
            attribute_title(&json!({"Name": null, "Detail": {"Ping": {}}})),
            ("Ping".to_string(), None)
        );
    }

    #[test]
    fn yaml_like() {
        let value = json!({"Package": "nginx", "Opt": null, "List": ["a", {"K": 1}],
            "Text": "l1\nl2", "Empty": []});
        assert_eq!(
            yamlish(&value),
            // in the document's order (serde_json's preserve_order)
            "Package: nginx\nList:\n  - a\n  -\n    K: 1\nText: |\n  l1\n  l2\nEmpty: []\n"
        );
    }
}
