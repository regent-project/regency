//! Catalog pages (D11, D23, D29): CRUD on the expected states and
//! assignments of API/frontend sources; items of Git sources are shown
//! read-only (edited in Git). Host definitions are in `hostdefs`.
//! Every write is a `catalog.upsert` / `catalog.delete` command (D10):
//! backend accepts it, dbwriter applies it, so lists are reloaded a moment
//! after a change.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

use crate::Session;
use crate::client::{api, authed, client, command_id};
use crate::config::{
    Entry, confirm, delete_item, load_catalog_with, message_view, report, str_of, upsert_item,
};
use crate::expected::job_label;
use crate::labels::{HostLabels, LabelSelector};
use crate::oneshot::{Input, convert};

/// How long dbwriter usually needs to apply a catalog command.
const RELOAD_AFTER: Duration = Duration::from_millis(1200);

/// The Code placeholder of an expected state (a syntax hint, not a value).
const STATE_PLACEHOLDER: &str = r#"- Name: motd present
  Privilege: !None
  Detail: !LineInFile
    FilePath: /etc/motd
    State: !Present
      Create: true
    Line: !Raw "Managed by regency"
"#;

type Message = RwSignal<Option<(bool, String)>>;

/// The catalog's sources and which ones regency may edit (D11: not Git).
#[derive(Clone, Copy)]
struct Sources(RwSignal<Vec<Entry>>);

impl Sources {
    fn kind_of(&self, source_id: &str) -> String {
        self.0
            .get()
            .iter()
            .find(|s| str_of(&s.item, "source_id") == source_id)
            .map(|s| str_of(&s.item, "kind"))
            .unwrap_or_else(|| "?".into())
    }

    fn editable(&self) -> Vec<String> {
        self.0
            .get()
            .into_iter()
            .filter(|s| str_of(&s.item, "kind") != "git")
            .map(|s| str_of(&s.item, "source_id"))
            .collect()
    }

    fn first_editable(&self) -> String {
        self.editable().first().cloned().unwrap_or_default()
    }
}

/// A catalog item name, part of `{source_id}/{name}` references and doc ids.
fn check_name(what: &str, name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains(['/', ':']) {
        Err(format!("{what} must be non-empty, without '/' or ':'"))
    } else {
        Ok(())
    }
}

/// Save `item`, then close the form and reload once dbwriter applied it.
fn save_item(
    session: Session,
    message: Message,
    item: Value,
    on_saved: impl Fn() + Copy + Send + Sync + 'static,
    reload: impl Fn() + Copy + Send + Sync + 'static,
) {
    spawn_local(async move {
        let result = upsert_item(session, item).await;
        let ok = result.is_ok();
        report(message, session, result);
        if ok {
            on_saved();
            set_timeout(reload, RELOAD_AFTER);
        }
    });
}

fn remove_item(
    session: Session,
    message: Message,
    doc_id: String,
    source_id: String,
    reload: impl Fn() + Copy + Send + Sync + 'static,
) {
    spawn_local(async move {
        report(
            message,
            session,
            delete_item(session, doc_id, source_id).await,
        );
        set_timeout(reload, RELOAD_AFTER);
    });
}

/// The source picker of a form (editable sources only).
fn source_select(
    sources: Sources,
    get: impl Fn() -> String + Copy + Send + Sync + 'static,
    set: impl Fn(String) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    view! {
        <label>"Source"
            <select on:change=move |ev| set(event_target_value(&ev))>
                <For each=move || sources.editable() key=|s| s.clone() let:source>
                    {
                        let (value, text) = (source.clone(), source.clone());
                        view! { <option value=value selected=move || get() == source>{text}</option> }
                    }
                </For>
            </select>
        </label>
    }
}

/// When no source is editable: explain, and offer to create source `ui`.
fn no_source_hint(
    session: Session,
    sources: Sources,
    message: Message,
    reload: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let create = move |_| {
        let item = json!({ "type": "source", "source_id": "ui", "kind": "frontend" });
        save_item(session, message, item, || {}, reload);
    };
    view! {
        <Show when=move || sources.editable().is_empty()>
            <span class="muted">"No API/frontend source yet: "</span>
            <button on:click=create>"Create source “ui”"</button>
        </Show>
    }
}

fn source_cell(source_id: String, kind: String) -> impl IntoView {
    view! { <td>{source_id} " " <span class="badge">{kind}</span></td> }
}

fn revision_cell(item: &Value) -> impl IntoView + use<> {
    let revision: String = str_of(item, "revision").chars().take(12).collect();
    view! { <td class="muted"><code>{revision}</code></td> }
}

/// Every host of the catalog with its labels, own and built-in (D26):
/// definitions of one host from several sources are merged.
pub(crate) fn catalog_hosts(hostdefs: &[Entry]) -> Vec<(String, HostLabels)> {
    let mut by_host: BTreeMap<String, (BTreeSet<String>, BTreeMap<String, String>)> =
        BTreeMap::new();
    for entry in hostdefs {
        let (sources, labels) = by_host.entry(str_of(&entry.item, "host_id")).or_default();
        sources.insert(str_of(&entry.item, "source_id"));
        if let Some(own) = entry.item["labels"].as_object() {
            for (k, v) in own {
                labels.insert(k.clone(), v.as_str().unwrap_or_default().to_string());
            }
        }
    }
    by_host
        .into_iter()
        .map(|(id, (sources, labels))| {
            let host = HostLabels::new(&id, sources.iter().map(String::as_str), &labels);
            (id, host)
        })
        .collect()
}

/// The hosts `selector` selects.
fn selected_hosts(selector: &LabelSelector, hosts: &[(String, HostLabels)]) -> Vec<String> {
    hosts
        .iter()
        .filter(|(_, labels)| selector.matches(labels))
        .map(|(id, _)| id.clone())
        .collect()
}

/// "N host(s): a, b, …" (at most 10 names).
fn describe_hosts(hosts: &[String]) -> String {
    let mut shown: Vec<&str> = hosts.iter().take(10).map(String::as_str).collect();
    if hosts.len() > shown.len() {
        shown.push("…");
    }
    if shown.is_empty() {
        "no host".to_string()
    } else {
        format!("{} host(s): {}", hosts.len(), shown.join(", "))
    }
}

// --------------------------------------------------------- expected states

#[derive(Clone, Default)]
struct StateForm {
    source_id: String,
    name: String,
    yaml: String,
    existing: bool,
    read_only: bool,
}

#[component]
pub fn ExpectedStatesConfig() -> impl IntoView {
    let session = expect_context::<Session>();
    let states = RwSignal::new(Vec::<Entry>::new());
    let assignments = RwSignal::new(Vec::<Entry>::new());
    let sources = Sources(RwSignal::new(Vec::new()));
    let error = RwSignal::new(None::<String>);
    let message = RwSignal::new(None::<(bool, String)>);
    let reload = move || {
        load_catalog_with(session, "expected_state", states, error);
        load_catalog_with(session, "assignment", assignments, error);
        load_catalog_with(session, "source", sources.0, error);
    };
    reload();
    let form = RwSignal::new(None::<StateForm>);
    let filter = RwSignal::new(String::new());
    // the YAML as checked by backend (the SDK's serde, D9 included):
    // Ok(attribute count) or the problem
    let check = RwSignal::new(None::<Result<usize, String>>);
    let busy = RwSignal::new(false);

    let used_by = move |reference: &str| {
        assignments
            .get()
            .iter()
            .filter(|a| {
                crate::expected::stack_of(&a.item)
                    .iter()
                    .any(|s| s == reference)
            })
            .map(|a| {
                format!(
                    "{}/{}",
                    str_of(&a.item, "source_id"),
                    str_of(&a.item, "name")
                )
            })
            .collect::<Vec<_>>()
    };

    // check the YAML as it changes (debounced)
    let generation = RwSignal::new(0u64);
    Effect::new(move |_| {
        let Some(yaml) = form.with(|f| f.as_ref().map(|f| f.yaml.clone())) else {
            return;
        };
        let this = generation.get_untracked() + 1;
        generation.set(this);
        if yaml.trim().is_empty() {
            check.set(Some(Ok(0)));
            return;
        }
        check.set(None);
        set_timeout(
            move || {
                if generation.get_untracked() != this {
                    return;
                }
                spawn_local(async move {
                    let result = convert(session, Input::Yaml(yaml)).await;
                    if generation.get_untracked() == this {
                        let _ = check.try_set(Some(match result {
                            Ok((_, json, None)) => Ok(serde_json::from_str::<Value>(&json)
                                .ok()
                                .and_then(|v| v.as_array().map(Vec::len))
                                .unwrap_or(0)),
                            Ok((_, _, Some(problem))) | Err(problem) => Err(problem),
                        }));
                    }
                });
            },
            Duration::from_millis(400),
        );
    });

    // open an item: its attributes as the SDK's YAML
    let open = move |item: Value, read_only: bool| {
        error.set(None);
        let attributes = item["state"]["Attributes"].clone();
        let base = StateForm {
            source_id: str_of(&item, "source_id"),
            name: str_of(&item, "name"),
            yaml: String::new(),
            existing: true,
            read_only,
        };
        if attributes.as_array().is_none_or(Vec::is_empty) {
            form.set(Some(base));
            return;
        }
        spawn_local(async move {
            match convert(session, Input::Json(attributes.to_string())).await {
                Ok((yaml, _, _)) => {
                    let _ = form.try_set(Some(StateForm { yaml, ..base }));
                }
                Err(e) => report(message, session, Err(tonic::Status::internal(e))),
            }
        });
    };
    let save = move || {
        let Some(current) = form.get_untracked() else {
            return;
        };
        if current.source_id.is_empty() {
            message.set(Some((false, "pick a source".into())));
            return;
        }
        let name = current.name.trim().to_string();
        if let Err(e) = check_name("expected state name", &name) {
            message.set(Some((false, e)));
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let attributes = if current.yaml.trim().is_empty() {
                Ok(Value::Array(Vec::new()))
            } else {
                match convert(session, Input::Yaml(current.yaml.clone())).await {
                    Ok((_, json, None)) => serde_json::from_str(&json).map_err(|e| e.to_string()),
                    Ok((_, _, Some(problem))) | Err(problem) => Err(problem),
                }
            };
            match attributes {
                Ok(attributes) => {
                    let item = json!({
                        "type": "expected_state",
                        "source_id": current.source_id,
                        "name": name,
                        "state": { "Attributes": attributes },
                    });
                    save_item(
                        session,
                        message,
                        item,
                        move || {
                            let _ = form.try_set(None);
                        },
                        reload,
                    );
                }
                Err(problem) => {
                    let _ = message.try_set(Some((false, problem)));
                }
            }
            let _ = busy.try_set(false);
        });
    };
    let visible = move || {
        let needle = filter.get().to_lowercase();
        states
            .get()
            .into_iter()
            .filter(|e| needle.is_empty() || e.doc_id.to_lowercase().contains(&needle))
            .map(|e| {
                let kind = sources.kind_of(&str_of(&e.item, "source_id"));
                (e, kind)
            })
            .collect::<Vec<_>>()
    };
    let read_only = move || form.get().is_some_and(|f| f.read_only);

    view! {
        <h1>"Expected states"</h1>
        <Show when=move || error.get().is_some()><p class="error">{move || error.get()}</p></Show>
        {message_view(message)}
        <div class="toolbar">
            <input placeholder="Filter" prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev)) />
            <button class="primary" disabled=move || sources.editable().is_empty()
                on:click=move |_| form.set(Some(StateForm { source_id: sources.first_editable(), ..Default::default() }))>"New expected state"</button>
            <button on:click=move |_| reload()>"Refresh"</button>
            {no_source_hint(session, sources, message, reload)}
        </div>
        <Show when=move || form.get().is_some()>
            <div class="card form wide-form">
                <h2>{move || if read_only() { "Expected state (read-only: Git source)" } else { "Expected state" }}</h2>
                <Show when=move || !form.get().is_some_and(|f| f.existing)
                    fallback=move || view! { <p class="muted">{move || form.get().map(|f| format!("{}/{}", f.source_id, f.name)).unwrap_or_default()}</p> }>
                    {source_select(sources,
                        move || form.get().map(|f| f.source_id).unwrap_or_default(),
                        move |v| form.update(|f| if let Some(f) = f { f.source_id = v }))}
                    <label>"Name"
                        <input placeholder="nginx" prop:value=move || form.get().map(|f| f.name).unwrap_or_default()
                            on:input=move |ev| { let v = event_target_value(&ev); form.update(|f| if let Some(f) = f { f.name = v }); } />
                    </label>
                </Show>
                <label>"Attributes (regent-sdk YAML: a list of attributes, or a whole expected state)"
                    <textarea class="yaml" rows="18" spellcheck="false" placeholder=STATE_PLACEHOLDER
                        prop:readonly=read_only
                        prop:value=move || form.get().map(|f| f.yaml).unwrap_or_default()
                        on:input=move |ev| { let v = event_target_value(&ev); form.update(|f| if let Some(f) = f { f.yaml = v }); }></textarea>
                </label>
                <Show when=move || !read_only()>
                    <p class="b-check">{move || match check.get() {
                        None => view! { <span class="muted">"checking…"</span> }.into_any(),
                        Some(Ok(0)) => view! { <span class="muted">"No attribute: an empty expected state."</span> }.into_any(),
                        Some(Ok(n)) => view! { <span class="notice">{format!("✓ {n} valid attribute(s)")}</span> }.into_any(),
                        Some(Err(problem)) => view! { <span class="error">{problem}</span> }.into_any(),
                    }}</p>
                </Show>
                <div class="toolbar">
                    <Show when=move || !read_only()>
                        <button class="primary" disabled=move || busy.get() || matches!(check.get(), Some(Err(_)))
                            on:click=move |_| save()>"Save"</button>
                    </Show>
                    <button on:click=move |_| form.set(None)>{move || if read_only() { "Close" } else { "Cancel" }}</button>
                </div>
            </div>
        </Show>
        <table>
            <thead><tr><th>"Expected state"</th><th>"Source"</th><th>"Attributes"</th><th>"Used by"</th><th>"Revision"</th><th></th></tr></thead>
            <tbody>
                <For each=visible key=|(e, kind)| (e.doc_id.clone(), str_of(&e.item, "revision"), kind.clone()) let:row>
                    {
                        let (entry, kind) = row;
                        let item = entry.item.clone();
                        let source_id = str_of(&item, "source_id");
                        let name = str_of(&item, "name");
                        let reference = format!("{source_id}/{name}");
                        let editable = kind != "git";
                        let count = item["state"]["Attributes"].as_array().map(Vec::len).unwrap_or(0);
                        let usage_ref = reference.clone();
                        let delete_ref = reference.clone();
                        let (open_item, doc_id) = (item.clone(), entry.doc_id.clone());
                        view! {
                            <tr>
                                <td>{reference.clone()}</td>
                                {source_cell(source_id.clone(), kind.clone())}
                                <td>{count}</td>
                                <td>{move || used_by(&usage_ref).join(", ")}</td>
                                {revision_cell(&item)}
                                <td>
                                    <button on:click=move |_| open(open_item.clone(), !editable)>{if editable { "Edit" } else { "View" }}</button>
                                    {editable.then(|| view! {
                                        <button on:click=move |_| {
                                            let users = used_by(&delete_ref);
                                            let warning = if users.is_empty() {
                                                String::new()
                                            } else {
                                                format!(" It is used by {}: their hosts can then not be checked.", users.join(", "))
                                            };
                                            let (doc_id, source_id) = (doc_id.clone(), source_id.clone());
                                            if confirm(&format!("Delete the expected state {}?{warning}", delete_ref)) {
                                                remove_item(session, message, doc_id, source_id, reload);
                                            }
                                        }>"Delete"</button>
                                    })}
                                </td>
                            </tr>
                        }
                    }
                </For>
            </tbody>
        </table>
    }
}

// ------------------------------------------------------------ assignments

#[derive(Clone, Default)]
struct AssignmentForm {
    source_id: String,
    name: String,
    /// Label selector of its hosts (D29), as text.
    selector: String,
    /// Order among a host's assignments, lower first (D29); as typed.
    priority: String,
    /// Base first, last wins (D27).
    expected_states: Vec<String>,
    existing: bool,
}

/// `<source>/<name>`, both parts non-empty.
fn is_reference(text: &str) -> bool {
    text.split_once('/')
        .is_some_and(|(s, n)| !s.is_empty() && !n.is_empty())
}

/// An assignment of the catalog, as the page needs it.
#[derive(Clone, Debug, PartialEq)]
struct AssignmentRow {
    reference: String,
    source_id: String,
    name: String,
    priority: i32,
    selector: String,
}

impl AssignmentRow {
    fn of(entry: &Entry) -> Self {
        let item = &entry.item;
        Self {
            reference: reference_of(entry),
            source_id: str_of(item, "source_id"),
            name: str_of(item, "name"),
            priority: priority_of(item),
            selector: str_of(item, "selector"),
        }
    }
}

fn priority_of(item: &Value) -> i32 {
    item["priority"].as_i64().unwrap_or(0) as i32
}

impl AssignmentForm {
    fn from_item(item: &Value) -> Self {
        Self {
            source_id: str_of(item, "source_id"),
            name: str_of(item, "name"),
            selector: str_of(item, "selector"),
            priority: priority_of(item).to_string(),
            expected_states: crate::expected::stack_of(item),
            existing: true,
        }
    }

    fn priority(&self) -> Result<i32, String> {
        let text = self.priority.trim();
        if text.is_empty() {
            return Ok(0);
        }
        text.parse()
            .map_err(|_| format!("priority {text:?}: a whole number (lower runs first)"))
    }

    fn to_item(&self) -> Result<Value, String> {
        if self.source_id.is_empty() {
            return Err("pick a source".into());
        }
        let name = self.name.trim();
        check_name("assignment name", name)?;
        let selector = LabelSelector::parse(&self.selector).map_err(|e| format!("hosts: {e}"))?;
        let priority = self.priority()?;
        if self.expected_states.is_empty() {
            return Err("add at least one expected state".into());
        }
        for (i, state) in self.expected_states.iter().enumerate() {
            if !is_reference(state) || self.expected_states[..i].contains(state) {
                return Err(format!("expected state {state}: invalid or twice"));
            }
        }
        Ok(json!({
            "type": "assignment",
            "source_id": self.source_id,
            "name": name,
            "selector": selector.to_string(),
            "priority": priority,
            "expected_states": self.expected_states,
        }))
    }

    /// Another source's assignment with the same priority and name (D29:
    /// their order would be ambiguous), if any.
    fn ambiguous_with<'a>(&self, rows: &'a [AssignmentRow]) -> Option<&'a AssignmentRow> {
        let priority = self.priority().ok()?;
        rows.iter().find(|r| {
            r.name == self.name.trim() && r.priority == priority && r.source_id != self.source_id
        })
    }
}

/// `{source}/{name}` of a catalog entry.
fn reference_of(entry: &Entry) -> String {
    format!(
        "{}/{}",
        str_of(&entry.item, "source_id"),
        str_of(&entry.item, "name")
    )
}

/// The final expected state of a stack (D27), composed by backend
/// (`ComposeStates`): each attribute with the state it comes from and the
/// ones it overrides, the suspected conflicts, and the hosts the selector
/// selects.
#[component]
fn FinalState(
    /// Expected states, base first.
    #[prop(into)]
    stack: Signal<Vec<String>>,
    /// Label selector, as text; empty: the hosts are not shown (the editor
    /// shows them in its own column).
    #[prop(into)]
    selector: Signal<String>,
    #[prop(into)] hosts: Signal<Vec<(String, HostLabels)>>,
    /// Changes when an expected state changes (recompose).
    #[prop(into)]
    version: Signal<String>,
) -> impl IntoView {
    let session = expect_context::<Session>();
    let composed = RwSignal::new(None::<Result<crate::expected::StateEntry, String>>);
    let generation = StoredValue::new(0u64);
    Effect::new(move |_| {
        version.track();
        let stack = stack.get();
        let current = generation.get_value() + 1;
        generation.set_value(current);
        if stack.is_empty() {
            composed.set(None);
            return;
        }
        crate::pages::spawn_scoped(async move {
            let request = api::ComposeStatesRequest {
                expected_states: stack,
            };
            let result = match client()
                .compose_states(authed(request, &session.token()))
                .await
            {
                Ok(response) => {
                    let r = response.into_inner();
                    if r.error.is_empty() {
                        crate::expected::StateEntry::composed(
                            &r.state_json,
                            "",
                            &r.blame,
                            &r.warnings,
                        )
                        .ok_or_else(|| "unreadable state".to_string())
                    } else {
                        Err(r.error)
                    }
                }
                Err(status) => {
                    session.check(&status);
                    Err(status.message().to_string())
                }
            };
            if generation.try_get_value() == Some(current) {
                let _ = composed.try_set(Some(result));
            }
        });
    });
    let selected = Memo::new(move |_| {
        LabelSelector::parse(&selector.get()).map(|s| hosts.with(|h| selected_hosts(&s, h)))
    });
    let selects = move || match selected.get() {
        _ if selector.with(|s| s.trim().is_empty()) => ().into_any(),
        Ok(list) => view! {
            <p><code>{selector.get()}</code> " · " {describe_hosts(&list)}</p>
        }
        .into_any(),
        Err(e) => view! { <p class="error">{format!("hosts: {e}")}</p> }.into_any(),
    };
    let state = Signal::derive(move || {
        composed.with(|c| c.as_ref().and_then(|r| r.as_ref().ok()).cloned())
    });
    view! {
        <div class="final-state">
            <h3>"Final expected state"</h3>
            {selects}
            {move || match composed.get() {
                None => view! { <p class="muted">"No expected state yet."</p> }.into_any(),
                Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
                Some(Ok(_)) => view! { <crate::expected::ExpectedStateView state=state /> }.into_any(),
            }}
        </div>
    }
}

type StackChange = Box<dyn FnOnce(&mut Vec<String>)>;

/// The ordered expected states of an assignment, base first, as chips on
/// one wrapping line, each with buttons to move it earlier / later or
/// remove it, and a list to add one (from the catalog only).
fn stack_editor(
    form: RwSignal<Option<AssignmentForm>>,
    state_refs: Memo<Vec<String>>,
) -> impl IntoView {
    let stack = move || {
        form.with(|f| {
            f.as_ref()
                .map(|f| f.expected_states.clone())
                .unwrap_or_default()
        })
    };
    let edit = move |change: StackChange| {
        form.update(|f| {
            if let Some(f) = f {
                change(&mut f.expected_states)
            }
        })
    };
    let chips = move || {
        let list = stack();
        let last = list.len().saturating_sub(1);
        list.into_iter()
            .enumerate()
            .map(|(i, reference)| {
                let missing = !state_refs.with(|known| known.contains(&reference));
                let title = match (i, missing) {
                    (_, true) => "No such expected state in the catalog",
                    (0, _) => "The base",
                    _ => "Overrides the states before it",
                };
                view! {
                    <span class="chip" class:missing=missing title=title>
                        <span class="chip-index">{i + 1}</span>
                        <span class="chip-text">{reference}</span>
                        <button type="button" class="chip-x" title="Earlier (overridden by more states)" disabled=i == 0
                            on:click=move |_| edit(Box::new(move |s| s.swap(i - 1, i)))>"◀"</button>
                        <button type="button" class="chip-x" title="Later (overrides more states)" disabled=i == last
                            on:click=move |_| edit(Box::new(move |s| s.swap(i, i + 1)))>"▶"</button>
                        <button type="button" class="chip-x" title="Remove from the stack"
                            on:click=move |_| edit(Box::new(move |s| { s.remove(i); }))>"×"</button>
                    </span>
                }
            })
            .collect::<Vec<_>>()
    };
    let addable = move || {
        let used = stack();
        state_refs
            .get()
            .into_iter()
            .filter(|r| !used.contains(r))
            .collect::<Vec<_>>()
    };
    view! {
        <div class="chip-line">
            {chips}
            <select class="chip-add" prop:value="" disabled=move || addable().is_empty()
                on:change=move |ev| {
                    let v = event_target_value(&ev);
                    if !v.is_empty() {
                        edit(Box::new(move |s| s.push(v)));
                    }
                }>
                <option value="">{move || if addable().is_empty() { "No other expected state" } else { "+ Expected state…" }}</option>
                <For each=addable key=|r| r.clone() let:reference>
                    <option value=reference.clone()>{reference.clone()}</option>
                </For>
            </select>
        </div>
    }
}

/// The head of the hosts field: its title and the Builder / Code switch.
fn selector_head(
    form: RwSignal<Option<AssignmentForm>>,
    code: RwSignal<bool>,
    opened: RwSignal<u32>,
    message: Message,
) -> impl IntoView {
    let to_builder = move || {
        let text =
            form.with_untracked(|f| f.as_ref().map(|f| f.selector.clone()).unwrap_or_default());
        match LabelSelector::parse(&text) {
            Ok(_) => {
                opened.update(|n| *n += 1);
                code.set(false);
            }
            Err(e) => message.set(Some((
                false,
                format!("the selector cannot be shown in the builder: {e}"),
            ))),
        }
    };
    view! {
        <div class="field-head">
            <span class="field-label">"Label filters (all must hold)"</span>
            <div class="modes" role="tablist">
                <button type="button" role="tab" class:active=move || !code.get()
                    title="Build the selector from the labels of the hosts"
                    on:click=move |_| if code.get_untracked() { to_builder() }>"Builder"</button>
                <button type="button" role="tab" class:active=move || code.get()
                    title="Write the selector as text"
                    on:click=move |_| code.set(true)>"Code"</button>
            </div>
        </div>
    }
}

/// The hosts field itself: label filters (Grafana-like builder) or the
/// selector as text (Code).
fn selector_body(
    form: RwSignal<Option<AssignmentForm>>,
    hosts: Memo<Vec<(String, HostLabels)>>,
    code: RwSignal<bool>,
    opened: RwSignal<u32>,
) -> impl IntoView {
    move || {
        opened.track();
        if code.get() {
            view! {
                <input class="mono selector-code" spellcheck="false" placeholder=r#"{env="prod", role=~"web|api"}"#
                    prop:value=move || form.get().map(|f| f.selector).unwrap_or_default()
                    on:input=move |ev| { let v = event_target_value(&ev); form.update(|f| if let Some(f) = f { f.selector = v }); } />
            }.into_any()
        } else {
            view! {
                <crate::labelpicker::SelectorBuilder
                    selector=Signal::derive(move || form.with(|f| f.as_ref().map(|f| f.selector.clone()).unwrap_or_default()))
                    set_selector=Callback::new(move |text: String| form.update(|f| if let Some(f) = f { f.selector = text }))
                    hosts=Signal::derive(move || hosts.get()) />
            }.into_any()
        }
    }
}

#[component]
pub fn AssignmentsConfig() -> impl IntoView {
    let session = expect_context::<Session>();
    let assignments = RwSignal::new(Vec::<Entry>::new());
    let states = RwSignal::new(Vec::<Entry>::new());
    let hostdefs = RwSignal::new(Vec::<Entry>::new());
    let sources = Sources(RwSignal::new(Vec::new()));
    let error = RwSignal::new(None::<String>);
    let message = RwSignal::new(None::<(bool, String)>);
    let reload = move || {
        load_catalog_with(session, "assignment", assignments, error);
        load_catalog_with(session, "expected_state", states, error);
        load_catalog_with(session, "hostdef", hostdefs, error);
        load_catalog_with(session, "source", sources.0, error);
    };
    reload();
    let form = RwSignal::new(None::<AssignmentForm>);
    // the assignment whose details are shown
    let details = RwSignal::new(None::<String>);
    let filter = RwSignal::new(String::new());
    // selector editing: Grafana-like builder or the raw text (Code)
    let code = RwSignal::new(false);
    // bumped when a form opens: the builder starts again from its selector
    let opened = RwSignal::new(0u32);

    let state_refs =
        Memo::new(move |_| states.with(|s| s.iter().map(reference_of).collect::<Vec<_>>()));
    let state_version = Memo::new(move |_| {
        states.with(|s| {
            s.iter()
                .map(|e| format!("{}@{}", e.doc_id, str_of(&e.item, "revision")))
                .collect::<Vec<_>>()
                .join(",")
        })
    });
    let hosts = Memo::new(move |_| hostdefs.with(|h| catalog_hosts(h)));
    let rows = Memo::new(move |_| {
        assignments.with(|list| list.iter().map(AssignmentRow::of).collect::<Vec<_>>())
    });
    let save = move || {
        let Some(current) = form.get_untracked() else {
            return;
        };
        // D29: backend refuses it too; say it before
        if let Some(other) = rows.with_untracked(|r| current.ambiguous_with(r).cloned()) {
            message.set(Some((
                false,
                format!(
                    "assignment {} has the same priority ({}) and name: their order would be ambiguous; change the priority or the name",
                    other.reference, other.priority
                ),
            )));
            return;
        }
        match current.to_item() {
            Err(e) => message.set(Some((false, e))),
            Ok(item) => save_item(
                session,
                message,
                item,
                move || {
                    let _ = form.try_set(None);
                },
                reload,
            ),
        }
    };
    let open_form = move |f: AssignmentForm| {
        details.set(None);
        form.set(Some(f));
        opened.update(|n| *n += 1);
    };
    let visible = move || {
        let needle = filter.get().to_lowercase();
        let mut rows: Vec<_> = assignments
            .get()
            .into_iter()
            .filter(|e| needle.is_empty() || e.item.to_string().to_lowercase().contains(&needle))
            .map(|e| {
                let kind = sources.kind_of(&str_of(&e.item, "source_id"));
                let selected = LabelSelector::parse(&str_of(&e.item, "selector"))
                    .ok()
                    .map(|s| hosts.with(|h| selected_hosts(&s, h)));
                (e, kind, selected)
            })
            .collect();
        // in the order they are applied (D29)
        rows.sort_by_key(|(e, _, _)| {
            (
                priority_of(&e.item),
                str_of(&e.item, "name"),
                e.doc_id.clone(),
            )
        });
        rows
    };
    let shown = Memo::new(move |_| {
        let reference = details.get()?;
        assignments.with(|list| list.iter().find(|e| reference_of(e) == reference).cloned())
    });
    view! {
        <h1>"Assignments"</h1>
        <Show when=move || error.get().is_some()><p class="error">{move || error.get()}</p></Show>
        {message_view(message)}
        <div class="toolbar">
            <input placeholder="Filter" prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev)) />
            <button class="primary" disabled=move || sources.editable().is_empty() || state_refs.with(Vec::is_empty)
                on:click=move |_| open_form(AssignmentForm { source_id: sources.first_editable(), selector: "{}".into(), priority: "0".into(), ..Default::default() })>"New assignment"</button>
            <button on:click=move |_| reload()>"Refresh"</button>
            {no_source_hint(session, sources, message, reload)}
            <Show when=move || state_refs.with(Vec::is_empty)>
                <span class="muted">"Create " <a href="/states">"an expected state"</a> " first."</span>
            </Show>
        </div>
        <Show when=move || form.get().is_some()>
            <div class="card assignment-editor">
                <div class="editor-head">
                    <h2>"Assignment"</h2>
                    <div class="toolbar">
                        <button class="primary" on:click=move |_| save()>"Save"</button>
                        <button on:click=move |_| form.set(None)>"Cancel"</button>
                    </div>
                </div>
                // common parameters
                <div class="form form-row">
                    <Show when=move || !form.get().is_some_and(|f| f.existing)
                        fallback=move || view! { <p class="muted">{move || form.get().map(|f| format!("{}/{}", f.source_id, f.name)).unwrap_or_default()}</p> }>
                        {source_select(sources,
                            move || form.get().map(|f| f.source_id).unwrap_or_default(),
                            move |v| form.update(|f| if let Some(f) = f { f.source_id = v }))}
                        <label>"Name"
                            <input placeholder="web" prop:value=move || form.get().map(|f| f.name).unwrap_or_default()
                                on:input=move |ev| { let v = event_target_value(&ev); form.update(|f| if let Some(f) = f { f.name = v }); } />
                        </label>
                    </Show>
                    <label title="A host gets every assignment selecting it, applied by priority (lower first), then by name">"Priority"
                        <input type="number" step="1" prop:value=move || form.get().map(|f| f.priority).unwrap_or_default()
                            on:input=move |ev| { let v = event_target_value(&ev); form.update(|f| if let Some(f) = f { f.priority = v }); } />
                    </label>
                </div>
                // a sheet: each row lines up across the two columns
                <div class="assignment-grid">
                    <h3 class="cell">"Hosts"</h3>
                    <h3 class="cell right">"Expected states"</h3>
                    <div class="cell">{selector_head(form, code, opened, message)}</div>
                    <div class="cell right"><div class="field-head"><span class="field-label">"Stack (base first, the last wins)"</span></div></div>
                    <div class="cell">{selector_body(form, hosts, code, opened)}</div>
                    <div class="cell right">{stack_editor(form, state_refs)}</div>
                    <div class="cell">
                        <crate::labelpicker::MatchingHosts
                            selector=Signal::derive(move || form.with(|f| f.as_ref().map(|f| f.selector.clone()).unwrap_or_default()))
                            hosts=Signal::derive(move || hosts.get()) />
                    </div>
                    <div class="cell right">
                        <FinalState
                            stack=Signal::derive(move || form.with(|f| f.as_ref().map(|f| f.expected_states.clone()).unwrap_or_default()))
                            selector=""
                            hosts=Signal::derive(move || hosts.get())
                            version=state_version />
                    </div>
                </div>
            </div>
        </Show>
        {move || shown.get().map(|entry| {
            let reference = reference_of(&entry);
            let stack = crate::expected::stack_of(&entry.item);
            let selector = str_of(&entry.item, "selector");
            let priority = priority_of(&entry.item);
            let rows_view = stack.iter().enumerate().map(|(i, s)| view! {
                <li><span class="index">{i + 1}</span> <span class="stack-ref">{s.clone()}</span>
                    {(i == 0).then(|| view! { <span class="muted small">" base"</span> })}</li>
            }).collect::<Vec<_>>();
            view! {
                <div class="card split-editor">
                    <div class="form">
                        <h2>{format!("Assignment {reference}")}</h2>
                        <p>"Hosts " <code>{selector.clone()}</code> " · priority " <b>{priority}</b></p>
                        <div class="field-label">"Expected states (base first, the last wins)"</div>
                        <ol class="stack-list">{rows_view}</ol>
                        <div class="toolbar"><button on:click=move |_| details.set(None)>"Close"</button></div>
                    </div>
                    <FinalState
                        stack=Signal::derive(move || stack.clone())
                        selector=Signal::derive(move || selector.clone())
                        hosts=Signal::derive(move || hosts.get())
                        version=state_version />
                </div>
            }
        })}
        <table>
            <thead><tr><th>"Priority"</th><th>"Assignment"</th><th>"Source"</th><th>"Hosts"</th><th>"Selected"</th><th>"Expected states"</th><th>"Revision"</th><th></th></tr></thead>
            <tbody>
                <For each=visible key=|(e, kind, selected)| (e.doc_id.clone(), str_of(&e.item, "revision"), kind.clone(), selected.as_ref().map(Vec::len)) let:row>
                    {
                        let (entry, kind, selected) = row;
                        let item = entry.item.clone();
                        let source_id = str_of(&item, "source_id");
                        let reference = format!("{source_id}/{}", str_of(&item, "name"));
                        let editable = kind != "git";
                        let selector = str_of(&item, "selector");
                        let stack = crate::expected::stack_of(&item);
                        let stack_view = stack.iter().enumerate().map(|(i, state)| {
                            let wanted = state.clone();
                            let missing = move || !state_refs.get().contains(&wanted);
                            view! {
                                {(i > 0).then(|| view! { <span class="muted">" → "</span> })}
                                {state.clone()}
                                {move || missing().then(|| view! { " " <span class="error small" title="No such expected state in the catalog">"(missing)"</span> })}
                            }
                        }).collect::<Vec<_>>();
                        let count = selected.as_ref().map_or(0, Vec::len);
                        let hosts_title = selected.as_deref().map(describe_hosts).unwrap_or_default();
                        let run_selector = api::HostSelector { assignments: vec![reference.clone()], ..Default::default() };
                        let run_ref = reference.clone();
                        let run = move |job: api::JobKind| {
                            let label = job_label(job as i32);
                            if !confirm(&format!("{label} the {count} host(s) of assignment {run_ref} (each against its whole expected state)?")) {
                                return;
                            }
                            run_now(session, message, run_selector.clone(), job);
                        };
                        let run_reach = run.clone();
                        let (edit_item, doc_id, delete_ref, details_ref) = (item.clone(), entry.doc_id.clone(), reference.clone(), reference.clone());
                        view! {
                            <tr>
                                <td>{priority_of(&item)}</td>
                                <td><a href="#" on:click=move |ev| { ev.prevent_default(); form.set(None); details.set(Some(details_ref.clone())); }>{reference.clone()}</a></td>
                                {source_cell(source_id.clone(), kind.clone())}
                                <td><code>{selector}</code>
                                    {selected.is_none().then(|| view! { " " <span class="error small" title="Unreadable label selector">"(invalid)"</span> })}
                                </td>
                                <td title=hosts_title>{count}</td>
                                <td>{stack_view}</td>
                                {revision_cell(&item)}
                                <td>
                                    <button disabled=count == 0 title="Assess now every host it selects" on:click=move |_| run(api::JobKind::Assess)>"Assess"</button>
                                    <button disabled=count == 0 title="Reach now every host it selects" on:click=move |_| run_reach(api::JobKind::Reach)>"Reach"</button>
                                {editable.then(|| view! {
                                    <button on:click=move |_| open_form(AssignmentForm::from_item(&edit_item))>"Edit"</button>
                                    <button on:click=move |_| {
                                        let (doc_id, source_id) = (doc_id.clone(), source_id.clone());
                                        if confirm(&format!("Delete the assignment {delete_ref}? Its hosts no longer get its expected states.")) {
                                            remove_item(session, message, doc_id, source_id, reload);
                                        }
                                    }>"Delete"</button>
                                })}
                                </td>
                            </tr>
                        }
                    }
                </For>
            </tbody>
        </table>
    }
}

/// Assess / Reach now the hosts of `selector`, each against its effective
/// expected state (D23); results show in Jobs and on the hosts.
fn run_now(session: Session, message: Message, selector: api::HostSelector, job: api::JobKind) {
    spawn_local(async move {
        let request = api::RunNowRequest {
            command_id: command_id("run"),
            selector: Some(selector),
            job: job as i32,
            pool: String::new(),
            one_shot_yaml: String::new(),
        };
        let result = client()
            .run_now(authed(request, &session.token()))
            .await
            .map(|_| format!("{} accepted: follow it in Jobs", job_label(job as i32)));
        report(message, session, result);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hostdef(source: &str, id: &str, labels: Value) -> Entry {
        Entry {
            doc_id: format!("hostdef:{source}:{id}"),
            item: json!({ "type": "hostdef", "source_id": source, "host_id": id, "labels": labels }),
        }
    }

    #[test]
    fn hosts_and_selectors_like_regencylib() {
        let defs = [
            hostdef("git", "web-01", json!({ "env": "prod", "role": "web" })),
            hostdef("ui", "web-01", json!({ "team": "a" })),
            hostdef("ui", "db-01", json!({ "env": "prod", "role": "db" })),
        ];
        let hosts = catalog_hosts(&defs);
        assert_eq!(hosts.len(), 2, "one host from two sources");
        let sel = |t: &str| LabelSelector::parse(t).unwrap();
        assert_eq!(selected_hosts(&sel("{}"), &hosts), ["db-01", "web-01"]);
        assert_eq!(
            selected_hosts(&sel(r#"{role=~"web|api"}"#), &hosts),
            ["web-01"]
        );
        assert_eq!(
            selected_hosts(&sel(r#"{source="git"}"#), &hosts),
            ["web-01"]
        );
        assert_eq!(
            selected_hosts(&sel(r#"{team="a", env="prod"}"#), &hosts),
            ["web-01"]
        );
        assert_eq!(selected_hosts(&sel(r#"{role!="db"}"#), &hosts), ["web-01"]);
        assert_eq!(describe_hosts(&[]), "no host");
    }

    #[test]
    fn assignment_form_round_trip_and_checks() {
        let form = AssignmentForm {
            source_id: "ui".into(),
            name: "web".into(),
            selector: r#"{ role =~ "web|api",env="prod" }"#.into(),
            priority: "10".into(),
            expected_states: vec!["infra/base".into(), "infra/nginx".into()],
            existing: false,
        };
        let item = form.to_item().unwrap();
        assert_eq!(item["type"], "assignment");
        assert_eq!(
            item["expected_states"],
            json!(["infra/base", "infra/nginx"])
        );
        assert_eq!(item["priority"], 10);
        assert_eq!(
            item["selector"], r#"{role=~"web|api", env="prod"}"#,
            "canonical form"
        );
        let back = AssignmentForm::from_item(&item);
        assert_eq!(back.expected_states, form.expected_states);
        assert_eq!(back.priority, "10");
        assert!(back.existing);
        // one state, no priority
        let old = AssignmentForm::from_item(&json!({"source_id": "ui", "name": "a",
            "selector": "{}", "expected_state": "ui/x"}));
        assert_eq!(old.expected_states, ["ui/x"]);
        assert_eq!(old.priority().unwrap(), 0);
        let bad = |change: fn(&mut AssignmentForm)| {
            let mut f = form.clone();
            change(&mut f);
            f.to_item().is_err()
        };
        assert!(bad(|f| f.name = "a/b".into()));
        assert!(bad(|f| f.selector = "{env=prod}".into()));
        assert!(bad(|f| f.priority = "high".into()));
        assert!(bad(|f| f.expected_states.clear()));
        assert!(bad(|f| f.expected_states.push("infra/base".into())));
        // same priority and name in another source: ambiguous
        let row = |source: &str, priority: i32| AssignmentRow {
            reference: format!("{source}/web"),
            source_id: source.into(),
            name: "web".into(),
            priority,
            selector: "{}".into(),
        };
        assert!(form.ambiguous_with(&[row("git", 10)]).is_some());
        assert!(
            form.ambiguous_with(&[row("git", 5), row("ui", 10)])
                .is_none()
        );
    }
}
