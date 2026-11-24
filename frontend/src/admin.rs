//! Administration pages of D12/D13: local accounts and API tokens (Users),
//! Cedar policies (Policies) and the certificate deny list of proactive
//! hosts (Revocation, on the Security page). Writes are commands (D10):
//! backend checks and hashes, dbwriter applies, so lists are reloaded a
//! moment after a change.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value;

use crate::Session;
use crate::client::{api, authed, client, command_id};
use crate::config::{confirm, message_view, report};

/// How long dbwriter usually needs to apply a command.
const RELOAD_AFTER: Duration = Duration::from_millis(1200);

type Message = RwSignal<Option<(bool, String)>>;

/// Groups of the shipped role templates (`regencylib::policy::TEMPLATES`).
const ROLE_GROUPS: &[(&str, &str)] = &[
    ("viewers", "read everything"),
    ("operators", "read, Assess, abort"),
    ("enforcers", "read, Assess, Reach, abort"),
    ("admins", "everything"),
];

/// Same rule as backend (`UpsertUser`).
const MIN_PASSWORD: usize = 12;

/// The Cedar schema policies are validated against (D12).
const CEDAR_SCHEMA: &str = include_str!("../../regencylib/src/policy.cedarschema");

// ---------------------------------------------------------------- users

#[derive(Clone, Debug, PartialEq, Default)]
struct UserForm {
    name: String,
    existing: bool,
    service_account: bool,
    groups: String,
    password: String,
    confirm: String,
    disabled: bool,
}

/// `"a, b  c"` → `["a", "b", "c"]`, deduplicated, in order.
fn parse_groups(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for group in text.split([',', ' ', '\n']).map(str::trim) {
        if !group.is_empty() && !out.iter().any(|g| g == group) {
            out.push(group.to_string());
        }
    }
    out
}

impl UserForm {
    fn from_user(user: &api::User) -> Self {
        Self {
            name: user.name.clone(),
            existing: true,
            service_account: user.service_account,
            groups: user.groups.join(", "),
            disabled: user.disabled,
            ..Default::default()
        }
    }

    fn toggle_group(&mut self, group: &str) {
        let mut groups = parse_groups(&self.groups);
        if let Some(i) = groups.iter().position(|g| g == group) {
            groups.remove(i);
        } else {
            groups.push(group.to_string());
        }
        self.groups = groups.join(", ");
    }

    fn to_request(&self) -> Result<api::UpsertUserRequest, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("the name is required".into());
        }
        if name.contains([':', '.']) || name.chars().any(char::is_whitespace) {
            return Err("names cannot contain ':', '.' or spaces".into());
        }
        if self.password != self.confirm {
            return Err("the two passwords differ".into());
        }
        if !self.password.is_empty() && self.password.chars().count() < MIN_PASSWORD {
            return Err(format!("passwords need at least {MIN_PASSWORD} characters"));
        }
        if !self.existing && !self.service_account && self.password.is_empty() {
            return Err("a new user needs a password (service accounts use API tokens)".into());
        }
        // command id filled when sent
        Ok(api::UpsertUserRequest {
            command_id: String::new(),
            name: name.to_string(),
            service_account: self.service_account,
            password: self.password.clone(),
            groups: parse_groups(&self.groups),
            disabled: self.disabled,
        })
    }
}

/// The same account, enabled or disabled; password and tokens kept.
fn toggle_request(user: &api::User, disabled: bool) -> api::UpsertUserRequest {
    api::UpsertUserRequest {
        command_id: command_id("user"),
        name: user.name.clone(),
        service_account: user.service_account,
        password: String::new(),
        groups: user.groups.clone(),
        disabled,
    }
}

fn load_users(session: Session, into: RwSignal<Vec<api::User>>, error: RwSignal<Option<String>>) {
    spawn_local(async move {
        match client()
            .list_users(authed(api::ListUsersRequest {}, &session.token()))
            .await
        {
            Ok(response) => {
                let _ = into.try_set(response.into_inner().users);
                let _ = error.try_set(None);
            }
            Err(status) => {
                session.check(&status);
                let _ = error.try_set(Some(status.message().to_string()));
            }
        }
    });
}

fn upsert_user(
    session: Session,
    message: Message,
    request: api::UpsertUserRequest,
    on_saved: impl Fn() + 'static,
    reload: impl Fn() + Copy + Send + Sync + 'static,
) {
    spawn_local(async move {
        let name = request.name.clone();
        let result = client()
            .upsert_user(authed(request, &session.token()))
            .await
            .map(|_| format!("{name}: accepted, the list updates in a moment"));
        let ok = result.is_ok();
        report(message, session, result);
        if ok {
            on_saved();
            set_timeout(reload, RELOAD_AFTER);
        }
    });
}

#[component]
pub fn UsersConfig() -> impl IntoView {
    let session = expect_context::<Session>();
    let users = RwSignal::new(Vec::<api::User>::new());
    let error = RwSignal::new(None::<String>);
    let message: Message = RwSignal::new(None);
    // a token just created: (account, token), shown once
    let fresh_token = RwSignal::new(None::<(String, String)>);
    let reload = move || load_users(session, users, error);
    reload();

    let form = RwSignal::new(None::<UserForm>);
    let save = move || {
        let Some(current) = form.get_untracked() else {
            return;
        };
        match current.to_request() {
            Err(e) => message.set(Some((false, e))),
            Ok(request) => upsert_user(
                session,
                message,
                api::UpsertUserRequest {
                    command_id: command_id("user"),
                    ..request
                },
                move || {
                    let _ = form.try_set(None);
                },
                reload,
            ),
        }
    };
    let create_token = move |account: String| {
        spawn_local(async move {
            let request = api::CreateApiTokenRequest {
                command_id: command_id("token"),
                account: account.clone(),
            };
            match client()
                .create_api_token(authed(request, &session.token()))
                .await
            {
                Ok(response) => {
                    let token = response.into_inner().token;
                    let _ = message.try_set(None);
                    let _ = fresh_token.try_set(Some((account, token)));
                    set_timeout(reload, RELOAD_AFTER);
                }
                Err(status) => report(message, session, Err(status)),
            }
        });
    };
    let revoke_token = move |account: String, token_id: String| {
        spawn_local(async move {
            let request = api::RevokeApiTokenRequest {
                command_id: command_id("token-revoke"),
                account: account.clone(),
                token_id: token_id.clone(),
            };
            let result = client()
                .revoke_api_token(authed(request, &session.token()))
                .await
                .map(|_| format!("token {token_id} of {account}: revocation accepted"));
            report(message, session, result);
            set_timeout(reload, RELOAD_AFTER);
        });
    };
    let delete_user = move |name: String| {
        spawn_local(async move {
            let request = api::DeleteUserRequest {
                command_id: command_id("user-delete"),
                name: name.clone(),
            };
            let result = client()
                .delete_user(authed(request, &session.token()))
                .await
                .map(|_| format!("{name}: deletion accepted"));
            report(message, session, result);
            set_timeout(reload, RELOAD_AFTER);
        });
    };

    let edit = move |f: fn(&mut UserForm, String)| {
        move |ev: leptos::ev::Event| {
            let value = event_target_value(&ev);
            form.update(|form| {
                if let Some(form) = form {
                    f(form, value)
                }
            });
        }
    };
    let read = move |f: fn(&UserForm) -> String| {
        move || form.with(|form| form.as_ref().map(f).unwrap_or_default())
    };
    let flag =
        move |f: fn(&UserForm) -> bool| move || form.with(|form| form.as_ref().is_some_and(f));

    view! {
        <h1>"Users " <span class="muted">"(local accounts, service accounts and their API tokens)"</span></h1>
        <Show when=move || error.get().is_some()><p class="error">{move || error.get()}</p></Show>
        {message_view(message)}
        {move || fresh_token.get().map(|(account, token)| view! {
            <div class="card">
                <h2>{format!("New API token of {account}")}</h2>
                <p class="warning">"Copy it now: it is shown only once (regency keeps only its hash)."</p>
                {crate::onboarding::snippet(token)}
                <p class="muted">"Clients send it as " <code>"authorization: Bearer <token>"</code> "."</p>
                <div class="toolbar"><button on:click=move |_| fresh_token.set(None)>"Done"</button></div>
            </div>
        })}
        <div class="toolbar">
            <button class="primary" on:click=move |_| { message.set(None); form.set(Some(UserForm::default())) }>"New account"</button>
            <button on:click=move |_| reload()>"Refresh list"</button>
        </div>
        <Show when=move || form.get().is_some()>
            <div class="card form">
                <h2>{move || if flag(|f| f.existing)() { "Edit account" } else { "New account" }}</h2>
                <label>"Name"
                    <input prop:value=read(|f| f.name.clone()) placeholder="alice"
                        prop:disabled=flag(|f| f.existing)
                        on:input=edit(|f, v| f.name = v) />
                </label>
                <label>"Kind"
                    <select prop:disabled=flag(|f| f.existing)
                        on:change=edit(|f, v| f.service_account = v == "service")>
                        <option value="user" selected=move || !flag(|f| f.service_account)()>"User (password login)"</option>
                        <option value="service" selected=flag(|f| f.service_account)>"Service account (API tokens only)"</option>
                    </select>
                </label>
                <label>"Groups (comma separated; policies grant rights to groups)"
                    <input prop:value=read(|f| f.groups.clone()) placeholder="operators, dba"
                        on:input=edit(|f, v| f.groups = v) />
                </label>
                <div class="chip-line">
                    <span class="muted">"Shipped roles:"</span>
                    {ROLE_GROUPS.iter().map(|(group, rights)| {
                        let group = *group;
                        let active = move || form.with(|f| f.as_ref().is_some_and(|f| parse_groups(&f.groups).iter().any(|g| g == group)));
                        view! {
                            <span class="chip" class:active=active title=*rights>
                                <button class="chip-text" on:click=move |_| form.update(|f| if let Some(f) = f { f.toggle_group(group) })>{group}</button>
                            </span>
                        }
                    }).collect::<Vec<_>>()}
                </div>
                <Show when=move || !flag(|f| f.service_account)()>
                    <label>{move || if flag(|f| f.existing)() { "New password (empty: unchanged)" } else { "Password" }}
                        <input type="password" autocomplete="new-password" prop:value=read(|f| f.password.clone())
                            placeholder=format!("{MIN_PASSWORD} characters at least")
                            on:input=edit(|f, v| f.password = v) />
                    </label>
                    <label>"Password again"
                        <input type="password" autocomplete="new-password" prop:value=read(|f| f.confirm.clone())
                            on:input=edit(|f, v| f.confirm = v) />
                    </label>
                </Show>
                <label>"Disabled "
                    <input type="checkbox" prop:checked=flag(|f| f.disabled)
                        on:change=move |ev| { let on = event_target_checked(&ev); form.update(|f| if let Some(f) = f { f.disabled = on }); } />
                </label>
                <p class="muted">"Passwords are hashed by backend before anything is stored or sent further. OIDC users are managed by their provider and do not appear here."</p>
                <div class="toolbar">
                    <button class="primary" on:click=move |_| save()>"Save"</button>
                    <button on:click=move |_| form.set(None)>"Cancel"</button>
                </div>
            </div>
        </Show>
        <table>
            <thead><tr>
                <th>"Name"</th><th>"Kind"</th><th>"Groups"</th><th>"State"</th><th>"API tokens"</th><th></th>
            </tr></thead>
            <tbody>
                <For each=move || users.get() key=|u| format!("{u:?}") let:user>
                    {
                        let name = user.name.clone();
                        let disabled = user.disabled;
                        let bootstrap = user.bootstrap;
                        let (to_edit, to_toggle) = (user.clone(), user.clone());
                        let (token_for, delete_name) = (name.clone(), name.clone());
                        let tokens = user.token_ids.iter().map(|token_id| {
                            let (account, token_id) = (name.clone(), token_id.clone());
                            let label = token_id.clone();
                            view! {
                                <span class="chip">
                                    <code class="chip-text">{label}</code>
                                    <button class="chip-x" title="Revoke this token" on:click=move |_| {
                                        if confirm(&format!("Revoke the API token {token_id} of {account}? Clients using it are refused at once.")) {
                                            revoke_token(account.clone(), token_id.clone());
                                        }
                                    }>"✕"</button>
                                </span>
                            }
                        }).collect::<Vec<_>>();
                        let no_token = user.token_ids.is_empty();
                        view! {
                            <tr>
                                <td><b>{name.clone()}</b>
                                    {bootstrap.then(|| view! { " " <span class="badge unknown" title="Created at installation; always authorized; can be disabled, not deleted">"bootstrap admin"</span> })}</td>
                                <td>{if user.service_account { "Service account" } else { "User" }}</td>
                                <td>{if user.groups.is_empty() {
                                    view! { <span class="muted">"none"</span> }.into_any()
                                } else {
                                    user.groups.iter().map(|g| view! { <span class="chip"><span class="chip-text">{g.clone()}</span></span> " " }).collect::<Vec<_>>().into_any()
                                }}</td>
                                <td>{if disabled {
                                    view! { <span class="badge not_compliant">"Disabled"</span> }.into_any()
                                } else {
                                    view! { <span class="badge compliant">"Enabled"</span> }.into_any()
                                }}</td>
                                <td><div class="chip-line">
                                    {tokens}
                                    {no_token.then(|| view! { <span class="muted">"none"</span> })}
                                </div></td>
                                <td>
                                    <button on:click=move |_| { message.set(None); form.set(Some(UserForm::from_user(&to_edit))) }>"Edit"</button>
                                    <button on:click=move |_| {
                                        let user = to_toggle.clone();
                                        let warn = if user.bootstrap && !disabled { " It is the bootstrap admin: make sure another admin can log in first." } else { "" };
                                        if confirm(&format!("{} {}?{warn}", if disabled { "Enable" } else { "Disable" }, user.name)) {
                                            upsert_user(session, message, toggle_request(&user, !disabled), || {}, reload);
                                        }
                                    }>{if disabled { "Enable" } else { "Disable" }}</button>
                                    <button title="Create an API token for this account" on:click=move |_| create_token(token_for.clone())>"New API token"</button>
                                    {(!bootstrap).then(|| view! {
                                        <button on:click=move |_| {
                                            let name = delete_name.clone();
                                            if confirm(&format!("Delete the account {name} and its API tokens?")) {
                                                delete_user(name);
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

// ---------------------------------------------------------------- policies

#[derive(Clone, Debug, PartialEq, Default)]
struct PolicyForm {
    policy_id: String,
    existing: bool,
    description: String,
    text: String,
}

const NEW_POLICY: &str = r#"// Members of group "dba" may read everything, and Assess / Reach
// the hosts of the assignment ui/db.
permit(principal in Group::"dba", action == Action::"read", resource);
permit(
  principal in Group::"dba",
  action in [Action::"assess", Action::"reach"],
  resource in Assignment::"ui/db"
);
"#;

impl PolicyForm {
    fn new() -> Self {
        Self {
            text: NEW_POLICY.into(),
            ..Default::default()
        }
    }

    fn from_policy(policy: &api::Policy) -> Self {
        Self {
            policy_id: policy.policy_id.clone(),
            existing: true,
            description: policy.description.clone(),
            text: policy.text.clone(),
        }
    }

    fn to_request(&self) -> Result<api::UpsertPolicyRequest, String> {
        let policy_id = self.policy_id.trim();
        if policy_id.is_empty() {
            return Err("the policy id is required".into());
        }
        if policy_id.starts_with("builtin-") {
            return Err("builtin-* ids are reserved for the shipped roles".into());
        }
        if self.text.trim().is_empty() {
            return Err("the policy text is empty".into());
        }
        // command id filled when sent
        Ok(api::UpsertPolicyRequest {
            command_id: String::new(),
            policy_id: policy_id.to_string(),
            text: self.text.clone(),
            description: self.description.trim().to_string(),
        })
    }
}

fn load_policies(
    session: Session,
    into: RwSignal<Vec<api::Policy>>,
    error: RwSignal<Option<String>>,
) {
    spawn_local(async move {
        match client()
            .list_policies(authed(api::ListPoliciesRequest {}, &session.token()))
            .await
        {
            Ok(response) => {
                let _ = into.try_set(response.into_inner().policies);
                let _ = error.try_set(None);
            }
            Err(status) => {
                session.check(&status);
                let _ = error.try_set(Some(status.message().to_string()));
            }
        }
    });
}

#[component]
pub fn PoliciesConfig() -> impl IntoView {
    let session = expect_context::<Session>();
    let policies = RwSignal::new(Vec::<api::Policy>::new());
    let error = RwSignal::new(None::<String>);
    let message: Message = RwSignal::new(None);
    let reload = move || load_policies(session, policies, error);
    reload();

    let form = RwSignal::new(None::<PolicyForm>);
    let save = move || {
        let Some(current) = form.get_untracked() else {
            return;
        };
        let request = match current.to_request() {
            Ok(request) => api::UpsertPolicyRequest {
                command_id: command_id("policy"),
                ..request
            },
            Err(e) => return message.set(Some((false, e))),
        };
        spawn_local(async move {
            let id = request.policy_id.clone();
            let result = client()
                .upsert_policy(authed(request, &session.token()))
                .await
                .map(|_| {
                    format!("{id}: valid and accepted, every backend replica loads it in a moment")
                });
            let ok = result.is_ok();
            report(message, session, result);
            if ok {
                let _ = form.try_set(None);
                set_timeout(reload, RELOAD_AFTER);
            }
        });
    };
    let delete = move |policy_id: String| {
        spawn_local(async move {
            let request = api::DeletePolicyRequest {
                command_id: command_id("policy-delete"),
                policy_id: policy_id.clone(),
            };
            let result = client()
                .delete_policy(authed(request, &session.token()))
                .await
                .map(|_| format!("{policy_id}: deletion accepted"));
            report(message, session, result);
            set_timeout(reload, RELOAD_AFTER);
        });
    };
    let edit = move |f: fn(&mut PolicyForm, String)| {
        move |ev: leptos::ev::Event| {
            let value = event_target_value(&ev);
            form.update(|form| {
                if let Some(form) = form {
                    f(form, value)
                }
            });
        }
    };
    let read = move |f: fn(&PolicyForm) -> String| {
        move || form.with(|form| form.as_ref().map(f).unwrap_or_default())
    };
    let existing = move || form.with(|f| f.as_ref().is_some_and(|f| f.existing));

    view! {
        <h1>"Policies " <span class="muted">"(Cedar authorization rules)"</span></h1>
        <Show when=move || error.get().is_some()><p class="error">{move || error.get()}</p></Show>
        {message_view(message)}
        <p class="muted">"A request is allowed when a policy permits it and none forbids it. Rights go to groups (set on Users, or from OIDC). The bootstrap admin is always allowed."</p>
        <div class="toolbar">
            <button class="primary" on:click=move |_| { message.set(None); form.set(Some(PolicyForm::new())) }>"New policy"</button>
            <button on:click=move |_| reload()>"Refresh list"</button>
        </div>
        <Show when=move || form.get().is_some()>
            <div class="card form">
                <h2>{move || if existing() { "Edit policy" } else { "New policy" }}</h2>
                <label>"Id"
                    <input prop:value=read(|f| f.policy_id.clone()) placeholder="dba-db-hosts"
                        prop:disabled=existing on:input=edit(|f, v| f.policy_id = v) />
                </label>
                <label>"Description"
                    <input prop:value=read(|f| f.description.clone()) placeholder="DBAs run the database hosts"
                        on:input=edit(|f, v| f.description = v) />
                </label>
                <label>"Policy (Cedar; checked against the schema below when saved)"
                    <textarea class="code" spellcheck="false" prop:value=read(|f| f.text.clone())
                        on:input=edit(|f, v| f.text = v)></textarea>
                </label>
                <details>
                    <summary>"Schema: principals, actions and resources"</summary>
                    <pre><code>{CEDAR_SCHEMA}</code></pre>
                    <p class="muted">"Resource ids: " <code>"Host::\"web-01\""</code> ", " <code>"Assignment::\"<source>/<name>\""</code> ", " <code>"Source::\"<source>\""</code> ", " <code>"ExpectedState::\"<source>/<name>\""</code> ", " <code>"Order::\"<name>\""</code> ", " <code>"Settings::\"global\""</code> "."</p>
                </details>
                <div class="toolbar">
                    <button class="primary" on:click=move |_| save()>"Save"</button>
                    <button on:click=move |_| form.set(None)>"Cancel"</button>
                </div>
            </div>
        </Show>
        <For each=move || policies.get() key=|p| format!("{p:?}") let:policy>
            {
                let builtin = policy.builtin;
                let id = policy.policy_id.clone();
                let to_edit = policy.clone();
                view! {
                    <div class="card">
                        <h2>{id.clone()} " "
                            {builtin.then(|| view! { <span class="badge unknown" title="Shipped with regency, always loaded, read-only">"shipped role"</span> })}</h2>
                        {(!policy.description.is_empty()).then(|| view! { <p class="muted">{policy.description.clone()}</p> })}
                        <pre><code>{policy.text.clone()}</code></pre>
                        {(!builtin).then(|| view! {
                            <div class="toolbar">
                                <button on:click=move |_| { message.set(None); form.set(Some(PolicyForm::from_policy(&to_edit))) }>"Edit"</button>
                                <button on:click=move |_| {
                                    let id = id.clone();
                                    if confirm(&format!("Delete the policy {id}? The rights it grants are withdrawn at once.")) {
                                        delete(id);
                                    }
                                }>"Delete"</button>
                            </div>
                        })}
                    </div>
                }
            }
        </For>
    }
}

// ---------------------------------------------------------------- revocation

/// The deny lists of D13 (settings keys, see `regencylib::model`).
const DENY_LISTS: [(&str, &str, &str); 3] = [
    (
        "revoked_hosts",
        "Host ids (whatever their certificate)",
        "web-02",
    ),
    (
        "revoked_serials",
        "Certificate serial numbers (hex)",
        "7f3a…",
    ),
    (
        "revoked_fingerprints",
        "Certificate SHA-256 fingerprints",
        "AB:CD:…",
    ),
];

/// One entry per line (or comma separated) → JSON list.
fn lines_to_list(text: &str) -> String {
    let items: Vec<&str> = text
        .split(['\n', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    Value::from(items).to_string()
}

fn list_to_lines(value: &Value) -> String {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Proactive hosts refused by the endpoint (D13 deny list), on the
/// Security page.
#[component]
pub fn Revocation() -> impl IntoView {
    let session = expect_context::<Session>();
    let lists = DENY_LISTS.map(|_| RwSignal::new(String::new()));
    let message: Message = RwSignal::new(None);
    let load = move || {
        spawn_local(async move {
            match client()
                .get_settings(authed(api::GetSettingsRequest {}, &session.token()))
                .await
            {
                Ok(response) => {
                    let values: Value = serde_json::from_str(&response.into_inner().settings_json)
                        .unwrap_or_default();
                    for ((key, _, _), list) in DENY_LISTS.iter().zip(lists) {
                        let _ = list.try_set(list_to_lines(&values[*key]));
                    }
                }
                Err(status) => report(message, session, Err(status)),
            }
        });
    };
    load();
    let save = move || {
        let values: Vec<(String, String)> = DENY_LISTS
            .iter()
            .zip(lists)
            .map(|((key, _, _), list)| (key.to_string(), lines_to_list(&list.get_untracked())))
            .collect();
        spawn_local(async move {
            for (key, value_json) in values {
                let request = api::SetSettingsRequest {
                    command_id: command_id("revocation"),
                    key,
                    value_json,
                };
                if let Err(status) = client()
                    .set_settings(authed(request, &session.token()))
                    .await
                {
                    return report(message, session, Err(status));
                }
            }
            let _ = message.try_set(Some((
                true,
                "deny lists accepted: every endpoint replica applies them in a moment".into(),
            )));
            set_timeout(load, RELOAD_AFTER);
        });
    };
    view! {
        <h2>"Certificate revocation " <span class="muted">"(proactive hosts)"</span></h2>
        {message_view(message)}
        <div class="card form">
            <p class="muted">"The endpoint refuses every connection matching one of these lists, one entry per line. Deleting a host also drops its pinned self-signed certificate; revoking keeps it out even if it enrolls again."</p>
            {DENY_LISTS.iter().zip(lists).map(|((_, label, placeholder), list)| view! {
                <label>{*label}
                    <textarea spellcheck="false" rows="4" placeholder=*placeholder
                        prop:value=move || list.get() on:input=move |ev| list.set(event_target_value(&ev))></textarea>
                </label>
            }).collect::<Vec<_>>()}
            <div class="toolbar">
                <button class="primary" on:click=move |_| save()>"Save deny lists"</button>
                <button on:click=move |_| load()>"Reload"</button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_are_parsed_and_toggled() {
        assert_eq!(parse_groups(" a, b  c,a "), ["a", "b", "c"]);
        let mut form = UserForm {
            groups: "operators".into(),
            ..Default::default()
        };
        form.toggle_group("admins");
        assert_eq!(form.groups, "operators, admins");
        form.toggle_group("operators");
        assert_eq!(form.groups, "admins");
    }

    #[test]
    fn user_form_checks_what_backend_would_refuse() {
        let mut form = UserForm {
            name: "alice".into(),
            ..Default::default()
        };
        assert!(form.to_request().is_err(), "new user without password");
        form.password = "short".into();
        form.confirm = "short".into();
        assert!(form.to_request().is_err());
        form.password = "a long enough one".into();
        assert!(form.to_request().is_err(), "confirmation differs");
        form.confirm = form.password.clone();
        form.groups = "operators, ops".into();
        let request = form.to_request().unwrap();
        assert_eq!(request.groups, ["operators", "ops"]);
        form.name = "a.b".into();
        assert!(form.to_request().is_err());

        let robot = UserForm {
            name: "ci".into(),
            service_account: true,
            ..Default::default()
        };
        assert!(robot.to_request().unwrap().password.is_empty());
        let existing = UserForm {
            name: "bob".into(),
            existing: true,
            ..Default::default()
        };
        assert!(existing.to_request().is_ok(), "password kept");
    }

    #[test]
    fn policy_ids_are_checked() {
        let mut form = PolicyForm::new();
        assert!(form.to_request().is_err());
        form.policy_id = "builtin-x".into();
        assert!(form.to_request().is_err());
        form.policy_id = " dba ".into();
        assert_eq!(form.to_request().unwrap().policy_id, "dba");
    }

    #[test]
    fn deny_lists_round_trip_as_lines() {
        assert_eq!(
            lines_to_list(" web-02\n\nweb-03, db-01 "),
            r#"["web-02","web-03","db-01"]"#
        );
        assert_eq!(list_to_lines(&serde_json::json!(["a", "b"])), "a\nb");
        assert_eq!(list_to_lines(&Value::Null), "");
    }
}
