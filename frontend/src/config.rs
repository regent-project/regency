//! Configuration pages: hosts (CRUD on API/frontend-sourced host
//! definitions), sources (Git addresses...), services (instances and central
//! configuration, D21), security levels (D19), settings.

use std::collections::BTreeMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use serde_json::{Value, json};

use crate::Session;
use crate::client::api;
use crate::client::{authed, client, command_id, format_ms};
use crate::pages::spawn_scoped;

const SERVICES: [&str; 6] = [
    "scheduler",
    "runner",
    "dbwriter",
    "backend",
    "endpoint",
    "sourcesync",
];

/// Keys a central document cannot set (D21 bootstrap keys).
const LOCAL_ONLY: [&str; 9] = [
    "rabbitmq",
    "couchdb",
    "secrets",
    "health",
    "log",
    "central",
    "instance",
    "security.internal",
    "runner.pool",
];

/// What the external level means for each service (D19).
fn external_meaning(service: &str, level: u32) -> &'static str {
    match (service, level) {
        ("backend", 0) => "callers without credentials act as the bootstrap admin; plain HTTP/2",
        ("backend", 1) => "authentication required (sessions, API tokens, OIDC), Cedar",
        ("backend", 2) => "+ TLS (own certificate or tls_offloaded), OIDC issuers https://",
        ("backend", _) => "+ TLS 1.3, password login only for the bootstrap admin, sessions <= 1 h",
        ("endpoint", 0) => "any client certificate, unverified",
        ("endpoint", 1) => "CA-issued or pinned self-signed; trust on first use allowed",
        ("endpoint", 2) => "trust on first use forbidden (admin approval)",
        ("endpoint", _) => "CA-issued only (<= 30-day built-in certificates), TLS 1.3",
        ("runner", 0 | 1) => "any connection method to hosts",
        ("runner", 2) => "host identity verified: SSH with a HostKey, SSHOQ without Insecure TLS",
        ("runner", _) => "+ no password authentication (keys or tokens)",
        ("sourcesync", 0 | 1) => "any Git URL",
        ("sourcesync", _) => "https:// repositories (or local paths)",
        _ => "no external surface",
    }
}

fn level_name(level: u32) -> &'static str {
    match level {
        0 => "0 open",
        1 => "1 authenticated",
        2 => "2 encrypted",
        _ => "3 hardened",
    }
}

/// Run a command RPC and report its result in `message`.
fn report(
    message: RwSignal<Option<(bool, String)>>,
    session: Session,
    result: Result<String, tonic::Status>,
) {
    let value = match result {
        Ok(text) => (true, text),
        Err(status) => {
            session.check(&status);
            (false, status.message().to_string())
        }
    };
    let _ = message.try_set(Some(value));
}

fn message_view(message: RwSignal<Option<(bool, String)>>) -> impl IntoView {
    move || {
        message.get().map(|(ok, text)| {
            let class = if ok { "notice" } else { "error" };
            view! { <p class=class>{text}</p> }
        })
    }
}

#[component]
pub fn ConfigNav() -> impl IntoView {
    view! {
        <nav class="tabs">
            <A href="/config/hosts">"Hosts"</A>
            <A href="/config/sources">"Sources"</A>
            <A href="/config/services">"Services"</A>
            <A href="/config/security">"Security"</A>
            <A href="/config/settings">"Settings"</A>
        </nav>
    }
}

// ---------------------------------------------------------------- catalog

#[derive(Clone, PartialEq)]
struct Entry {
    doc_id: String,
    item: Value,
}

fn load_catalog(kind: &'static str, into: RwSignal<Vec<Entry>>, error: RwSignal<Option<String>>) {
    let session = expect_context::<Session>();
    spawn_local(async move {
        let request = api::ListCatalogRequest {
            source_id: String::new(),
            r#type: kind.into(),
        };
        match client()
            .list_catalog(authed(request, &session.token()))
            .await
        {
            Ok(response) => {
                let mut entries: Vec<Entry> = response
                    .into_inner()
                    .items
                    .into_iter()
                    .filter_map(|e| {
                        Some(Entry {
                            item: serde_json::from_str(&e.item_json).ok()?,
                            doc_id: e.doc_id,
                        })
                    })
                    .collect();
                entries.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
                let _ = into.try_set(entries);
            }
            Err(status) => {
                session.check(&status);
                let _ = error.try_set(Some(status.message().to_string()));
            }
        }
    });
}

fn str_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A content hash as the revision of API/frontend items (D9).
fn content_revision(item: &Value) -> String {
    let text = item.to_string();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("ui-{hash:016x}")
}

async fn upsert_item(session: Session, mut item: Value) -> Result<String, tonic::Status> {
    item["revision"] = Value::Null;
    let revision = content_revision(&item);
    item["revision"] = json!(revision);
    let request = api::UpsertCatalogItemRequest {
        command_id: command_id("catalog"),
        item_json: item.to_string(),
    };
    client()
        .upsert_catalog_item(authed(request, &session.token()))
        .await
        .map(|_| "accepted: the catalog updates in a moment".to_string())
}

async fn delete_item(
    session: Session,
    doc_id: String,
    source_id: String,
) -> Result<String, tonic::Status> {
    let request = api::DeleteCatalogItemRequest {
        command_id: command_id("catalog-delete"),
        doc_id,
        source_id,
    };
    client()
        .delete_catalog_item(authed(request, &session.token()))
        .await
        .map(|_| "deletion accepted".to_string())
}

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
    inventories: String,
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
            inventories: item["inventories"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default(),
            labels: item["labels"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default(),
            pool: str_of(item, "pool"),
            vars: builder["HostVars"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default(),
        }
    }

    fn pairs(text: &str) -> Result<serde_json::Map<String, Value>, String> {
        let mut map = serde_json::Map::new();
        for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (k, v) = part
                .split_once('=')
                .ok_or_else(|| format!("`{part}`: expected key=value"))?;
            map.insert(k.trim().to_string(), json!(v.trim()));
        }
        Ok(map)
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
        let vars = Self::pairs(&self.vars)?;
        let inventories: Vec<String> = self
            .inventories
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
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
            "inventories": inventories,
            "labels": Self::pairs(&self.labels)?,
        });
        if !self.pool.trim().is_empty() {
            item["pool"] = json!(self.pool.trim());
        }
        Ok(item)
    }
}

#[component]
pub fn HostsConfig() -> impl IntoView {
    let session = expect_context::<Session>();
    let hostdefs = RwSignal::new(Vec::<Entry>::new());
    let sources = RwSignal::new(Vec::<Entry>::new());
    let error = RwSignal::new(None::<String>);
    let message = RwSignal::new(None::<(bool, String)>);
    let reload = move || {
        load_catalog("hostdef", hostdefs, error);
        load_catalog("source", sources, error);
    };
    reload();
    let editable_sources = move || {
        sources
            .get()
            .into_iter()
            .filter(|s| str_of(&s.item, "kind") != "git")
            .map(|s| str_of(&s.item, "source_id"))
            .collect::<Vec<_>>()
    };
    let kind_of = move |source_id: &str| {
        sources
            .get()
            .iter()
            .find(|s| str_of(&s.item, "source_id") == source_id)
            .map(|s| str_of(&s.item, "kind"))
            .unwrap_or_else(|| "?".into())
    };

    let form = RwSignal::new(None::<HostForm>);
    let filter = RwSignal::new(String::new());
    let new_host = move || {
        let source_id = editable_sources().first().cloned().unwrap_or_default();
        form.set(Some(HostForm {
            source_id,
            connection: SSH_TEMPLATE.to_string(),
            ..Default::default()
        }));
    };
    let create_ui_source = move || {
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
    // rows carry their source's kind, so they re-render once sources load
    let visible = move || {
        let needle = filter.get().to_lowercase();
        hostdefs
            .get()
            .into_iter()
            .filter(|e| needle.is_empty() || e.doc_id.to_lowercase().contains(&needle))
            .map(|e| {
                let kind = kind_of(&str_of(&e.item, "source_id"));
                (e, kind)
            })
            .collect::<Vec<_>>()
    };

    view! {
        <ConfigNav />
        <h1>"Hosts"</h1>
        <p class="muted">"Host definitions of the catalog (D11). Hosts from a Git source are edited in Git; hosts from API or frontend sources are edited here. Deleting a definition never deletes the host's history: use “Delete host data” on the host page for that."</p>
        <Show when=move || error.get().is_some()><p class="error">{move || error.get()}</p></Show>
        {message_view(message)}
        <div class="toolbar">
            <input placeholder="Filter" prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev)) />
            <button class="primary" on:click=move |_| new_host() disabled=move || editable_sources().is_empty()>"New host"</button>
            <button on:click=move |_| reload()>"Refresh"</button>
            <Show when=move || editable_sources().is_empty()>
                <span class="muted">"No API/frontend source yet: "</span>
                <button on:click=move |_| create_ui_source()>"Create source “ui”"</button>
            </Show>
        </div>
        <Show when=move || form.get().is_some()>
            <div class="card form">
                <h2>"Host definition"</h2>
                <label>"Source"
                    <select on:change=move |ev| { let v = event_target_value(&ev); form.update(|f| if let Some(f) = f { f.source_id = v }); }>
                        <For each=editable_sources key=|s| s.clone() let:source>
                            <option value=source.clone() selected=move || form.get().is_some_and(|f| f.source_id == source)>{source.clone()}</option>
                        </For>
                    </select>
                </label>
                {field("Host id", |f| f.host_id.clone(), |f, v| f.host_id = v, "web-01 (global host id, D9)")}
                {field("Endpoint", |f| f.endpoint.clone(), |f, v| f.endpoint = v, "10.0.0.5:22 or localhost")}
                {field("Inventories", |f| f.inventories.clone(), |f, v| f.inventories = v, "web, production")}
                {field("Labels", |f| f.labels.clone(), |f, v| f.labels = v, "env=prod, team=web")}
                {field("Runner pool", |f| f.pool.clone(), |f, v| f.pool = v, "default")}
                {field("Host vars", |f| f.vars.clone(), |f, v| f.vars = v, "key=value, ...")}
                <label>"Connection method (regent-sdk JSON; secrets as references only, D9)"
                    <textarea rows="8" prop:value=move || form.get().map(|f| f.connection).unwrap_or_default()
                        on:input=move |ev| { let v = event_target_value(&ev); form.update(|f| if let Some(f) = f { f.connection = v }); }></textarea>
                </label>
                <div class="toolbar">
                    <button class="primary" on:click=move |_| save()>"Save"</button>
                    <button on:click=move |_| form.set(None)>"Cancel"</button>
                </div>
            </div>
        </Show>
        <table>
            <thead><tr><th>"Host"</th><th>"Source"</th><th>"Endpoint"</th><th>"Inventories"</th><th>"Labels"</th><th>"Pool"</th><th></th></tr></thead>
            <tbody>
                <For each=visible key=|(e, kind)| (e.doc_id.clone(), str_of(&e.item, "revision"), kind.clone()) let:row>
                    {
                        let (entry, kind) = row;
                        let item = entry.item.clone();
                        let source_id = str_of(&item, "source_id");
                        let editable = kind != "git";
                        let host_id = str_of(&item, "host_id");
                        let (edit_item, doc_id) = (item.clone(), entry.doc_id.clone());
                        let delete_source = source_id.clone();
                        view! {
                            <tr>
                                <td><a href=format!("/hosts/{host_id}")>{host_id.clone()}</a></td>
                                <td>{source_id.clone()} " " <span class="badge">{kind.clone()}</span></td>
                                <td>{str_of(&item["builder"], "Endpoint")}</td>
                                <td>{item["inventories"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default()}</td>
                                <td>{item["labels"].as_object().map(|m| m.iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default())).collect::<Vec<_>>().join(", ")).unwrap_or_default()}</td>
                                <td>{str_of(&item, "pool")}</td>
                                <td>{editable.then(|| view! {
                                    <button on:click=move |_| form.set(Some(HostForm::from_item(&edit_item)))>"Edit"</button>
                                    <button on:click=move |_| {
                                        let (doc_id, source_id) = (doc_id.clone(), delete_source.clone());
                                        if confirm(&format!("Delete the definition {doc_id}?")) {
                                            spawn_local(async move { report(message, session, delete_item(session, doc_id, source_id).await); });
                                        }
                                    }>"Delete"</button>
                                })}</td>
                            </tr>
                        }
                    }
                </For>
            </tbody>
        </table>
    }
}

fn confirm(text: &str) -> bool {
    web_sys::window()
        .and_then(|w| w.confirm_with_message(text).ok())
        .unwrap_or(false)
}

// ---------------------------------------------------------------- sources

#[derive(Clone, Default)]
struct SourceForm {
    source_id: String,
    kind: String,
    repo: String,
    branch: String,
    path: String,
    poll: String,
    secret: String,
    provider: String,
}

impl SourceForm {
    fn from_item(item: &Value) -> Self {
        Self {
            source_id: str_of(item, "source_id"),
            kind: str_of(item, "kind"),
            repo: str_of(item, "repo"),
            branch: str_of(item, "branch"),
            path: str_of(item, "path"),
            poll: item["poll_interval_sec"]
                .as_u64()
                .map(|p| p.to_string())
                .unwrap_or_default(),
            secret: str_of(&item["credentials"], "SecRef"),
            provider: str_of(&item["credentials"], "Provider"),
        }
    }

    fn to_item(&self) -> Result<Value, String> {
        if self.source_id.trim().is_empty() {
            return Err("source id is required".into());
        }
        let mut item =
            json!({ "type": "source", "source_id": self.source_id.trim(), "kind": self.kind });
        if self.kind == "git" {
            if self.repo.trim().is_empty() {
                return Err("a Git source needs a repository URL".into());
            }
            item["repo"] = json!(self.repo.trim());
            for (key, value) in [("branch", &self.branch), ("path", &self.path)] {
                if !value.trim().is_empty() {
                    item[key] = json!(value.trim());
                }
            }
            if !self.poll.trim().is_empty() {
                item["poll_interval_sec"] = json!(
                    self.poll
                        .trim()
                        .parse::<u64>()
                        .map_err(|_| "poll interval: seconds".to_string())?
                );
            }
            if !self.secret.trim().is_empty() {
                let mut reference = json!({ "SecRef": self.secret.trim() });
                if !self.provider.trim().is_empty() {
                    reference["Provider"] = json!(self.provider.trim());
                }
                item["credentials"] = reference;
            }
        }
        Ok(item)
    }
}

#[component]
pub fn SourcesConfig() -> impl IntoView {
    let session = expect_context::<Session>();
    let sources = RwSignal::new(Vec::<Entry>::new());
    let error = RwSignal::new(None::<String>);
    let message = RwSignal::new(None::<(bool, String)>);
    let reload = move || load_catalog("source", sources, error);
    reload();
    let form = RwSignal::new(None::<SourceForm>);
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
                }
            }),
        }
    };
    let refresh = move |source_id: String| {
        spawn_local(async move {
            let request = api::RefreshSourceRequest {
                command_id: command_id("refresh"),
                source_id,
            };
            report(
                message,
                session,
                client()
                    .refresh_source(authed(request, &session.token()))
                    .await
                    .map(|_| "refresh requested".to_string()),
            );
        });
    };
    let input = move |label: &'static str,
                      get: fn(&SourceForm) -> String,
                      set: fn(&mut SourceForm, String),
                      hint: &'static str| {
        view! {
            <label>{label}
                <input prop:value=move || form.get().map(|f| get(&f)).unwrap_or_default() placeholder=hint
                    on:input=move |ev| { let v = event_target_value(&ev); form.update(|f| if let Some(f) = f { set(f, v) }); } />
            </label>
        }
    };
    view! {
        <ConfigNav />
        <h1>"Sources"</h1>
        <p class="muted">"Where inventories and expected states come from (D9, D11). Git sources are polled by sourcesync (layout: inventories/*.yaml, expected_states/*.yaml under the path); credentials are secret references resolved by sourcesync."</p>
        <Show when=move || error.get().is_some()><p class="error">{move || error.get()}</p></Show>
        {message_view(message)}
        <div class="toolbar">
            <button class="primary" on:click=move |_| form.set(Some(SourceForm { kind: "git".into(), branch: "main".into(), poll: "300".into(), ..Default::default() }))>"New source"</button>
            <button on:click=move |_| reload()>"Refresh list"</button>
        </div>
        <Show when=move || form.get().is_some()>
            <div class="card form">
                <h2>"Source"</h2>
                {input("Source id", |f| f.source_id.clone(), |f, v| f.source_id = v, "infra")}
                <label>"Kind"
                    <select on:change=move |ev| { let v = event_target_value(&ev); form.update(|f| if let Some(f) = f { f.kind = v }); }>
                        {["git", "api", "frontend"].map(|k| view! { <option value=k selected=move || form.get().is_some_and(|f| f.kind == k)>{k}</option> })}
                    </select>
                </label>
                <Show when=move || form.get().is_some_and(|f| f.kind == "git")>
                    {input("Repository", |f| f.repo.clone(), |f, v| f.repo = v, "https://git.example.com/ops/infra.git")}
                    {input("Branch", |f| f.branch.clone(), |f, v| f.branch = v, "main")}
                    {input("Path", |f| f.path.clone(), |f, v| f.path = v, "regency/")}
                    {input("Poll interval (s)", |f| f.poll.clone(), |f, v| f.poll = v, "300")}
                    {input("Credentials: secret reference", |f| f.secret.clone(), |f, v| f.secret = v, "git_token")}
                    {input("Credentials: provider", |f| f.provider.clone(), |f, v| f.provider = v, "files")}
                </Show>
                <div class="toolbar">
                    <button class="primary" on:click=move |_| save()>"Save"</button>
                    <button on:click=move |_| form.set(None)>"Cancel"</button>
                </div>
            </div>
        </Show>
        <table>
            <thead><tr><th>"Source"</th><th>"Kind"</th><th>"Repository"</th><th>"Branch / path"</th><th>"Poll"</th><th>"Revision"</th><th></th></tr></thead>
            <tbody>
                <For each=move || sources.get() key=|e| (e.doc_id.clone(), str_of(&e.item, "revision")) let:entry>
                    {
                        let item = entry.item.clone();
                        let source_id = str_of(&item, "source_id");
                        let git = str_of(&item, "kind") == "git";
                        let (edit, doc_id, id_refresh, id_delete) = (item.clone(), entry.doc_id.clone(), source_id.clone(), source_id.clone());
                        view! {
                            <tr>
                                <td>{source_id.clone()}</td>
                                <td><span class="badge">{str_of(&item, "kind")}</span></td>
                                <td><code>{str_of(&item, "repo")}</code></td>
                                <td>{format!("{} {}", str_of(&item, "branch"), str_of(&item, "path"))}</td>
                                <td>{item["poll_interval_sec"].as_u64().map(|p| format!("{p} s")).unwrap_or_default()}</td>
                                <td class="muted"><code>{str_of(&item, "revision").chars().take(12).collect::<String>()}</code></td>
                                <td>
                                    {git.then(|| view! { <button on:click=move |_| refresh(id_refresh.clone())>"Sync now"</button> })}
                                    <button on:click=move |_| form.set(Some(SourceForm::from_item(&edit)))>"Edit"</button>
                                    <button on:click=move |_| {
                                        let (doc_id, source_id) = (doc_id.clone(), id_delete.clone());
                                        if confirm(&format!("Delete the source {source_id}? Its items stay until removed.")) {
                                            spawn_local(async move { report(message, session, delete_item(session, doc_id, source_id).await); });
                                        }
                                    }>"Delete"</button>
                                </td>
                            </tr>
                        }
                    }
                </For>
            </tbody>
        </table>
    }
}

// ---------------------------------------------------------------- services

#[derive(Clone, PartialEq)]
struct InstanceView {
    summary: api::Instance,
    status: Value,
}

fn load_instances(
    session: Session,
    into: RwSignal<Vec<InstanceView>>,
    error: RwSignal<Option<String>>,
) {
    spawn_local(async move {
        match client()
            .list_instances(authed(api::ListInstancesRequest {}, &session.token()))
            .await
        {
            Ok(response) => {
                let list = response
                    .into_inner()
                    .instances
                    .into_iter()
                    .map(|summary| InstanceView {
                        status: serde_json::from_str(&summary.status_json).unwrap_or_default(),
                        summary,
                    })
                    .collect();
                let _ = into.try_set(list);
                let _ = error.try_set(None);
            }
            Err(status) => {
                session.check(&status);
                let _ = error.try_set(Some(status.message().to_string()));
            }
        }
    });
}

/// Poll the instances every 10 s while the page is open.
fn poll_instances(into: RwSignal<Vec<InstanceView>>, error: RwSignal<Option<String>>) {
    let session = expect_context::<Session>();
    load_instances(session, into, error);
    spawn_scoped(async move {
        loop {
            sleep_ms(10_000).await;
            load_instances(session, into, error);
        }
    });
}

async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

fn flatten(value: &Value, prefix: &str, out: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(map)
            if !(map.contains_key("SecRef") && prefix.contains('.')) || map.is_empty() =>
        {
            for (k, v) in map {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(v, &path, out);
            }
        }
        other => {
            out.insert(prefix.to_string(), other.to_string());
        }
    }
}

fn instance_state(i: &api::Instance) -> (&'static str, String) {
    if i.gone {
        ("unknown", "gone".into())
    } else if i.restart_pending {
        ("aborted", "restarting".into())
    } else if !i.config_error.is_empty() {
        ("error", "config refused".into())
    } else if i.ready {
        ("compliant", "ready".into())
    } else {
        ("not_compliant", "not ready".into())
    }
}

#[component]
pub fn ServicesConfig() -> impl IntoView {
    let session = expect_context::<Session>();
    let instances = RwSignal::new(Vec::<InstanceView>::new());
    let error = RwSignal::new(None::<String>);
    let message = RwSignal::new(None::<(bool, String)>);
    poll_instances(instances, error);

    // central configuration editor (TOML, D21)
    let service = RwSignal::new("scheduler".to_string());
    let pool = RwSignal::new(String::new());
    let text = RwSignal::new(String::new());
    let meta = RwSignal::new(String::new());
    let load_doc = move || {
        let (service, pool) = (service.get_untracked(), pool.get_untracked());
        spawn_local(async move {
            let request = api::GetServiceConfigRequest { service, pool };
            match client()
                .get_service_config(authed(request, &session.token()))
                .await
            {
                Ok(response) => {
                    let doc = response.into_inner();
                    let settings: Value =
                        serde_json::from_str(&doc.settings_json).unwrap_or(json!({}));
                    let toml_text = toml::to_string_pretty(&settings).unwrap_or_default();
                    let _ = text.try_set(toml_text);
                    let _ = meta.try_set(if doc.revision.is_empty() {
                        "no central document yet: every key comes from the service's file, env or defaults".into()
                    } else {
                        format!("revision {} by {} at {}", doc.revision, doc.updated_by, format_ms(doc.updated_at))
                    });
                }
                Err(status) => report(message, session, Err(status)),
            }
        });
    };
    load_doc();
    let save_doc = move || {
        let parsed: Result<toml::Table, _> = text.get_untracked().parse();
        let settings = match parsed {
            Ok(table) => serde_json::to_value(table).unwrap_or(json!({})),
            Err(e) => {
                message.set(Some((false, format!("invalid TOML: {e}"))));
                return;
            }
        };
        let (service, pool) = (service.get_untracked(), pool.get_untracked());
        spawn_local(async move {
            let request = api::SetServiceConfigRequest {
                command_id: command_id("config"),
                service,
                pool,
                settings_json: settings.to_string(),
            };
            report(
                message,
                session,
                client()
                    .set_service_config(authed(request, &session.token()))
                    .await
                    .map(|_| {
                        "accepted: instances check it, then restart one by one to apply it"
                            .to_string()
                    }),
            );
        });
    };
    let selected = RwSignal::new(None::<String>);
    let effective = move || {
        let id = selected.get()?;
        let instance = instances
            .get()
            .into_iter()
            .find(|i| format!("{}/{}", i.summary.service, i.summary.instance) == id)?;
        let mut keys = BTreeMap::new();
        flatten(&instance.status["config"], "", &mut keys);
        let origins = instance.status["origins"].clone();
        Some((id, keys, origins))
    };

    view! {
        <ConfigNav />
        <h1>"Services"</h1>
        <Show when=move || error.get().is_some()><p class="error">{move || error.get()}</p></Show>
        {message_view(message)}
        <h2>"Instances " <span class="muted">"(live status, D21)"</span></h2>
        <table>
            <thead><tr><th>"Service"</th><th>"Instance"</th><th>"Version"</th><th>"Pool"</th><th>"State"</th><th>"Security"</th><th>"Started"</th><th>"Last seen"</th><th></th></tr></thead>
            <tbody>
                <For each=move || instances.get() key=|i| (i.summary.service.clone(), i.summary.instance.clone(), i.summary.last_seen) let:instance>
                    {
                        let s = instance.summary.clone();
                        let (state, label) = instance_state(&s);
                        let id = format!("{}/{}", s.service, s.instance);
                        view! {
                            <tr>
                                <td><b>{s.service.clone()}</b></td>
                                <td>{s.instance.clone()}</td>
                                <td>{s.version.clone()} <div class="muted">{format!("regent-sdk {}", s.sdk_version)}</div></td>
                                <td>{s.pool.clone()}</td>
                                <td><span class=format!("badge {state}")>{label}</span>
                                    {(!s.config_error.is_empty()).then(|| view! { <div class="error">{s.config_error.clone()}</div> })}</td>
                                <td>{format!("internal {} · external {}", s.security_internal, s.security_external)}</td>
                                <td>{format_ms(s.started_at)}</td>
                                <td>{format_ms(s.last_seen)}</td>
                                <td><button on:click=move |_| selected.set(Some(id.clone()))>"Effective config"</button></td>
                            </tr>
                        }
                    }
                </For>
            </tbody>
        </table>
        {move || effective().map(|(id, keys, origins)| view! {
            <div class="card">
                <h2>{format!("Effective configuration of {id}")} " " <button on:click=move |_| selected.set(None)>"Close"</button></h2>
                <p class="muted">"Keys set in the instance's file or environment are pinned: the central document cannot change them."</p>
                <table>
                    <thead><tr><th>"Key"</th><th>"Value"</th><th>"From"</th></tr></thead>
                    <tbody>
                        {keys.into_iter().map(|(key, value)| {
                            // secret references are shown whole; their origin is per sub-key
                            let origin = origins.get(&key).and_then(Value::as_str).or_else(|| {
                                origins.as_object()?.iter()
                                    .find(|(k, _)| k.starts_with(&format!("{key}.")))
                                    .and_then(|(_, v)| v.as_str())
                            }).unwrap_or("default").to_string();
                            let local_only = LOCAL_ONLY.iter().any(|p| key == *p || key.starts_with(&format!("{p}.")));
                            let note = if local_only {
                                " (local only)"
                            } else if origin == "file" || origin == "env" {
                                " (pinned)"
                            } else {
                                ""
                            };
                            view! { <tr><td><code>{key}</code></td><td><code>{value}</code></td>
                                <td>{origin}{note}</td></tr> }
                        }).collect::<Vec<_>>()}
                    </tbody>
                </table>
            </div>
        })}
        <h2>"Central configuration"</h2>
        <p class="muted">"Same sections and keys as the service's TOML file. Local-only keys ([rabbitmq], [couchdb], [secrets], [health], [log], [central], instance, security.internal, runner.pool) are refused. Saving validates the document against every live instance; each instance checks it again and restarts to apply it."</p>
        <div class="toolbar">
            <select on:change=move |ev| { service.set(event_target_value(&ev)); load_doc(); }>
                {SERVICES.map(|s| view! { <option value=s selected=move || service.get() == s>{s}</option> })}
            </select>
            <Show when=move || service.get() == "runner">
                <input placeholder="pool (empty = every pool)" prop:value=move || pool.get()
                    on:change=move |ev| { pool.set(event_target_value(&ev)); load_doc(); } />
            </Show>
            <span class="muted">{move || meta.get()}</span>
        </div>
        <textarea class="code" rows="14" prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))
            placeholder="[scheduler]\ncommand_prefetch = 16"></textarea>
        <div class="toolbar">
            <button class="primary" on:click=move |_| save_doc()>"Save"</button>
            <button on:click=move |_| load_doc()>"Reload"</button>
        </div>
    }
}

// ---------------------------------------------------------------- security

#[component]
pub fn SecurityConfig() -> impl IntoView {
    let session = expect_context::<Session>();
    let instances = RwSignal::new(Vec::<InstanceView>::new());
    let error = RwSignal::new(None::<String>);
    let message = RwSignal::new(None::<(bool, String)>);
    poll_instances(instances, error);

    let set_external = move |service: String, level: u32| {
        spawn_local(async move {
            // read-modify-write of the service's central document
            let request = api::GetServiceConfigRequest {
                service: service.clone(),
                pool: String::new(),
            };
            let current = match client()
                .get_service_config(authed(request, &session.token()))
                .await
            {
                Ok(doc) => serde_json::from_str::<Value>(&doc.into_inner().settings_json)
                    .unwrap_or(json!({})),
                Err(status) => return report(message, session, Err(status)),
            };
            let mut settings = current;
            settings["security"]["external"] = json!(level);
            let request = api::SetServiceConfigRequest {
                command_id: command_id("security"),
                service: service.clone(),
                pool: String::new(),
                settings_json: settings.to_string(),
            };
            report(
                message,
                session,
                client()
                    .set_service_config(authed(request, &session.token()))
                    .await
                    .map(|_| {
                        format!(
                            "{service}: external level {} accepted, instances restart to apply it",
                            level_name(level)
                        )
                    }),
            );
        });
    };
    let rows = move || {
        SERVICES
            .iter()
            .map(|service| {
                let live: Vec<InstanceView> = instances
                    .get()
                    .into_iter()
                    .filter(|i| i.summary.service == *service && !i.summary.gone)
                    .collect();
                (service.to_string(), live)
            })
            .collect::<Vec<_>>()
    };
    let warnings = move || {
        let mut list = Vec::new();
        for i in instances.get().iter().filter(|i| !i.summary.gone) {
            let s = &i.summary;
            if s.security_external == 0 && matches!(s.service.as_str(), "backend" | "endpoint") {
                list.push(format!(
                    "{}/{}: external level 0 (open) — reachable without authentication",
                    s.service, s.instance
                ));
            }
            if s.security_internal == 0 {
                list.push(format!("{}/{}: internal level 0 — only for a trusted, isolated network (e.g. inside the all-in-one container)", s.service, s.instance));
            }
            if s.security_internal < 2 && s.security_external >= 2 {
                list.push(format!("{}/{}: external {} but internal {}: broker and database links are not encrypted", s.service, s.instance, s.security_external, s.security_internal));
            }
        }
        list
    };

    view! {
        <ConfigNav />
        <h1>"Security levels"</h1>
        <p class="muted">"Four levels (D19): 0 open, 1 authenticated, 2 encrypted, 3 hardened. The internal level (links to RabbitMQ and CouchDB) is a local startup parameter of each instance; the external level can be set here (central configuration, D21) unless an instance pins it in its file or environment. An instance whose configuration is too weak for its levels refuses it."</p>
        <Show when=move || error.get().is_some()><p class="error">{move || error.get()}</p></Show>
        {message_view(message)}
        <For each=warnings key=|w| w.clone() let:warning>
            <p class="warning">{warning}</p>
        </For>
        <table>
            <thead><tr><th>"Service"</th><th>"Instances"</th><th>"Internal"</th><th>"External"</th><th>"Meaning"</th><th>"Set external"</th></tr></thead>
            <tbody>
                <For each=rows key=|(s, live)| (s.clone(), live.iter().map(|i| (i.summary.instance.clone(), i.summary.security_external, i.summary.security_internal)).collect::<Vec<_>>()) let:row>
                    {
                        let (service, live) = row;
                        let internal: Vec<String> = live.iter().map(|i| level_name(i.summary.security_internal).to_string()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
                        let externals: std::collections::BTreeSet<u32> = live.iter().map(|i| i.summary.security_external).collect();
                        let pinned = live.iter().any(|i| matches!(i.status["origins"]["security.external"].as_str(), Some("file" | "env")));
                        let current = externals.iter().next().copied().unwrap_or(3);
                        let surface = matches!(service.as_str(), "backend" | "endpoint" | "runner" | "sourcesync");
                        let set_service = service.clone();
                        view! {
                            <tr>
                                <td><b>{service.clone()}</b></td>
                                <td>{live.len()}</td>
                                <td>{internal.join(", ")}</td>
                                <td>{externals.iter().map(|l| level_name(*l)).collect::<Vec<_>>().join(", ")}
                                    {pinned.then(|| view! { <div class="muted">"pinned locally on some instances"</div> })}</td>
                                <td class="muted">{external_meaning(&service, current)}</td>
                                <td>{surface.then(|| view! {
                                    <select on:change=move |ev| {
                                        if let Ok(level) = event_target_value(&ev).parse::<u32>() {
                                            let service = set_service.clone();
                                            if confirm(&format!("Set the external level of {service} to {}?", level_name(level))) {
                                                set_external(service, level);
                                            }
                                        }
                                    }>
                                        <option value="">"—"</option>
                                        {(0..=3u32).map(|l| view! { <option value=l.to_string()>{level_name(l)}</option> }).collect::<Vec<_>>()}
                                    </select>
                                })}</td>
                            </tr>
                        }
                    }
                </For>
            </tbody>
        </table>
    }
}

// ---------------------------------------------------------------- settings

#[component]
pub fn SettingsConfig() -> impl IntoView {
    let session = expect_context::<Session>();
    let retention = RwSignal::new(String::new());
    let message = RwSignal::new(None::<(bool, String)>);
    spawn_scoped(async move {
        if let Ok(response) = client()
            .get_settings(authed(api::GetSettingsRequest {}, &session.token()))
            .await
        {
            let values: Value =
                serde_json::from_str(&response.into_inner().settings_json).unwrap_or_default();
            let _ = retention.try_set(
                values["retention_months"]
                    .as_i64()
                    .map(|m| m.to_string())
                    .unwrap_or_default(),
            );
        }
    });
    let save = move || {
        let value = retention.get_untracked();
        spawn_local(async move {
            let request = api::SetSettingsRequest {
                command_id: command_id("settings"),
                key: "retention_months".into(),
                value_json: value.trim().to_string(),
            };
            report(
                message,
                session,
                client()
                    .set_settings(authed(request, &session.token()))
                    .await
                    .map(|_| "retention accepted".to_string()),
            );
        });
    };
    view! {
        <ConfigNav />
        <h1>"Settings"</h1>
        {message_view(message)}
        <div class="card form">
            <label>"Retention (months of jobs and outcomes kept, D8; hosts are never removed by retention)"
                <input type="number" min="1" prop:value=move || retention.get() on:input=move |ev| retention.set(event_target_value(&ev)) />
            </label>
            <div class="toolbar"><button class="primary" on:click=move |_| save()>"Save"</button></div>
        </div>
        <p class="muted">"Each service's own settings are on the Services page (central configuration)."</p>
    }
}
