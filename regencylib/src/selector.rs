//! Host selector (D16), shared by recurring orders and `run` commands, and its
//! resolution against the catalog (D11) at fire time.

use std::collections::{BTreeMap, BTreeSet};

use regent_sdk::hosts::managed_host::ManagedHostBuilder;
use serde::{Deserialize, Serialize};

use crate::model::{HostDef, InventoryDef};

/// Union of three criteria: explicit host ids, whole inventories
/// (`{source_id}/{name}`), and hosts carrying **all** the given labels.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSelector {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub hosts: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub inventories: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

impl HostSelector {
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty() && self.inventories.is_empty() && self.labels.is_empty()
    }

    pub fn hosts(ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            hosts: ids.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    /// Parse CLI-style terms: `host:<id>`, `inventory:<source>/<name>`,
    /// `label:<key>=<value>`; a bare term is a host id.
    /// Whether a host with these facts is selected (`inventories` as
    /// `{source_id}/{name}`).
    pub fn matches(
        &self,
        host_id: &str,
        inventories: &BTreeSet<String>,
        labels: &BTreeMap<String, String>,
    ) -> bool {
        self.hosts.contains(host_id)
            || !self.inventories.is_disjoint(inventories)
            || (!self.labels.is_empty()
                && self.labels.iter().all(|(k, v)| labels.get(k) == Some(v)))
    }

    pub fn parse_terms<'a>(terms: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        let mut selector = Self::default();
        for term in terms {
            if let Some(inventory) = term.strip_prefix("inventory:") {
                if crate::model::split_ref(inventory).is_none() {
                    return Err(format!("inventory must be <source>/<name>: {term}"));
                }
                selector.inventories.insert(inventory.to_string());
            } else if let Some(label) = term.strip_prefix("label:") {
                let (k, v) = label
                    .split_once('=')
                    .ok_or_else(|| format!("label must be key=value: {term}"))?;
                selector.labels.insert(k.to_string(), v.to_string());
            } else {
                selector
                    .hosts
                    .insert(term.strip_prefix("host:").unwrap_or(term).to_string());
            }
        }
        Ok(selector)
    }
}

/// A host ready to be turned into a task.
#[derive(Debug, Clone)]
pub struct ResolvedHost {
    pub host_id: String,
    /// The `ManagedHostBuilder` with its inventories' defaults applied.
    pub builder: ManagedHostBuilder,
    pub pool: String,
    pub labels: BTreeMap<String, String>,
    /// Inventories of every source, as `{source_id}/{name}`.
    pub inventories: BTreeSet<String>,
}

/// Outcome of a resolution.
#[derive(Debug, Default)]
pub struct Resolution {
    pub targets: Vec<ResolvedHost>,
    /// Selected but defined differently by several sources (D11): skipped.
    pub conflicting: Vec<String>,
    /// Selected by id but defined nowhere: skipped.
    pub unknown: Vec<String>,
}

pub const DEFAULT_POOL: &str = "default";

/// Effective definition of one host from one source.
fn effective(
    def: &HostDef,
    inventories: &BTreeMap<(String, String), InventoryDef>,
) -> (ManagedHostBuilder, String) {
    let mut builder = def.builder.clone();
    builder.id = def.host_id.clone();
    let mut pool = def.pool.clone();
    for name in &def.inventories {
        let Some(inventory) = inventories.get(&(def.source_id.clone(), name.clone())) else {
            continue;
        };
        if builder.host_connection_method.is_none()
            && let Some(method) = &inventory.default_connection_method
        {
            builder.set_connection_method(method.clone());
        }
        if let Some(global) = &inventory.global_vars {
            let mut vars = global.clone();
            if let Some(own) = &builder.host_vars {
                vars.extend(own.clone());
            }
            builder.set_host_vars(Some(vars));
        }
        if pool.is_none() {
            pool = inventory.pool.clone();
        }
    }
    (builder, pool.unwrap_or_else(|| DEFAULT_POOL.to_string()))
}

/// Resolve `selector` against every host definition and inventory of the
/// catalog. `pool_override` replaces the pool of every target (D16).
pub fn resolve(
    selector: &HostSelector,
    hostdefs: &[HostDef],
    inventories: &[InventoryDef],
    pool_override: Option<&str>,
) -> Resolution {
    let inventories: BTreeMap<(String, String), InventoryDef> = inventories
        .iter()
        .map(|i| ((i.source_id.clone(), i.name.clone()), i.clone()))
        .collect();
    let mut by_host: BTreeMap<&str, Vec<&HostDef>> = BTreeMap::new();
    for def in hostdefs {
        by_host.entry(def.host_id.as_str()).or_default().push(def);
    }

    let mut resolution = Resolution::default();
    for id in &selector.hosts {
        if !by_host.contains_key(id.as_str()) {
            resolution.unknown.push(id.clone());
        }
    }
    for (host_id, defs) in by_host {
        let labels: BTreeMap<String, String> = defs.iter().flat_map(|d| d.labels.clone()).collect();
        let host_inventories = inventories_of(&defs);
        let selected = selector.matches(host_id, &host_inventories, &labels);
        if !selected {
            continue;
        }
        // identical effective definitions merge; different ones conflict
        let mut variants: Vec<(ManagedHostBuilder, String)> =
            defs.iter().map(|d| effective(d, &inventories)).collect();
        let first = serde_json::to_value(&variants[0]).ok();
        if variants
            .iter()
            .skip(1)
            .any(|v| serde_json::to_value(v).ok() != first)
        {
            resolution.conflicting.push(host_id.to_string());
            continue;
        }
        let (builder, pool) = variants.swap_remove(0);
        resolution.targets.push(ResolvedHost {
            host_id: host_id.to_string(),
            builder,
            pool: pool_override.map(str::to_string).unwrap_or(pool),
            labels,
            inventories: host_inventories,
        });
    }
    resolution
}

/// Inventories (`{source_id}/{name}`) of a host's definitions.
pub fn inventories_of(defs: &[&HostDef]) -> BTreeSet<String> {
    defs.iter()
        .flat_map(|d| d.inventories.iter().map(|i| format!("{}/{i}", d.source_id)))
        .collect()
}

/// Host ids with conflicting definitions among `hostdefs` (for the UI, D11).
pub fn conflicting_hosts(hostdefs: &[HostDef], inventories: &[InventoryDef]) -> Vec<String> {
    let all = HostSelector {
        labels: BTreeMap::new(),
        hosts: hostdefs.iter().map(|d| d.host_id.clone()).collect(),
        inventories: BTreeSet::new(),
    };
    resolve(&all, hostdefs, inventories, None).conflicting
}

#[cfg(test)]
mod tests {
    use super::*;
    use regent_sdk::hosts::handlers::{ConnectionMethod, TargetUser};

    fn def(
        source: &str,
        host: &str,
        inventories: &[&str],
        labels: &[(&str, &str)],
        endpoint: &str,
    ) -> HostDef {
        HostDef {
            source_id: source.into(),
            host_id: host.into(),
            builder: ManagedHostBuilder::new(host, endpoint, None),
            inventories: inventories.iter().map(|s| s.to_string()).collect(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            pool: None,
        }
    }

    #[test]
    fn union_of_criteria_with_inventory_defaults() {
        let inventory = InventoryDef {
            source_id: "git".into(),
            name: "web".into(),
            default_connection_method: Some(
                ConnectionMethod::Localhost(TargetUser::current_user()),
            ),
            global_vars: None,
            pool: Some("dmz".into()),
        };
        let defs = vec![
            def("git", "a", &["web"], &[], "a:22"),
            def("git", "b", &[], &[("env", "prod"), ("tier", "db")], "b:22"),
            def("git", "c", &[], &[("env", "prod")], "c:22"),
        ];
        let selector = HostSelector::parse_terms([
            "inventory:git/web",
            "label:env=prod",
            "label:tier=db",
            "zz",
        ])
        .unwrap();
        let resolution = resolve(&selector, &defs, &[inventory], None);
        let ids: Vec<_> = resolution
            .targets
            .iter()
            .map(|t| t.host_id.as_str())
            .collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(resolution.targets[0].pool, "dmz");
        assert!(
            resolution.targets[0]
                .builder
                .host_connection_method
                .is_some()
        );
        assert_eq!(resolution.targets[1].pool, DEFAULT_POOL);
        assert_eq!(resolution.unknown, ["zz"]);
    }

    #[test]
    fn same_host_from_two_sources() {
        let same = vec![
            def("s1", "a", &[], &[], "a:22"),
            def("s2", "a", &[], &[], "a:22"),
        ];
        let resolution = resolve(&HostSelector::hosts(["a"]), &same, &[], Some("p"));
        assert_eq!(resolution.targets.len(), 1);
        assert_eq!(resolution.targets[0].pool, "p");
        let different = vec![
            def("s1", "a", &[], &[], "a:22"),
            def("s2", "a", &[], &[], "other:22"),
        ];
        let resolution = resolve(&HostSelector::hosts(["a"]), &different, &[], None);
        assert_eq!(resolution.conflicting, ["a"]);
        assert!(resolution.targets.is_empty());
    }
}
