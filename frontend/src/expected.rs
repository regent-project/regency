//! Expected states and assignments of the catalog (D11, D23), and the
//! explorer of a host's effective expected state (host page and map's right
//! panel).

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value;

use crate::Session;
use crate::client::api;
use crate::client::{authed, client, command_id};
use crate::pages::spawn_scoped;

/// An expected state to display: an effective state (D23) and the
/// assignment each attribute comes from.
#[derive(Clone, PartialEq)]
pub struct StateEntry {
    pub reference: String,
    pub revision: String,
    /// The `ExpectedState`'s attributes (regent-sdk serde JSON).
    pub attributes: Vec<Value>,
    /// For an effective state (D23): the assignment of each attribute.
    pub origins: Vec<String>,
}

/// An assignment of the catalog (D23): hosts + expected state + priority.
#[derive(Clone, Debug, PartialEq)]
pub struct AssignmentEntry {
    /// `{source_id}/{name}`
    pub reference: String,
    pub source_id: String,
    pub name: String,
    pub expected_state: String,
    pub priority: i32,
    pub selector: api::HostSelector,
    pub revision: String,
}

impl AssignmentEntry {
    pub fn from_catalog(entry: &api::CatalogEntry) -> Option<Self> {
        let item: Value = serde_json::from_str(&entry.item_json).ok()?;
        if item["type"] != "assignment" {
            return None;
        }
        let strings = |v: &Value| -> Vec<String> {
            v.as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let selector = &item["selector"];
        let source_id = item["source_id"].as_str()?.to_string();
        let name = item["name"].as_str()?.to_string();
        Some(Self {
            reference: format!("{source_id}/{name}"),
            source_id,
            name,
            expected_state: item["expected_state"].as_str()?.to_string(),
            priority: item["priority"].as_i64().unwrap_or(0) as i32,
            selector: api::HostSelector {
                hosts: strings(&selector["hosts"]),
                inventories: strings(&selector["inventories"]),
                labels: selector["labels"]
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            revision: item["revision"].as_str().unwrap_or_default().to_string(),
        })
    }
}

pub fn describe_selector(selector: &api::HostSelector) -> String {
    let mut parts = Vec::new();
    if !selector.hosts.is_empty() {
        parts.push(format!("hosts {}", selector.hosts.join(", ")));
    }
    if !selector.inventories.is_empty() {
        parts.push(format!("inventories {}", selector.inventories.join(", ")));
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

/// Load every assignment of the catalog into `into` (lowest priority first).
pub fn load_assignments(into: RwSignal<Vec<AssignmentEntry>>, error: RwSignal<Option<String>>) {
    let session = expect_context::<Session>();
    spawn_scoped(async move {
        let request = api::ListCatalogRequest {
            source_id: String::new(),
            r#type: "assignment".into(),
        };
        match client()
            .list_catalog(authed(request, &session.token()))
            .await
        {
            Ok(response) => {
                let mut list: Vec<AssignmentEntry> = response
                    .into_inner()
                    .items
                    .iter()
                    .filter_map(AssignmentEntry::from_catalog)
                    .collect();
                list.sort_by(|a, b| {
                    a.priority
                        .cmp(&b.priority)
                        .then_with(|| a.reference.cmp(&b.reference))
                });
                let _ = into.try_set(list);
            }
            Err(status) => {
                session.check(&status);
                let _ = error.try_set(Some(status.message().to_string()));
            }
        }
    });
}

/// What a map group stands for: the hosts its Assess / Reach run on.
#[derive(Clone, Debug, PartialEq)]
pub enum GroupTarget {
    /// `{source_id}/{name}`
    Inventory(String),
    Label(String, String),
    /// An assignment (D23) and its host selector.
    Assignment(String, api::HostSelector),
}

impl GroupTarget {
    pub fn selector(&self) -> api::HostSelector {
        match self {
            GroupTarget::Inventory(inventory) => api::HostSelector {
                inventories: vec![inventory.clone()],
                ..Default::default()
            },
            GroupTarget::Label(key, value) => api::HostSelector {
                labels: [(key.clone(), value.clone())].into_iter().collect(),
                ..Default::default()
            },
            GroupTarget::Assignment(_, selector) => selector.clone(),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            GroupTarget::Inventory(inventory) => format!("inventory {inventory}"),
            GroupTarget::Label(key, value) => format!("label {key}={value}"),
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

/// A host's effective expected state (D23): its assignments (layers,
/// lowest priority first), Assess / Reach now with it, and its attributes
/// with the assignment each one comes from.
#[component]
pub fn HostStateExplorer(#[prop(into)] host: Signal<Option<api::HostSummary>>) -> impl IntoView {
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
            if e.state_json.is_empty() {
                return None;
            }
            let value: Value = serde_json::from_str(&e.state_json).ok()?;
            Some(StateEntry {
                reference: String::new(),
                revision: e.revision.clone(),
                attributes: value["Attributes"].as_array().cloned().unwrap_or_default(),
                origins: e.origins.clone(),
            })
        })
    });
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
    let layers = move || {
        effective.with(|e| {
            e.as_ref()
                .map(|e| {
                    e.layers
                        .iter()
                        .map(|l| {
                            view! {
                                <li>
                                    <b>{l.assignment.clone()}</b>
                                    <span class="muted">{format!(" priority {} · state {}", l.priority, l.expected_state)}</span>
                                </li>
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
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
        <Show when=unassigned>
            <p class="muted">"No assignment selects this host: it has no expected state, nothing is checked on it."</p>
        </Show>
        <ol class="layers">{layers}</ol>
        {move || error().map(|e| view! { <p class="error">{format!("These assignments cannot be layered: {e}")}</p> })}
        <Show when=runnable>
            <ExpectedStateView state=state />
        </Show>
    }
}

/// The attributes of an expected state, one folded row each.
#[component]
pub fn ExpectedStateView(#[prop(into)] state: Signal<Option<StateEntry>>) -> impl IntoView {
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
                    let (kind, name) = attribute_title(attribute);
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
                    view! {
                        <li>
                            <details prop:open=move || open.get()>
                                <summary>
                                    <span class="index">{i + 1}</span>
                                    <span class="attr-type">{kind}</span>
                                    <span class="attr-name">{name.unwrap_or_default()}</span>
                                    {origin.map(|o| view! { <span class="attr-origin" title="Assignment this attribute comes from">{o}</span> })}
                                </summary>
                                <div class="attr-body">
                                    <span class="muted">{extra.join(" · ")}</span>
                                    <pre>{yamlish(&detail)}</pre>
                                </div>
                            </details>
                        </li>
                    }
                })
                .collect::<Vec<_>>();
            view! {
                <div class="toolbar">
                    <span class="muted">{format!("{count} attributes, revision {}", entry.revision)}</span>
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
