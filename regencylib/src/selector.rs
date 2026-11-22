//! Host selector (D16), shared by recurring orders, `run` commands and live
//! views, and its resolution against the catalog at fire time. A host's
//! assignments are computed from its labels (own + built-in, D26/D29),
//! never stored.

use std::collections::{BTreeMap, BTreeSet};

use regent_sdk::hosts::managed_host::ManagedHostBuilder;
use serde::{Deserialize, Serialize};

use crate::labels::{HostLabels, LabelSelector};
use crate::model::{AssignmentDef, HostDef};

/// Union of four criteria: explicit host ids, the hosts of assignments
/// (`{source_id}/{name}`, i.e. the hosts their selectors match), hosts
/// carrying **all** the given labels, and hosts matching a label selector
/// (built-in labels included).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSelector {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub hosts: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub assignments: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<LabelSelector>,
}

impl HostSelector {
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
            && self.assignments.is_empty()
            && self.labels.is_empty()
            && self.selector.is_none()
    }

    pub fn hosts(ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            hosts: ids.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    /// The hosts of one assignment (`{source_id}/{name}`).
    pub fn assignment(reference: impl Into<String>) -> Self {
        Self {
            assignments: BTreeSet::from([reference.into()]),
            ..Self::default()
        }
    }

    /// Whether a host is selected, given the assignments selecting it
    /// (`{source_id}/{name}`, see [`assignments_of`]) and its labels.
    pub fn matches(
        &self,
        host_id: &str,
        assignments: &BTreeSet<String>,
        labels: &HostLabels,
    ) -> bool {
        self.hosts.contains(host_id)
            || !self.assignments.is_disjoint(assignments)
            || (!self.labels.is_empty()
                && LabelSelector::equal(&self.labels).is_ok_and(|s| s.matches(labels)))
            || self.selector.as_ref().is_some_and(|s| s.matches(labels))
    }

    /// Parse CLI-style terms: `host:<id>`, `assignment:<source>/<name>`,
    /// `label:<key>=<value>`, `selector:{...}`; a bare term is a host id.
    pub fn parse_terms<'a>(terms: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        let mut selector = Self::default();
        for term in terms {
            if let Some(assignment) = term.strip_prefix("assignment:") {
                if crate::model::split_ref(assignment).is_none() {
                    return Err(format!("assignment must be <source>/<name>: {term}"));
                }
                selector.assignments.insert(assignment.to_string());
            } else if let Some(text) = term.strip_prefix("selector:") {
                selector.selector = Some(LabelSelector::parse(text)?);
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

/// A host's labels (D26): the merged own labels of its definitions plus the
/// built-in `host` and `source`.
pub fn host_labels(host_id: &str, defs: &[&HostDef]) -> HostLabels {
    let labels: BTreeMap<String, String> = defs.iter().flat_map(|d| d.labels.clone()).collect();
    HostLabels::new(host_id, defs.iter().map(|d| d.source_id.as_str()), &labels)
}

/// The assignments (`{source_id}/{name}`) whose selector matches `labels`.
pub fn assignments_of<'a>(
    labels: &HostLabels,
    assignments: impl IntoIterator<Item = &'a AssignmentDef>,
) -> BTreeSet<String> {
    assignments
        .into_iter()
        .filter(|a| a.selector.matches(labels))
        .map(AssignmentDef::reference)
        .collect()
}

/// A host ready to be turned into a task.
#[derive(Debug, Clone)]
pub struct ResolvedHost {
    pub host_id: String,
    pub builder: ManagedHostBuilder,
    pub pool: String,
    /// Own and built-in labels.
    pub labels: HostLabels,
    /// The assignments selecting it (`{source_id}/{name}`).
    pub assignments: BTreeSet<String>,
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

/// What a definition gives a task: its builder (id set) and pool (D26: no
/// inventory defaults, the host carries everything).
fn effective(def: &HostDef) -> (ManagedHostBuilder, String) {
    let mut builder = def.builder.clone();
    builder.id = def.host_id.clone();
    let pool = def.pool.clone().unwrap_or_else(|| DEFAULT_POOL.to_string());
    (builder, pool)
}

/// Resolve `selector` against every host definition and assignment of the
/// catalog. `pool_override` replaces the pool of every target (D16).
pub fn resolve(
    selector: &HostSelector,
    hostdefs: &[HostDef],
    assignments: &[AssignmentDef],
    pool_override: Option<&str>,
) -> Resolution {
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
        let labels = host_labels(host_id, &defs);
        let host_assignments = assignments_of(&labels, assignments);
        if !selector.matches(host_id, &host_assignments, &labels) {
            continue;
        }
        // identical effective definitions merge; different ones conflict
        let mut variants: Vec<(ManagedHostBuilder, String)> =
            defs.iter().map(|d| effective(d)).collect();
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
            assignments: host_assignments,
        });
    }
    resolution
}

/// Host ids with conflicting definitions among `hostdefs` (for the UI, D11).
pub fn conflicting_hosts(hostdefs: &[HostDef]) -> Vec<String> {
    let all = HostSelector {
        hosts: hostdefs.iter().map(|d| d.host_id.clone()).collect(),
        ..HostSelector::default()
    };
    resolve(&all, hostdefs, &[], None).conflicting
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(source: &str, host: &str, labels: &[(&str, &str)], endpoint: &str) -> HostDef {
        HostDef {
            source_id: source.into(),
            host_id: host.into(),
            builder: ManagedHostBuilder::new(host, endpoint, None),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            pool: None,
        }
    }

    fn assignment(source: &str, name: &str, selector: &str) -> AssignmentDef {
        AssignmentDef {
            source_id: source.into(),
            name: name.into(),
            selector: LabelSelector::parse(selector).unwrap(),
            priority: 0,
            expected_states: vec!["s/x".into()],
        }
    }

    #[test]
    fn assignments_are_computed_from_labels() {
        let mut dmz = def("git", "a", &[("role", "web")], "a:22");
        dmz.pool = Some("dmz".into());
        let defs = vec![
            dmz,
            def("git", "b", &[("env", "prod"), ("tier", "db")], "b:22"),
            def("ui", "c", &[("env", "prod")], "c:22"),
        ];
        let assignments = [
            assignment("git", "web", r#"{role=~"web|api"}"#),
            assignment("git", "all", "{}"),
            assignment("ui", "prod_from_ui", r#"{env="prod", source="ui"}"#),
        ];
        let selector = HostSelector::parse_terms([
            "assignment:git/web",
            "label:env=prod",
            "label:tier=db",
            "zz",
        ])
        .unwrap();
        let resolution = resolve(&selector, &defs, &assignments, None);
        let ids: Vec<_> = resolution
            .targets
            .iter()
            .map(|t| t.host_id.as_str())
            .collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(resolution.targets[0].pool, "dmz");
        assert_eq!(resolution.targets[1].pool, DEFAULT_POOL);
        assert_eq!(
            resolution.targets[0].assignments,
            BTreeSet::from(["git/web".to_string(), "git/all".to_string()])
        );
        assert_eq!(resolution.unknown, ["zz"]);
        // a built-in label in an assignment's selector
        let c = resolve(
            &HostSelector::assignment("ui/prod_from_ui"),
            &defs,
            &assignments,
            None,
        );
        assert_eq!(c.targets.len(), 1);
        assert_eq!(c.targets[0].host_id, "c");
        // a label selector
        let by_selector =
            HostSelector::parse_terms([r#"selector:{env="prod", tier!="db"}"#]).unwrap();
        let found = resolve(&by_selector, &defs, &assignments, None);
        assert_eq!(found.targets.len(), 1);
        assert_eq!(found.targets[0].host_id, "c");
    }

    #[test]
    fn same_host_from_two_sources() {
        let same = vec![def("s1", "a", &[], "a:22"), def("s2", "a", &[], "a:22")];
        let resolution = resolve(&HostSelector::hosts(["a"]), &same, &[], Some("p"));
        assert_eq!(resolution.targets.len(), 1);
        assert_eq!(resolution.targets[0].pool, "p");
        let both = [assignment("x", "s2", r#"{source="s2"}"#)];
        let in_s2 = resolve(&HostSelector::assignment("x/s2"), &same, &both, None);
        assert_eq!(in_s2.targets.len(), 1, "source matches any of its sources");
        let different = vec![def("s1", "a", &[], "a:22"), def("s2", "a", &[], "other:22")];
        let resolution = resolve(&HostSelector::hosts(["a"]), &different, &[], None);
        assert_eq!(resolution.conflicting, ["a"]);
        assert!(resolution.targets.is_empty());
        assert_eq!(conflicting_hosts(&different), ["a"]);
    }
}
