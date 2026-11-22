//! The "Add a host" dialog of the Hosts page (D25): everything needed to
//! manage a host right after installation, both ways (one tab each):
//! - hosts reached by runners over SSH: the runners' public keys (advertised
//!   in their instance status) to authorize on the host, and a short form
//!   creating the host definition with the pool's key;
//! - hosts running regops: an enrollment token for a host id, the endpoint's
//!   CA certificate and a ready `[Regency]` section (endpoint URL: the
//!   `endpoint_url` setting, else this page's host and the endpoint's port).

use std::collections::BTreeMap;
use std::time::Duration;

use js_sys::wasm_bindgen::JsCast;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

use crate::Session;
use crate::client::{api, authed, client, format_ms};
use crate::config::{
    Entry, InstanceView, load_catalog_with, load_instances, message_view, parse_pairs, report,
    split_list, str_of, upsert_item,
};
use crate::pages::{sleep_ms, spawn_scoped};

/// Where regops keeps the files written by the commands below.
const CA_PATH: &str = "/etc/regops/regency-ca.pem";
const TOKEN_PATH: &str = "/etc/regops/regency-enrollment.token";

/// One SSH identity of a runner pool.
#[derive(Clone, Debug, PartialEq)]
pub struct RunnerKey {
    pub public_key: String,
    /// Private key file on the runners (the host's secret reference).
    pub path: String,
}

/// Runner pools and their SSH identities (live instances only; runners of a
/// pool on several machines each have their own key, at the same path).
pub fn runner_keys(instances: &[InstanceView]) -> BTreeMap<String, Vec<RunnerKey>> {
    let mut pools: BTreeMap<String, Vec<RunnerKey>> = BTreeMap::new();
    for instance in instances {
        if instance.summary.service != "runner" || instance.summary.gone {
            continue;
        }
        let status = &instance.status;
        let pool = status["pool"]
            .as_str()
            .or(status["config"]["runner"]["pool"].as_str())
            .unwrap_or("default")
            .to_string();
        let keys = pools.entry(pool).or_default();
        let (Some(public_key), Some(path)) = (
            status["advertised"]["ssh_public_key"].as_str(),
            status["config"]["runner"]["ssh_key"].as_str(),
        ) else {
            continue;
        };
        let key = RunnerKey {
            public_key: public_key.to_string(),
            path: path.to_string(),
        };
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    pools
}

/// What regops agents need from the endpoint.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EndpointInfo {
    pub port: String,
    pub names: Vec<String>,
    pub ca_cert: Option<String>,
}

pub fn endpoint_info(instances: &[InstanceView]) -> Option<EndpointInfo> {
    instances
        .iter()
        .filter(|i| i.summary.service == "endpoint" && !i.summary.gone)
        .map(|i| {
            let advertised = &i.status["advertised"];
            EndpointInfo {
                port: str_of(advertised, "port"),
                names: split_list(&str_of(advertised, "server_names")),
                ca_cert: advertised["ca_cert"].as_str().map(str::to_string),
            }
        })
        // prefer an instance that publishes what is needed
        .max_by_key(|e| (e.ca_cert.is_some(), !e.names.is_empty()))
}

/// The endpoint URL agents use: the setting, else this page's host.
pub fn endpoint_url(setting: &str, page_host: &str, port: &str) -> String {
    if !setting.trim().is_empty() {
        return setting.trim().trim_end_matches('/').to_string();
    }
    let host = if page_host.contains(':') {
        format!("[{page_host}]")
    } else {
        page_host.to_string()
    };
    let port = if port.is_empty() { "8443" } else { port };
    format!("https://{host}:{port}")
}

/// The host part of an `https://host[:port]` URL.
fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split('/').next().unwrap_or_default();
    if let Some(inner) = authority.strip_prefix('[') {
        return inner.split(']').next().unwrap_or_default().to_string();
    }
    authority
        .rsplit_once(':')
        .map_or(authority, |(h, _)| h)
        .to_string()
}

/// regops' `server_name` when the URL's host is not in the certificate: a
/// DNS name the certificate has (`regency` first).
pub fn server_name_for(url: &str, names: &[String]) -> Option<String> {
    let host = url_host(url).to_lowercase();
    if names.is_empty() || names.iter().any(|n| n.to_lowercase() == host) {
        return None;
    }
    let dns = |n: &&String| n.parse::<std::net::IpAddr>().is_err() && *n != "localhost";
    names
        .iter()
        .find(|n| *n == "regency")
        .or_else(|| names.iter().find(dns))
        .cloned()
}

/// A valid host id for regops (it must be a DNS name, like its certificate's).
pub fn valid_host_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 253
        && id.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Shell commands writing the CA and the token for regops.
pub fn regops_commands(ca_cert: &str, token: &str) -> String {
    format!(
        "sudo install -d -m 0755 /etc/regops\n\
         sudo tee {CA_PATH} >/dev/null <<'EOF'\n{ca}\nEOF\n\
         echo '{token}' | sudo tee {TOKEN_PATH} >/dev/null\n\
         sudo chgrp regops {TOKEN_PATH} && sudo chmod 0640 {TOKEN_PATH}\n",
        ca = ca_cert.trim_end(),
    )
}

/// The lines to put in regops' `/etc/regops/config.toml`.
pub fn regops_config(url: &str, server_name: Option<&str>, host_id: &str) -> String {
    let mut text = format!(
        "# top-level key: before the first [Section]\n\
         Reporting = \"Regency\"\n\n\
         [Regency]\n\
         endpoint = \"{url}\"\n\
         ca_cert_path = \"{CA_PATH}\"\n"
    );
    if let Some(name) = server_name {
        text.push_str(&format!(
            "# the endpoint's certificate does not name {}\nserver_name = \"{name}\"\n",
            url_host(url)
        ));
    }
    text.push_str(&format!(
        "host_id = \"{host_id}\"\n\
         tls_mode = \"BuiltInCa\"\n\
         enrollment_token_path = \"{TOKEN_PATH}\"\n"
    ));
    text
}

/// The SSH host form.
#[derive(Clone, Debug, Default)]
pub struct SshHostForm {
    pub host_id: String,
    pub address: String,
    pub user: String,
    pub pool: String,
    pub labels: String,
    pub fingerprints: String,
}

impl SshHostForm {
    /// The host definition (catalog item of `source_id`), the runner key at
    /// `key_path` as its credentials.
    pub fn to_item(&self, source_id: &str, key_path: &str) -> Result<Value, String> {
        let host_id = self.host_id.trim();
        if host_id.is_empty() || host_id.contains(['/', ':']) {
            return Err("host id: required, without '/' or ':'".into());
        }
        let address = self.address.trim();
        if address.is_empty() {
            return Err("address: the host's name or IP address".into());
        }
        // a port is added unless given (IPv6 addresses go in brackets)
        let endpoint = if address.parse::<std::net::Ipv6Addr>().is_ok() {
            format!("[{address}]:22")
        } else if address.starts_with('[') || address.contains(':') {
            address.to_string()
        } else {
            format!("{address}:22")
        };
        let user = self.user.trim();
        if user.is_empty() {
            return Err("user: the account the runner logs in as".into());
        }
        let mut ssh = json!({
            "AuthMethod": { "Key": {
                "Username": user,
                "Key": { "SecRef": key_path, "Provider": "files" },
            } },
        });
        let fingerprints = split_list(&self.fingerprints);
        if let Some(bad) = fingerprints.iter().find(|f| !f.starts_with("SHA256:")) {
            return Err(format!("host key `{bad}`: write SHA256:… (ssh-keygen -lf)"));
        }
        if !fingerprints.is_empty() {
            ssh["HostKey"] = json!({ "Fingerprints": fingerprints });
        }
        let labels = parse_pairs(&self.labels)?;
        let own: BTreeMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
            .collect();
        crate::labels::check_user_labels(&own)?;
        let mut item = json!({
            "type": "hostdef",
            "source_id": source_id,
            "host_id": host_id,
            "builder": {
                "Id": host_id,
                "Endpoint": endpoint,
                "HostConnectionMethod": { "Ssh2": ssh },
            },
            "labels": labels,
        });
        if !self.pool.is_empty() && self.pool != "default" {
            item["pool"] = json!(self.pool);
        }
        Ok(item)
    }
}

fn copy(text: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let clipboard = js_sys::Reflect::get(&window, &"navigator".into())
        .and_then(|navigator| js_sys::Reflect::get(&navigator, &"clipboard".into()))
        .ok();
    let write = clipboard
        .as_ref()
        .and_then(|c| js_sys::Reflect::get(c, &"writeText".into()).ok())
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
    if let (Some(clipboard), Some(write)) = (clipboard, write) {
        let _ = write.call1(&clipboard, &text.into());
    }
}

/// A block of text to paste somewhere, with a Copy button.
fn snippet(text: String) -> impl IntoView {
    let copied = RwSignal::new(false);
    let to_copy = text.clone();
    view! {
        <div class="snippet">
            <button class="copy" on:click=move |_| {
                copy(&to_copy);
                copied.set(true);
                set_timeout(move || { let _ = copied.try_set(false); }, Duration::from_secs(2));
            }>{move || if copied.get() { "Copied" } else { "Copy" }}</button>
            <pre>{text}</pre>
        </div>
    }
}

/// The dialog's two ways (tabs).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Way {
    Ssh,
    Regops,
}

/// "Add a host" (D25), opened from the Hosts page.
#[component]
pub fn AddHostDialog(on_close: Callback<()>) -> impl IntoView {
    let session = expect_context::<Session>();
    let way = RwSignal::new(Way::Ssh);
    let instances = RwSignal::new(Vec::<InstanceView>::new());
    let sources = RwSignal::new(Vec::<Entry>::new());
    let error = RwSignal::new(None::<String>);
    let message = RwSignal::new(None::<(bool, String)>);
    let setting_url = RwSignal::new(String::new());
    let reload = move || {
        load_instances(session, instances, error);
        load_catalog_with(session, "source", sources, error);
    };
    reload();
    // instances publish their status every 30 s (the first one may be lost
    // on a brand-new broker): follow them while the dialog is open
    spawn_scoped(async move {
        loop {
            sleep_ms(10_000).await;
            load_instances(session, instances, error);
        }
    });
    spawn_scoped(async move {
        if let Ok(response) = client()
            .get_settings(authed(api::GetSettingsRequest {}, &session.token()))
            .await
        {
            let values: Value =
                serde_json::from_str(&response.into_inner().settings_json).unwrap_or_default();
            let _ = setting_url.try_set(str_of(&values, "endpoint_url"));
        }
    });

    let pools = Memo::new(move |_| instances.with(|i| runner_keys(i)));
    let endpoint = Memo::new(move |_| instances.with(|i| endpoint_info(i)));
    let page_host = web_sys::window()
        .and_then(|w| w.location().hostname().ok())
        .unwrap_or_else(|| "localhost".into());
    let url = Memo::new(move |_| {
        let port = endpoint.with(|e| e.as_ref().map(|e| e.port.clone()).unwrap_or_default());
        endpoint_url(&setting_url.get(), &page_host, &port)
    });
    let server_name = Memo::new(move |_| {
        endpoint.with(|e| {
            e.as_ref()
                .and_then(|e| server_name_for(&url.get(), &e.names))
        })
    });

    // ---- SSH hosts
    let ssh = RwSignal::new(SshHostForm {
        pool: "default".into(),
        ..Default::default()
    });
    let created = RwSignal::new(None::<String>);
    let ensure_source = move || async move {
        let editable = sources.with_untracked(|s| {
            s.iter()
                .find(|e| str_of(&e.item, "kind") != "git")
                .map(|e| str_of(&e.item, "source_id"))
        });
        match editable {
            Some(source_id) => Ok(source_id),
            None => {
                let item = json!({ "type": "source", "source_id": "ui", "kind": "frontend" });
                upsert_item(session, item).await?;
                // dbwriter applies commands concurrently: a host sent now
                // could arrive before its source and be refused
                for _ in 0..20 {
                    sleep_ms(500).await;
                    let request = api::ListCatalogRequest {
                        source_id: "ui".into(),
                        r#type: "source".into(),
                    };
                    let listed = client()
                        .list_catalog(authed(request, &session.token()))
                        .await
                        .is_ok_and(|r| !r.into_inner().items.is_empty());
                    if listed {
                        load_catalog_with(session, "source", sources, error);
                        return Ok("ui".to_string());
                    }
                }
                Err(tonic::Status::unavailable(
                    "source “ui” not created yet: try again in a moment",
                ))
            }
        }
    };
    let add_ssh = move || {
        let form = ssh.get_untracked();
        let key_path = pools.with_untracked(|p| {
            p.get(&form.pool)
                .and_then(|keys| keys.first())
                .map(|k| k.path.clone())
        });
        let Some(key_path) = key_path else {
            message.set(Some((
                false,
                format!("no runner of pool {} has an SSH key", form.pool),
            )));
            return;
        };
        spawn_local(async move {
            let source_id = match ensure_source().await {
                Ok(id) => id,
                Err(status) => return report(message, session, Err(status)),
            };
            match form.to_item(&source_id, &key_path) {
                Err(e) => {
                    let _ = message.try_set(Some((false, e)));
                }
                Ok(item) => {
                    let result = upsert_item(session, item).await;
                    if result.is_ok() {
                        let _ = created.try_set(Some(form.host_id.trim().to_string()));
                        let _ = ssh.try_update(|f| {
                            f.host_id.clear();
                            f.address.clear();
                            f.fingerprints.clear();
                        });
                    }
                    report(message, session, result);
                }
            }
        });
    };
    let ssh_input = move |label: &'static str,
                          get: fn(&SshHostForm) -> String,
                          set: fn(&mut SshHostForm, String),
                          hint: &'static str| {
        view! {
            <label>{label}
                <input prop:value=move || get(&ssh.get()) placeholder=hint
                    on:input=move |ev| { let v = event_target_value(&ev); ssh.update(|f| set(f, v)); } />
            </label>
        }
    };

    let pool_names = move || -> Vec<String> {
        pools
            .get()
            .into_iter()
            .filter(|(_, keys)| !keys.is_empty())
            .map(|(pool, _)| pool)
            .collect()
    };

    // ---- regops hosts
    let agent_id = RwSignal::new(String::new());
    let ttl_hours = RwSignal::new("24".to_string());
    // (host id, token, expires at ms)
    let token = RwSignal::new(None::<(String, String, i64)>);
    let issue = move || {
        let host_id = agent_id.get_untracked().trim().to_lowercase();
        if !valid_host_id(&host_id) {
            message.set(Some((false, "host id: a DNS name (letters, digits, '-', '.'); by default regops uses the machine's hostname".into())));
            return;
        }
        let Ok(hours) = ttl_hours.get_untracked().trim().parse::<u32>() else {
            message.set(Some((
                false,
                "validity: a number of hours (1 to 168)".into(),
            )));
            return;
        };
        spawn_local(async move {
            let request = api::IssueEnrollmentTokenRequest {
                host_id: host_id.clone(),
                ttl_sec: hours.clamp(1, 168) * 3600,
            };
            match client()
                .issue_enrollment_token(authed(request, &session.token()))
                .await
            {
                Ok(response) => {
                    let response = response.into_inner();
                    let _ =
                        token.try_set(Some((host_id, response.token, response.expires_at * 1000)));
                    let _ = message.try_set(None);
                }
                Err(status) => report(message, session, Err(status)),
            }
        });
    };
    let ca_cert = move || endpoint.with(|e| e.as_ref().and_then(|e| e.ca_cert.clone()));

    view! {
        <div class="overlay" on:click=move |_| on_close.run(())>
        <div class="dialog wide add-host" on:click=|ev| ev.stop_propagation()>
        <div class="dialog-head">
            <h2>"Add a host"</h2>
            <div class="modes" role="tablist">
                <button type="button" role="tab" class:active=move || way.get() == Way::Ssh
                    title="regency's runners reach the host over SSH: nothing to install on it"
                    on:click=move |_| way.set(Way::Ssh)>"Over SSH"</button>
                <button type="button" role="tab" class:active=move || way.get() == Way::Regops
                    title="The host runs the regops agent, which applies its expected state itself and reports to regency"
                    on:click=move |_| way.set(Way::Regops)>"With regops"</button>
            </div>
        </div>
        <p class="muted small">"Over SSH, regency's runners reach the host (nothing to install on it); with regops, the host runs the agent, which applies its expected state itself and reports to regency. A host is checked once an assignment gives it an expected state (Assignments)."</p>
        <Show when=move || error.get().is_some()><p class="error">{move || error.get()}</p></Show>
        {message_view(message)}

        <Show when=move || way.get() == Way::Ssh>
            <p>"1. Authorize the runners' key on the host, for the account they log in as:"</p>
            {move || {
                let pools = pools.get();
                if pools.values().all(Vec::is_empty) {
                    return view! { <p class="muted">"No runner has published an SSH key yet. Right after a first start, wait half a minute (this dialog follows the runners). Otherwise: runner setting [runner] ssh_key (the all-in-one sets it); runners started before this version publish it after their next restart."</p> }.into_any();
                }
                pools.into_iter().filter(|(_, keys)| !keys.is_empty()).map(|(pool, keys)| {
                    let lines = keys.iter().map(|k| k.public_key.clone()).collect::<Vec<_>>().join("\n");
                    let command = format!(
                        "mkdir -p ~/.ssh && chmod 700 ~/.ssh && cat >> ~/.ssh/authorized_keys <<'EOF'\n{lines}\nEOF\nchmod 600 ~/.ssh/authorized_keys"
                    );
                    let several = keys.len() > 1;
                    view! {
                        <h3>{format!("Runner pool {pool}")}</h3>
                        {several.then(|| view! { <p class="muted small">"Several runners serve this pool: authorize every key."</p> })}
                        {snippet(command)}
                    }
                }).collect_view().into_any()
            }}
            <p>"2. Define the host:"</p>
            <div class="form">
                {ssh_input("Host id", |f| f.host_id.clone(), |f, v| f.host_id = v, "web-01 (unique in regency)")}
                {ssh_input("Address", |f| f.address.clone(), |f, v| f.address = v, "10.0.0.5 or web-01.example.com (port 22 unless host:port)")}
                {ssh_input("User", |f| f.user.clone(), |f, v| f.user = v, "the account whose authorized_keys has the key")}
                <label>"Runner pool"
                    <select on:change=move |ev| { let v = event_target_value(&ev); ssh.update(|f| f.pool = v); }>
                        <For each=pool_names key=|p| p.clone() let:pool>
                            {
                                let (value, text) = (pool.clone(), pool.clone());
                                view! { <option value=value selected=move || ssh.with(|f| f.pool == pool)>{text}</option> }
                            }
                        </For>
                    </select>
                </label>
                {ssh_input("Labels (they decide its assignments)", |f| f.labels.clone(), |f, v| f.labels = v, "env=prod, role=web")}
                {ssh_input("Host key fingerprints (required from security level 2)", |f| f.fingerprints.clone(), |f, v| f.fingerprints = v, "SHA256:… (ssh-keyscan HOST | ssh-keygen -lf -)")}
                <div class="toolbar">
                    <button class="primary" disabled=move || pools.with(|p| p.values().all(Vec::is_empty))
                        on:click=move |_| add_ssh()>"Add the host"</button>
                </div>
            </div>
            {move || created.get().map(|host| view! {
                <p class="notice">"Host " <a href=format!("/hosts/{host}")>{host.clone()}</a>
                    " defined. Give it an expected state with an assignment ("
                    <a href="/assignments">"Assignments"</a>"), then Assess it from its page. It appears in this table after its first job."</p>
            })}
        </Show>

        <Show when=move || way.get() == Way::Regops>
            {move || match endpoint.get() {
                None => view! { <p class="muted">"No endpoint has reported yet: right after a first start, wait half a minute (this dialog follows the services). regops agents report to the endpoint."</p> }.into_any(),
                Some(_) => view! {
                    <p>"Endpoint for agents: " <code>{move || url.get()}</code>
                        {move || setting_url.with(String::is_empty).then(|| view! { " " <span class="muted small">"(this page's address; set another in Settings → Endpoint URL)"</span> })}
                        ". The host must reach it."</p>
                }.into_any(),
            }}
            <p>"1. Install regops on the host (packages: github.com/regent-project/regops/releases)."</p>
            <p>"2. Issue an enrollment token for the host (single host, short-lived; regops exchanges it for its certificate at its first connection):"</p>
            <div class="form">
                <label>"Host id (regops' host_id, by default the machine's hostname)"
                    <input placeholder="web-01" prop:value=move || agent_id.get() on:input=move |ev| agent_id.set(event_target_value(&ev)) />
                </label>
                <label>"Valid for (hours, at most 168)"
                    <input type="number" min="1" max="168" prop:value=move || ttl_hours.get() on:input=move |ev| ttl_hours.set(event_target_value(&ev)) />
                </label>
                <div class="toolbar">
                    <button class="primary" disabled=move || endpoint.with(Option::is_none) on:click=move |_| issue()>"Issue a token"</button>
                </div>
            </div>
            {move || token.get().map(|(host_id, value, expires)| {
                let commands = match ca_cert() {
                    Some(ca) => regops_commands(&ca, &value),
                    None => format!("# the endpoint has no built-in CA: put the CA of its certificate in {CA_PATH}\necho '{value}' | sudo tee {TOKEN_PATH} >/dev/null\nsudo chgrp regops {TOKEN_PATH} && sudo chmod 0640 {TOKEN_PATH}\n"),
                };
                let config = regops_config(&url.get(), server_name.get().as_deref(), &host_id);
                view! {
                    <p>{format!("3. On {host_id}, write the CA and the token (valid until {}):", format_ms(expires))}</p>
                    {snippet(commands)}
                    <p>"4. In /etc/regops/config.toml (its expected state still comes from its own Git repository, [Git]):"</p>
                    {snippet(config)}
                    <p>"5. " <code>"sudo systemctl restart regops"</code> ": the host appears after its first pass, with its outcomes and live logs."</p>
                }
            })}
        </Show>
        <div class="actions">
            <button on:click=move |_| reload()>"Refresh"</button>
            <button on:click=move |_| on_close.run(())>"Close"</button>
        </div>
        </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(service: &str, gone: bool, status: Value) -> InstanceView {
        InstanceView {
            summary: api::Instance {
                service: service.into(),
                gone,
                ..Default::default()
            },
            status,
        }
    }

    #[test]
    fn runner_keys_per_pool_live_and_deduplicated() {
        let runner = |pool: &str, key: &str| {
            json!({ "pool": pool,
                "config": { "runner": { "pool": pool, "ssh_key": "/k" } },
                "advertised": { "ssh_public_key": key } })
        };
        let instances = vec![
            instance("runner", false, runner("default", "ssh-ed25519 A r1")),
            instance("runner", false, runner("default", "ssh-ed25519 A r1")),
            instance("runner", false, runner("default", "ssh-ed25519 B r2")),
            instance("runner", true, runner("default", "ssh-ed25519 C gone")),
            instance("runner", false, runner("dmz", "ssh-ed25519 D r3")),
            instance("runner", false, json!({ "pool": "edge" })),
            instance("endpoint", false, json!({})),
        ];
        let pools = runner_keys(&instances);
        assert_eq!(pools["default"].len(), 2);
        assert_eq!(pools["dmz"][0].path, "/k");
        assert!(pools["edge"].is_empty());
    }

    #[test]
    fn endpoint_url_and_server_name() {
        assert_eq!(
            endpoint_url("", "192.168.1.40", "8443"),
            "https://192.168.1.40:8443"
        );
        assert_eq!(endpoint_url("", "::1", ""), "https://[::1]:8443");
        assert_eq!(
            endpoint_url("https://r.example.com:443/", "x", "8443"),
            "https://r.example.com:443"
        );
        let names: Vec<String> = ["localhost", "regency", "inference", "192.168.1.40"]
            .map(String::from)
            .to_vec();
        assert_eq!(server_name_for("https://192.168.1.40:8443", &names), None);
        assert_eq!(server_name_for("https://INFERENCE:8443", &names), None);
        assert_eq!(
            server_name_for("https://10.0.0.9:8443", &names).as_deref(),
            Some("regency")
        );
        assert_eq!(
            server_name_for("https://[::1]:8443", &names).as_deref(),
            Some("regency")
        );
        assert_eq!(server_name_for("https://x:8443", &[]), None);
    }

    #[test]
    fn regops_snippets() {
        let config = regops_config("https://10.0.0.9:8443", Some("regency"), "web-01");
        assert!(config.contains("Reporting = \"Regency\"\n\n[Regency]\n"));
        assert!(config.contains("server_name = \"regency\""));
        assert!(config.contains("tls_mode = \"BuiltInCa\""));
        assert!(!regops_config("https://a:1", None, "h").contains("server_name"));
        let commands = regops_commands(
            "-----BEGIN CERTIFICATE-----\nX\n-----END CERTIFICATE-----\n",
            "tok",
        );
        assert!(
            commands.contains(
                "<<'EOF'\n-----BEGIN CERTIFICATE-----\nX\n-----END CERTIFICATE-----\nEOF\n"
            )
        );
        assert!(commands.contains("echo 'tok' | sudo tee /etc/regops/regency-enrollment.token"));
        assert!(valid_host_id("web-01.example.com"));
        assert!(!valid_host_id("web_01") && !valid_host_id("-a") && !valid_host_id(""));
    }

    #[test]
    fn ssh_host_definition() {
        let form = SshHostForm {
            host_id: "web-01".into(),
            address: "10.0.0.5".into(),
            user: "ops".into(),
            pool: "dmz".into(),
            labels: "env=prod".into(),
            fingerprints: "SHA256:abc".into(),
        };
        let item = form
            .to_item("ui", "/var/lib/regency/secrets/runner-ssh-key")
            .unwrap();
        assert_eq!(item["builder"]["Endpoint"], "10.0.0.5:22");
        let ssh = &item["builder"]["HostConnectionMethod"]["Ssh2"];
        assert_eq!(
            ssh["AuthMethod"]["Key"]["Key"]["SecRef"],
            "/var/lib/regency/secrets/runner-ssh-key"
        );
        assert_eq!(ssh["HostKey"]["Fingerprints"], json!(["SHA256:abc"]));
        assert_eq!(item["pool"], "dmz");
        let plain = SshHostForm {
            fingerprints: String::new(),
            pool: "default".into(),
            address: "h:2222".into(),
            ..form.clone()
        }
        .to_item("ui", "/k")
        .unwrap();
        assert_eq!(plain["builder"]["Endpoint"], "h:2222");
        assert!(
            plain["builder"]["HostConnectionMethod"]["Ssh2"]
                .get("HostKey")
                .is_none()
        );
        assert!(plain.get("pool").is_none());
        assert_eq!(
            SshHostForm {
                address: "fe80::1".into(),
                ..form.clone()
            }
            .to_item("ui", "/k")
            .unwrap()["builder"]["Endpoint"],
            "[fe80::1]:22"
        );
        assert!(
            SshHostForm {
                fingerprints: "MD5:x".into(),
                ..form.clone()
            }
            .to_item("ui", "/k")
            .is_err()
        );
        assert!(
            SshHostForm {
                user: String::new(),
                ..form
            }
            .to_item("ui", "/k")
            .is_err()
        );
    }
}
