//! Repository layout of a Git source (D11), relative to the source's `path`:
//!
//! - `inventories/<name>.yaml`: regent-sdk inventory YAML (`Name`, `Hosts`,
//!   `DefaultConnectionMethod`, `GlobalVars`), plus regency keys: `Pool` at
//!   the top (the inventory's runner pool, D16), and `Labels` / `Pool` on each
//!   host;
//! - `expected_states/<name>.yaml`: a regent-sdk `ExpectedState`;
//! - `assignments/<name>.yaml` (D23): `ExpectedState` (a name of this source
//!   or `<source>/<name>`), `Priority` (integer, default 0) and the hosts:
//!   any of `Hosts` (ids), `Inventories` (names of this source or
//!   `<source>/<name>`), `Labels` (all must match).
//!
//! Items are rejected when they carry inline credentials (D9).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use regencylib::model::{
    AssignmentDef, CatalogBody, CatalogItem, ExpectedStateDef, HostDef, InventoryDef,
};
use regencylib::selector::HostSelector;
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

/// `assignments/<name>.yaml` (D23).
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct AssignmentFile {
    expected_state: String,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    hosts: BTreeSet<String>,
    #[serde(default)]
    inventories: BTreeSet<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
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
    let file: AssignmentFile = yaml_serde::from_str(text).map_err(|e| e.to_string())?;
    let def = AssignmentDef {
        source_id: source_id.to_string(),
        name: name.to_string(),
        selector: HostSelector {
            hosts: file.hosts,
            inventories: file
                .inventories
                .iter()
                .map(|i| qualify(source_id, i))
                .collect(),
            labels: file.labels,
        },
        expected_state: qualify(source_id, &file.expected_state),
        priority: file.priority,
    };
    def.validate()?;
    Ok(def)
}

/// Catalog items of a source (without the source doc itself), or the
/// per-file errors. `revision` is set to `sha` (the caller keeps the old
/// revision for unchanged items).
pub fn parse(source_id: &str, sha: &str, files: &[File]) -> (Vec<CatalogItem>, Vec<String>) {
    let mut items = Vec::new();
    let mut errors = Vec::new();
    let mut hosts: BTreeMap<String, HostDef> = BTreeMap::new();
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
        } else if let Some(file_stem) = stem(&file.path, "inventories") {
            match parse_inventory(source_id, file_stem, text) {
                Ok((inventory, defs)) => {
                    let name = inventory.name.clone();
                    items.push(item(CatalogBody::Inventory(inventory)));
                    for def in defs {
                        match hosts.get_mut(&def.host_id) {
                            Some(existing) => {
                                if serde_json::to_value(&existing.builder).ok()
                                    != serde_json::to_value(&def.builder).ok()
                                {
                                    errors.push(format!(
                                        "{}: host {} is defined differently in another inventory of this source",
                                        file.path, def.host_id
                                    ));
                                    continue;
                                }
                                existing.inventories.insert(name.clone());
                                existing.labels.extend(def.labels);
                                if existing.pool.is_none() {
                                    existing.pool = def.pool;
                                }
                            }
                            None => {
                                hosts.insert(def.host_id.clone(), def);
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
            .map(|def| item(CatalogBody::Hostdef(def))),
    );
    items.retain(|i| match reject_inline_secrets(i) {
        Ok(()) => true,
        Err(reason) => {
            errors.push(format!("{}: {reason}", i.doc_id()));
            false
        }
    });
    (items, errors)
}

fn parse_inventory(
    source_id: &str,
    file_stem: &str,
    text: &str,
) -> Result<(InventoryDef, Vec<HostDef>), String> {
    let mut root: Value = yaml_serde::from_str(text).map_err(|e| e.to_string())?;
    let mapping = root.as_mapping_mut().ok_or("an inventory is a mapping")?;
    let name: String = take(mapping, "Name")?.unwrap_or_else(|| file_stem.to_string());
    let pool: Option<String> = take(mapping, "Pool")?;
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
        let labels: BTreeMap<String, String> = take(host, "Labels")?.unwrap_or_default();
        let host_pool: Option<String> = take(host, "Pool")?;
        let builder: ManagedHostBuilder =
            yaml_serde::from_value(raw).map_err(|e| format!("host: {e}"))?;
        defs.push(HostDef {
            source_id: source_id.to_string(),
            host_id: builder.id.clone(),
            builder,
            inventories: BTreeSet::from([name.clone()]),
            labels,
            pool: host_pool,
        });
    }
    Ok((
        InventoryDef {
            source_id: source_id.to_string(),
            name,
            default_connection_method,
            global_vars,
            pool,
        },
        defs,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, content: &str) -> File {
        File {
            path: path.to_string(),
            content: content.as_bytes().to_vec(),
        }
    }

    #[test]
    fn parses_inventories_and_states() {
        let files = [
            file(
                "inventories/web.yaml",
                r#"
Pool: dmz
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
"#,
            ),
            file(
                "inventories/all.yaml",
                "Hosts:\n  - Id: web-01\n    Endpoint: 10.0.0.1:22\n    Labels: { tier: front }\n",
            ),
            file("expected_states/base.yaml", "Attributes: []\n"),
            file(
                "assignments/front.yaml",
                "ExpectedState: base\nPriority: 100\nInventories: [web, other/x]\nLabels: { env: prod }\n",
            ),
            file("README.md", "ignored"),
            file("inventories/nested/x.yaml", "ignored"),
        ];
        let (items, errors) = parse("git1", "abc", &files);
        assert!(errors.is_empty(), "{errors:?}");
        let ids: BTreeSet<String> = items.iter().map(CatalogItem::doc_id).collect();
        assert_eq!(
            ids,
            BTreeSet::from(
                [
                    "inventory:git1:web",
                    "inventory:git1:all",
                    "hostdef:git1:web-01",
                    "hostdef:git1:web-02",
                    "expected_state:git1:base",
                    "assignment:git1:front"
                ]
                .map(String::from)
            )
        );
        let web01 = items
            .iter()
            .find_map(|i| match &i.body {
                CatalogBody::Hostdef(d) if d.host_id == "web-01" => Some(d),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            web01.inventories,
            BTreeSet::from(["web".to_string(), "all".to_string()])
        );
        assert_eq!(web01.labels.get("env").map(String::as_str), Some("prod"));
        assert_eq!(web01.labels.get("tier").map(String::as_str), Some("front"));
        let web = items
            .iter()
            .find_map(|i| match &i.body {
                CatalogBody::Inventory(d) if d.name == "web" => Some(d),
                _ => None,
            })
            .unwrap();
        assert_eq!(web.pool.as_deref(), Some("dmz"));
        assert!(web.default_connection_method.is_some());
        let front = items
            .iter()
            .find_map(|i| match &i.body {
                CatalogBody::Assignment(d) => Some(d),
                _ => None,
            })
            .unwrap();
        assert_eq!(front.expected_state, "git1/base");
        assert_eq!(front.priority, 100);
        assert_eq!(
            front.selector.inventories,
            BTreeSet::from(["git1/web".to_string(), "other/x".to_string()])
        );
    }

    #[test]
    fn refuses_inline_credentials_and_bad_files() {
        let files = [
            file(
                "inventories/raw.yaml",
                "Hosts:\n  - Id: h\n    Endpoint: localhost\n    HostConnectionMethod: !Localhost\n      UserKind: !RawUser\n        Username: u\n        Password: p\n",
            ),
            file("expected_states/broken.yaml", "Attributes: 3\n"),
            file("inventories/typo.yaml", "Hostz: []\n"),
        ];
        let (items, errors) = parse("git1", "abc", &files);
        assert_eq!(
            items.len(),
            1,
            "only the inventory without hosts remains: {items:?}"
        );
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("D9")));
    }
}
