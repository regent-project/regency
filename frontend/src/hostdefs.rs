//! Host definitions (D11 `hostdef:` catalog items) on the Hosts pages: a
//! host's definitions on its page (view, edit, add, delete), and the hosts
//! that are defined but have no host doc yet (never run) for the Hosts table.

use std::collections::{BTreeMap, BTreeSet};

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

use crate::Session;
use crate::client::api;
use crate::config::{
    Entry, InstanceView, confirm, delete_item, load_catalog_with, load_instances, message_view,
    parse_pairs, report, str_of, upsert_item,
};
use crate::labels::{HostLabels, LabelSelector};

const SSH_TEMPLATE: &str = r#"{
  "Ssh2": {
    "AuthMethod": { "Key": { "Username": "ops", "Key": { "SecRef": "ops_ed25519", "Provider": "files" } } },
    "HostKey": { "Fingerprints": ["SHA256:..."] }
  }
}"#;

#[derive(Clone, Default)]
struct HostForm {
    source_id: String,
    host_id: String,
    endpoint: String,
    connection: String,
    labels: String,
    pool: String,
    vars: String,
}

impl HostForm {
    fn from_item(item: &Value) -> Self {
        let builder = &item["builder"];
        let pretty = |v: &Value| {
            if v.is_null() {
                String::new()
            } else {
                serde_json::to_string_pretty(v).unwrap_or_default()
            }
        };
        Self {
            source_id: str_of(item, "source_id"),
            host_id: str_of(item, "host_id"),
            endpoint: str_of(builder, "Endpoint"),
            connection: pretty(&builder["HostConnectionMethod"]),
            labels: pairs_text(&item["labels"]),
            pool: str_of(item, "pool"),
            vars: pairs_text(&builder["HostVars"]),
        }
    }

    fn to_item(&self) -> Result<Value, String> {
        if self.source_id.is_empty() || self.host_id.trim().is_empty() {
            return Err("source and host id are required".into());
        }
        let connection: Value = if self.connection.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&self.connection).map_err(|e| format!("connection method: {e}"))?
        };
        let vars = parse_pairs(&self.vars)?;
        // D29: labels decide the host's assignments; host and source are built in
        let labels = parse_pairs(&self.labels)?;
        let own: std::collections::BTreeMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
            .collect();
        crate::labels::check_user_labels(&own)?;
        let mut item = json!({
            "type": "hostdef",
            "source_id": self.source_id,
            "host_id": self.host_id.trim(),
            "builder": {
                "Id": self.host_id.trim(),
                "Endpoint": self.endpoint.trim(),
                "HostConnectionMethod": connection,
                "HostVars": if vars.is_empty() { Value::Null } else { Value::Object(vars) },
            },
            "labels": labels,
        });
        if !self.pool.trim().is_empty() {
            item["pool"] = json!(self.pool.trim());
        }
        Ok(item)
    }
}

/// `key=value, ...` of a JSON object of strings.
fn pairs_text(value: &Value) -> String {
    value
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// A definition's content, its source and revision aside (to compare the
/// definitions of one host, D11).
fn content(item: &Value) -> Value {
    let mut item = item.clone();
    if let Some(map) = item.as_object_mut() {
        map.remove("source_id");
        map.remove("revision");
    }
    item
}

/// The catalog's assignments as (order key, `source/name`, selector), in
/// order (D29: priority, then name); unreadable ones are left out.
fn assignment_selectors(assignments: &[Entry]) -> Vec<((i64, String), String, LabelSelector)> {
    let mut list: Vec<_> = assignments
        .iter()
        .filter_map(|a| {
            let selector = LabelSelector::parse(a.item.get("selector")?.as_str()?).ok()?;
            let name = str_of(&a.item, "name");
            let priority = a.item["priority"].as_i64().unwrap_or(0);
            let full = format!("{}/{name}", str_of(&a.item, "source_id"));
            Some(((priority, name), full, selector))
        })
        .collect();
    list.sort_by(|a, b| a.0.cmp(&b.0));
    list
}

/// Hosts that have definitions but no host doc yet (never run, absent
/// from the live feed), as summaries for the Hosts table: labels from their
/// definitions, assignments computed from the catalog's selectors,
/// `conflicting` when their sources define them differently; compliance
/// `never_run`.
pub fn never_run_hosts(
    hostdefs: &[Entry],
    assignments: &[Entry],
    known: &BTreeMap<String, api::HostSummary>,
) -> Vec<api::HostSummary> {
    let mut by_host: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    for def in hostdefs {
        let host_id = str_of(&def.item, "host_id");
        if !host_id.is_empty() && !known.contains_key(&host_id) {
            by_host.entry(host_id).or_default().push(def);
        }
    }
    let selectors = assignment_selectors(assignments);
    by_host
        .into_iter()
        .map(|(host_id, defs)| {
            let mut labels = BTreeMap::new();
            let mut sources = BTreeSet::new();
            for def in &defs {
                sources.insert(str_of(&def.item, "source_id"));
                if let Some(own) = def.item["labels"].as_object() {
                    for (k, v) in own {
                        labels.insert(k.clone(), v.as_str().unwrap_or_default().to_string());
                    }
                }
            }
            let host = HostLabels::new(&host_id, sources.iter().map(String::as_str), &labels);
            let first = content(&defs[0].item);
            api::HostSummary {
                compliance: "never_run".into(),
                conflicting: defs.iter().any(|d| content(&d.item) != first),
                assignments: selectors
                    .iter()
                    .filter(|(_, _, s)| s.matches(&host))
                    .map(|(_, name, _)| name.clone())
                    .collect(),
                labels: labels.into_iter().collect(),
                host_id,
                ..Default::default()
            }
        })
        .collect()
}

/// Per host: its definitions' sources and addresses (`Endpoint`).
pub fn definitions_by_host(hostdefs: &[Entry]) -> BTreeMap<String, (Vec<String>, Vec<String>)> {
    let mut map: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
    for def in hostdefs {
        let (sources, addresses) = map.entry(str_of(&def.item, "host_id")).or_default();
        sources.push(str_of(&def.item, "source_id"));
        let address = str_of(&def.item["builder"], "Endpoint");
        if !address.is_empty() && !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    map
}

/// A host's definitions on its page: one card per source (read-only for
/// Git sources), Edit / Delete for the others, and "Add a definition" in an
/// API/frontend source that does not define it yet.
#[component]
pub fn HostDefinitions(host_id: String) -> impl IntoView {
    let session = expect_context::<Session>();
    let hostdefs = RwSignal::new(Vec::<Entry>::new());
    let sources = RwSignal::new(Vec::<Entry>::new());
    let error = RwSignal::new(None::<String>);
    let message = RwSignal::new(None::<(bool, String)>);
    let reload = move || {
        load_catalog_with(session, "hostdef", hostdefs, error);
        load_catalog_with(session, "source", sources, error);
    };
    reload();
    // D25: new SSH definitions use the runners' own key (default pool)
    let instances = RwSignal::new(Vec::<InstanceView>::new());
    load_instances(session, instances, error);

    let mine = {
        let host_id = host_id.clone();
        move || {
            hostdefs.with(|defs| {
                defs.iter()
                    .filter(|d| str_of(&d.item, "host_id") == host_id)
                    .cloned()
                    .collect::<Vec<_>>()
            })
        }
    };
    let kind_of = move |source_id: &str| {
        sources.with(|s| {
            s.iter()
                .find(|s| str_of(&s.item, "source_id") == source_id)
                .map(|s| str_of(&s.item, "kind"))
                .unwrap_or_else(|| "?".into())
        })
    };
    // API/frontend sources that do not define this host yet
    let free_sources = {
        let mine = mine.clone();
        move || {
            let taken: BTreeSet<String> = mine()
                .iter()
                .map(|d| str_of(&d.item, "source_id"))
                .collect();
            sources.with(|s| {
                s.iter()
                    .filter(|s| str_of(&s.item, "kind") != "git")
                    .map(|s| str_of(&s.item, "source_id"))
                    .filter(|id| !taken.contains(id))
                    .collect::<Vec<_>>()
            })
        }
    };

    let form = RwSignal::new(None::<HostForm>);
    // true while adding (the source can be picked), false while editing
    let adding = RwSignal::new(false);
    let new_definition = {
        let (host_id, free_sources) = (host_id.clone(), free_sources.clone());
        move || {
            let key = instances.with_untracked(|i| {
                crate::onboarding::runner_keys(i)
                    .remove("default")
                    .and_then(|keys| keys.into_iter().next())
            });
            let connection = match key {
                Some(key) => SSH_TEMPLATE.replace(
                    r#""SecRef": "ops_ed25519""#,
                    &format!(r#""SecRef": {}"#, json!(key.path)),
                ),
                None => SSH_TEMPLATE.to_string(),
            };
            adding.set(true);
            form.set(Some(HostForm {
                source_id: free_sources().first().cloned().unwrap_or_default(),
                host_id: host_id.clone(),
                connection,
                ..Default::default()
            }));
        }
    };
    let create_ui_source = move |_| {
        let item = json!({ "type": "source", "source_id": "ui", "kind": "frontend" });
        spawn_local(async move {
            report(message, session, upsert_item(session, item).await);
            reload();
        });
    };
    let save = move || {
        let Some(current) = form.get_untracked() else {
            return;
        };
        match current.to_item() {
            Err(e) => message.set(Some((false, e))),
            Ok(item) => spawn_local(async move {
                let result = upsert_item(session, item).await;
                let ok = result.is_ok();
                report(message, session, result);
                if ok {
                    let _ = form.try_set(None);
                    // the catalog is written asynchronously (D10)
                    set_timeout(reload, std::time::Duration::from_millis(1500));
                }
            }),
        }
    };
    let field = move |label: &'static str,
                      get: fn(&HostForm) -> String,
                      set: fn(&mut HostForm, String),
                      hint: &'static str| {
        view! {
            <label>{label}
                <input prop:value=move || form.get().map(|f| get(&f)).unwrap_or_default()
                    placeholder=hint
                    on:input=move |ev| { let v = event_target_value(&ev); form.update(|f| if let Some(f) = f { set(f, v) }); } />
            </label>
        }
    };
    let no_editable = move || sources.with(|s| s.iter().all(|s| str_of(&s.item, "kind") == "git"));

    view! {
        <h2>"Definition"</h2>
        <Show when=move || error.get().is_some()><p class="error">{move || error.get()}</p></Show>
        {message_view(message)}
        {
            let mine = mine.clone();
            move || mine().is_empty().then(|| view! {
                <p class="muted">"No definition: regency does not connect to this host (a regops agent may still report it)."</p>
            })
        }
        <For each=mine.clone() key=|d| (d.doc_id.clone(), str_of(&d.item, "revision")) let:entry>
            {
                let item = entry.item.clone();
                let source_id = str_of(&item, "source_id");
                let kind = kind_of(&source_id);
                let editable = kind != "git";
                let builder = &item["builder"];
                let connection = if builder["HostConnectionMethod"].is_null() {
                    "—".to_string()
                } else {
                    serde_json::to_string_pretty(&builder["HostConnectionMethod"]).unwrap_or_default()
                };
                let pool = str_of(&item, "pool");
                let (edit_item, doc_id, delete_source) = (item.clone(), entry.doc_id.clone(), source_id.clone());
                view! {
                    <div class="card">
                        <dl>
                            <dt>"Source"</dt><dd>{source_id.clone()} " " <span class="badge">{kind.clone()}</span>
                                {(!editable).then(|| view! { " " <span class="muted">"read-only: edit it in Git"</span> })}</dd>
                            <dt>"Address"</dt><dd>{str_of(builder, "Endpoint")}</dd>
                            <dt>"Labels"</dt><dd>{pairs_text(&item["labels"])}</dd>
                            <dt>"Runner pool"</dt><dd>{if pool.is_empty() { "default".to_string() } else { pool }}</dd>
                            <dt>"Host vars"</dt><dd>{pairs_text(&builder["HostVars"])}</dd>
                            <dt>"Connection"</dt><dd><pre>{connection}</pre></dd>
                        </dl>
                        {editable.then(|| view! {
                            <div class="toolbar">
                                <button on:click=move |_| { adding.set(false); form.set(Some(HostForm::from_item(&edit_item))); }>"Edit"</button>
                                <button on:click=move |_| {
                                    let (doc_id, source_id) = (doc_id.clone(), delete_source.clone());
                                    if confirm(&format!("Delete the definition of this host in source {source_id}? Its history stays.")) {
                                        spawn_local(async move {
                                            report(message, session, delete_item(session, doc_id, source_id).await);
                                            set_timeout(reload, std::time::Duration::from_millis(1500));
                                        });
                                    }
                                }>"Delete"</button>
                            </div>
                        })}
                    </div>
                }
            }
        </For>
        <div class="toolbar">
            {
                let (new_definition, free_sources) = (new_definition.clone(), free_sources.clone());
                move || {
                    let new_definition = new_definition.clone();
                    (!free_sources().is_empty() && form.with(Option::is_none)).then(|| view! {
                        <button on:click=move |_| new_definition()>"Add a definition"</button>
                    })
                }
            }
            <Show when=no_editable>
                <span class="muted">"No API/frontend source yet: "</span>
                <button on:click=create_ui_source>"Create source “ui”"</button>
            </Show>
        </div>
        <Show when=move || form.get().is_some()>
            <div class="card form">
                <h2>"Host definition"</h2>
                {
                    let free_sources = free_sources.clone();
                    move || if adding.get() {
                        let free_sources = free_sources.clone();
                        view! {
                            <label>"Source"
                                <select on:change=move |ev| { let v = event_target_value(&ev); form.update(|f| if let Some(f) = f { f.source_id = v }); }>
                                    {free_sources().into_iter().map(|source| {
                                        let selected = form.with_untracked(|f| f.as_ref().is_some_and(|f| f.source_id == source));
                                        view! { <option value=source.clone() selected=selected>{source.clone()}</option> }
                                    }).collect::<Vec<_>>()}
                                </select>
                            </label>
                        }.into_any()
                    } else {
                        view! { <p class="muted">{move || form.get().map(|f| format!("Source {}", f.source_id))}</p> }.into_any()
                    }
                }
                {field("Address", |f| f.endpoint.clone(), |f, v| f.endpoint = v, "10.0.0.5:22 or localhost")}
                {field("Labels (they decide the host's assignments)", |f| f.labels.clone(), |f, v| f.labels = v, "env=prod, role=web")}
                {field("Runner pool", |f| f.pool.clone(), |f, v| f.pool = v, "default")}
                {field("Host vars", |f| f.vars.clone(), |f, v| f.vars = v, "key=value, ...")}
                <label>"Connection method (regent-sdk JSON; secrets as references only)"
                    <textarea rows="8" prop:value=move || form.get().map(|f| f.connection).unwrap_or_default()
                        on:input=move |ev| { let v = event_target_value(&ev); form.update(|f| if let Some(f) = f { f.connection = v }); }></textarea>
                </label>
                <div class="toolbar">
                    <button class="primary" on:click=move |_| save()>"Save"</button>
                    <button on:click=move |_| form.set(None)>"Cancel"</button>
                </div>
            </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(source: &str, id: &str, labels: Value, endpoint: &str) -> Entry {
        Entry {
            doc_id: format!("hostdef:{source}:{id}"),
            item: json!({ "type": "hostdef", "source_id": source, "host_id": id,
                "revision": format!("r-{source}"), "labels": labels,
                "builder": { "Id": id, "Endpoint": endpoint } }),
        }
    }

    fn assignment(source: &str, name: &str, selector: &str, priority: i64) -> Entry {
        Entry {
            doc_id: format!("assignment:{source}:{name}"),
            item: json!({ "type": "assignment", "source_id": source, "name": name,
                "selector": selector, "priority": priority, "expected_states": [] }),
        }
    }

    #[test]
    fn never_run_hosts_are_the_defined_ones_without_host_doc() {
        let defs = [
            def("git", "web-01", json!({ "role": "web" }), "10.0.0.1:22"),
            def("ui", "web-01", json!({ "role": "web" }), "10.0.0.1:22"),
            def("ui", "db-01", json!({ "role": "db" }), "10.0.0.2:22"),
            def("ui", "seen", json!({}), "10.0.0.3:22"),
        ];
        let assignments = [
            assignment("ui", "web", r#"{role="web"}"#, 10),
            assignment("ui", "baseline", "{}", 0),
            assignment("git", "edge", r#"{source="git"}"#, 10),
        ];
        let known = BTreeMap::from([("seen".to_string(), api::HostSummary::default())]);
        let hosts = never_run_hosts(&defs, &assignments, &known);
        let ids: Vec<_> = hosts.iter().map(|h| h.host_id.as_str()).collect();
        assert_eq!(ids, ["db-01", "web-01"]);
        let web = &hosts[1];
        // priority, then name; built-in `source` label matched
        assert_eq!(web.assignments, ["ui/baseline", "git/edge", "ui/web"]);
        assert!(!web.conflicting, "same content from two sources");
        assert_eq!(crate::pages::host_state(web), "never_run");
        assert_eq!(hosts[0].assignments, ["ui/baseline"]);
    }

    #[test]
    fn different_definitions_conflict_and_unassigned_wins() {
        let defs = [
            def("git", "web-01", json!({}), "10.0.0.1:22"),
            def("ui", "web-01", json!({}), "10.0.0.9:22"),
        ];
        let hosts = never_run_hosts(&defs, &[], &BTreeMap::new());
        assert!(hosts[0].conflicting);
        assert_eq!(crate::pages::host_state(&hosts[0]), "unassigned");
        let by_host = definitions_by_host(&defs);
        assert_eq!(
            by_host["web-01"],
            (
                vec!["git".to_string(), "ui".to_string()],
                vec!["10.0.0.1:22".to_string(), "10.0.0.9:22".to_string()]
            )
        );
    }
}
