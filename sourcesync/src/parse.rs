//! Repository layout of a Git source (D11, D26), relative to the source's
//! `path`:
//!
//! - `hosts/<file>.yaml`: hosts, in regent-sdk inventory YAML (`Hosts`,
//!   `DefaultConnectionMethod`, `GlobalVars`), plus regency keys `Labels`
//!   and `Pool` at the top and on each host. Top-level values are copied
//!   into each host of the file (conveniences of the file: regency has no
//!   inventory defaults, D26); a host's own values win.
//! - `expected_states/<name>.yaml`: a regent-sdk `ExpectedState`;
//! - `assignments/<name>.yaml` (D23, D27, D29): `Selector` (a label
//!   selector, `{env="prod", role=~"web|api"}`; `{}` = every host),
//!   `ExpectedStates` (names of this source or `<source>/<name>`, base
//!   first, last wins; `ExpectedState` for one) and `Priority` (integer,
//!   default 0: order among a host's assignments, lower first).
//! - `inventories/` is gone (D29): its files are errors.
//!
//! Items are rejected when they carry inline credentials (D9) or break the
//! catalog's checks (names, label keys).

use std::collections::{BTreeMap, HashMap};

use regencylib::labels::LabelSelector;
use regencylib::model::{AssignmentDef, CatalogBody, CatalogItem, ExpectedStateDef, HostDef};
use regencylib::validate::reject_inline_secrets;
use regent_sdk::hosts::handlers::ConnectionMethod;
use regent_sdk::hosts::managed_host::ManagedHostBuilder;
use regent_sdk::state::ExpectedState;
use yaml_serde::Value;

/// A file of the tree: path relative to the source's `path`, content.
pub struct File {
    pub path: String,
    pub content: Vec<u8>,
}

fn take<T: serde::de::DeserializeOwned>(
    mapping: &mut yaml_serde::Mapping,
    key: &str,
) -> Result<Option<T>, String> {
    match mapping.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => yaml_serde::from_value(value)
            .map(Some)
            .map_err(|e| format!("{key}: {e}")),
    }
}

fn stem<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(dir)?.strip_prefix('/')?;
    if rest.contains('/') {
        return None;
    }
    rest.strip_suffix(".yaml")
        .or_else(|| rest.strip_suffix(".yml"))
}

/// `assignments/<name>.yaml` (D23, D27, D29): a selector, `ExpectedStates`
/// base first (last wins) or `ExpectedState` for one, a priority.
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct AssignmentFile {
    selector: String,
    #[serde(default)]
    expected_states: Vec<String>,
    #[serde(default)]
    expected_state: Option<String>,
    #[serde(default)]
    priority: i32,
}

/// A name of this source, or already `<source>/<name>`.
fn qualify(source_id: &str, name: &str) -> String {
    if name.contains('/') {
        name.to_string()
    } else {
        format!("{source_id}/{name}")
    }
}

fn parse_assignment(source_id: &str, name: &str, text: &str) -> Result<AssignmentDef, String> {
    let file: AssignmentFile = yaml_serde::from_str(text).map_err(|e| {
        let e = e.to_string();
        if ["`Hosts`", "`Inventories`", "`Inventory`", "`Labels`", "`Selector`"]
            .iter()
            .any(|k| e.contains(k))
        {
            format!("{e} (since D29 an assignment selects its hosts by their labels: `Selector: '{{role=\"web\"}}'`, `{{}}` = every host)")
        } else {
            e
        }
    })?;
    if file.expected_state.is_some() && !file.expected_states.is_empty() {
        return Err("`ExpectedState` and `ExpectedStates` together: keep one".to_string());
    }
    let def = AssignmentDef {
        source_id: source_id.to_string(),
        name: name.to_string(),
        selector: LabelSelector::parse(&file.selector).map_err(|e| format!("Selector: {e}"))?,
        priority: file.priority,
        expected_states: file
            .expected_states
            .iter()
            .chain(&file.expected_state)
            .map(|state| qualify(source_id, state))
            .collect(),
    };
    def.validate()?;
    Ok(def)
}

/// `hosts/<file>.yaml`: the file's hosts, file-level values copied in.
fn parse_hosts(source_id: &str, text: &str) -> Result<Vec<HostDef>, String> {
    let mut root: Value = yaml_serde::from_str(text).map_err(|e| e.to_string())?;
    let mapping = root.as_mapping_mut().ok_or("a hosts file is a mapping")?;
    let _name: Option<String> = take(mapping, "Name")?;
    let pool: Option<String> = take(mapping, "Pool")?;
    let labels: BTreeMap<String, String> = take(mapping, "Labels")?.unwrap_or_default();
    let default_connection_method: Option<ConnectionMethod> =
        take(mapping, "DefaultConnectionMethod")?;
    let global_vars: Option<HashMap<String, String>> = take(mapping, "GlobalVars")?;
    let raw_hosts: Vec<Value> = take(mapping, "Hosts")?.unwrap_or_default();
    if let Some((key, _)) = mapping.iter().next() {
        return Err(format!("unknown key {key:?}"));
    }
    let mut defs = Vec::new();
    for mut raw in raw_hosts {
        let host = raw.as_mapping_mut().ok_or("each host is a mapping")?;
        let own_labels: BTreeMap<String, String> = take(host, "Labels")?.unwrap_or_default();
        let host_pool: Option<String> = take(host, "Pool")?;
        let mut builder: ManagedHostBuilder =
            yaml_serde::from_value(raw).map_err(|e| format!("host: {e}"))?;
        if builder.host_connection_method.is_none()
            && let Some(method) = &default_connection_method
        {
            builder.set_connection_method(method.clone());
        }
        if let Some(global) = &global_vars {
            let mut vars = global.clone();
            if let Some(own) = &builder.host_vars {
                vars.extend(own.clone());
            }
            builder.set_host_vars(Some(vars));
        }
        let mut host_labels = labels.clone();
        host_labels.extend(own_labels);
        defs.push(HostDef {
            source_id: source_id.to_string(),
            host_id: builder.id.clone(),
            builder,
            labels: host_labels,
            pool: host_pool.or_else(|| pool.clone()),
        });
    }
    Ok(defs)
}

/// Catalog items of a source (without the source doc itself), or the
/// per-file errors. `revision` is set to `sha` (the caller keeps the old
/// revision for unchanged items).
pub fn parse(source_id: &str, sha: &str, files: &[File]) -> (Vec<CatalogItem>, Vec<String>) {
    let mut items = Vec::new();
    let mut errors = Vec::new();
    let mut hosts: BTreeMap<String, (HostDef, String)> = BTreeMap::new();
    let item = |body| CatalogItem {
        body,
        revision: sha.to_string(),
    };
    for file in files {
        let text = match std::str::from_utf8(&file.content) {
            Ok(text) => text,
            Err(_) => {
                errors.push(format!("{}: not UTF-8", file.path));
                continue;
            }
        };
        if let Some(name) = stem(&file.path, "expected_states") {
            match yaml_serde::from_str::<ExpectedState>(text) {
                Ok(state) => items.push(item(CatalogBody::ExpectedState(ExpectedStateDef {
                    source_id: source_id.to_string(),
                    name: name.to_string(),
                    state,
                }))),
                Err(e) => errors.push(format!("{}: {e}", file.path)),
            }
        } else if let Some(name) = stem(&file.path, "assignments") {
            match parse_assignment(source_id, name, text) {
                Ok(def) => items.push(item(CatalogBody::Assignment(def))),
                Err(e) => errors.push(format!("{}: {e}", file.path)),
            }
        } else if stem(&file.path, "inventories").is_some() {
            errors.push(format!(
                "{}: inventories are gone (D29): give each assignment its `Selector` and delete inventories/",
                file.path
            ));
        } else if stem(&file.path, "hosts").is_some() {
            match parse_hosts(source_id, text) {
                Ok(defs) => {
                    for def in defs {
                        match hosts.get(&def.host_id) {
                            Some((existing, first)) => {
                                if serde_json::to_value(existing).ok()
                                    != serde_json::to_value(&def).ok()
                                {
                                    errors.push(format!(
                                        "{}: host {} is also defined, differently, in {first}",
                                        file.path, def.host_id
                                    ));
                                }
                            }
                            None => {
                                hosts.insert(def.host_id.clone(), (def, file.path.clone()));
                            }
                        }
                    }
                }
                Err(e) => errors.push(format!("{}: {e}", file.path)),
            }
        }
    }
    items.extend(
        hosts
            .into_values()
            .map(|(def, _)| item(CatalogBody::Hostdef(def))),
    );
    items.retain(
        |i| match reject_inline_secrets(i).and_then(|()| i.validate()) {
            Ok(()) => true,
            Err(reason) => {
                errors.push(format!("{}: {reason}", i.doc_id()));
                false
            }
        },
    );
    (items, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn file(path: &str, content: &str) -> File {
        File {
            path: path.to_string(),
            content: content.as_bytes().to_vec(),
        }
    }

    #[test]
    fn parses_hosts_inventories_and_states() {
        let files = [
            file(
                "hosts/web.yaml",
                r#"
Pool: dmz
Labels: { role: web }
DefaultConnectionMethod: !Ssh2
  AuthMethod: !UsernamePassword
    SecRef: web_creds
    Provider: files
Hosts:
  - Id: web-01
    Endpoint: 10.0.0.1:22
    Labels: { env: prod }
  - Id: web-02
    Endpoint: 10.0.0.2:22
    Pool: edge
"#,
            ),
            file("expected_states/base.yaml", "Attributes: []\n"),
            file(
                "assignments/front.yaml",
                "Selector: '{role=\"web\"}'\nPriority: 10\nExpectedStates: [base, other/web]\n",
            ),
            file("README.md", "ignored"),
            file("hosts/nested/x.yaml", "ignored"),
        ];
        let (items, errors) = parse("git1", "abc", &files);
        assert!(errors.is_empty(), "{errors:?}");
        let ids: BTreeSet<String> = items.iter().map(CatalogItem::doc_id).collect();
        assert_eq!(
            ids,
            BTreeSet::from(
                [
                    "hostdef:git1:web-01",
                    "hostdef:git1:web-02",
                    "expected_state:git1:base",
                    "assignment:git1:front"
                ]
                .map(String::from)
            )
        );
        let host = |id: &str| {
            items
                .iter()
                .find_map(|i| match &i.body {
                    CatalogBody::Hostdef(d) if d.host_id == id => Some(d.clone()),
                    _ => None,
                })
                .unwrap()
        };
        let web01 = host("web-01");
        assert_eq!(web01.labels.get("role").map(String::as_str), Some("web"));
        assert_eq!(web01.labels.get("env").map(String::as_str), Some("prod"));
        assert_eq!(web01.pool.as_deref(), Some("dmz"));
        assert!(
            web01.builder.host_connection_method.is_some(),
            "file default copied"
        );
        assert_eq!(host("web-02").pool.as_deref(), Some("edge"));
        let front = items
            .iter()
            .find_map(|i| match &i.body {
                CatalogBody::Assignment(d) => Some(d),
                _ => None,
            })
            .unwrap();
        assert_eq!(front.expected_states, ["git1/base", "other/web"]);
        assert_eq!(front.selector.to_string(), r#"{role="web"}"#);
        assert_eq!(front.priority, 10);
    }

    #[test]
    fn refuses_inline_credentials_former_layouts_and_bad_files() {
        let files = [
            file(
                "hosts/raw.yaml",
                "Hosts:\n  - Id: h\n    Endpoint: localhost\n    HostConnectionMethod: !Localhost\n      UserKind: !RawUser\n        Username: u\n        Password: p\n",
            ),
            file("expected_states/broken.yaml", "Attributes: 3\n"),
            file(
                "inventories/old.yaml",
                "Hosts:\n  - Id: x\n    Endpoint: x:22\n",
            ),
            file(
                "assignments/bad.yaml",
                "Selector: '{env=prod}'\nExpectedState: base\n",
            ),
            file("assignments/old.yaml", "ExpectedState: base\nHosts: [a]\n"),
            file(
                "hosts/reserved.yaml",
                "Hosts:\n  - Id: r\n    Endpoint: r:22\n    Labels: { host: other }\n",
            ),
            file(
                "assignments/inventory.yaml",
                "ExpectedState: base\nInventory: web\n",
            ),
            file(
                "assignments/empty.yaml",
                "ExpectedStates: []\nSelector: '{}'\n",
            ),
        ];
        let (items, errors) = parse("git1", "abc", &files);
        assert!(
            items.is_empty(),
            "{:?}",
            items.iter().map(CatalogItem::doc_id).collect::<Vec<_>>()
        );
        assert_eq!(errors.len(), 8, "{errors:?}");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("D29") && e.contains("inventories are gone"))
        );
        assert!(errors.iter().any(|e| e.contains("Selector: ")));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("at least one expected state"))
        );
        assert!(errors.iter().any(|e| e.contains("D9")));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("selects its hosts by their labels"))
        );
        assert!(errors.iter().any(|e| e.contains("built in")));
    }
}
