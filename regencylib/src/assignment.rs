//! Effective expected state of a host (D23): the composition of every
//! assignment selecting it, layered like Helm values (generic → specific).
//!
//! - Layers are ordered by priority, lowest first (then by assignment
//!   reference, for determinism).
//! - Attributes are matched by `Name`: a higher-priority attribute replaces
//!   the lower one in its place; unnamed attributes are always added.
//! - The same `Name` from two assignments of equal priority is a conflict
//!   (unless both attributes are identical): no effective state (and no
//!   task) until fixed.
//!
//! Shared by scheduler (tasks), backend (UI) and endpoint (regops).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use regent_sdk::state::ExpectedState;
use regent_sdk::state::attribute::Attribute;

use crate::hashing::stable_hash;
use crate::model::{AssignmentDef, CatalogBody, CatalogItem, ExpectedStateDef};

/// One layer of an effective state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    /// `{source_id}/{name}` of the assignment.
    pub assignment: String,
    pub priority: i32,
    /// `{source_id}/{name}` of its expected state.
    pub expected_state: String,
}

/// Effective expected state of one host.
#[derive(Debug, Clone)]
pub enum Effective {
    /// No assignment selects the host: nothing to check (grey, no task).
    Unassigned,
    /// Assignments select it but cannot be composed: no task.
    Invalid { layers: Vec<Layer>, reason: String },
    Ready {
        layers: Vec<Layer>,
        state: ExpectedState,
        /// Assignment each attribute of `state` comes from.
        origins: Vec<String>,
        /// Changes whenever a layer, its expected state or their order
        /// changes.
        revision: String,
    },
}

impl Effective {
    pub fn layers(&self) -> &[Layer] {
        match self {
            Effective::Unassigned => &[],
            Effective::Invalid { layers, .. } | Effective::Ready { layers, .. } => layers,
        }
    }

    pub fn revision(&self) -> Option<&str> {
        match self {
            Effective::Ready { revision, .. } => Some(revision),
            _ => None,
        }
    }
}

/// Assignments and expected states of the catalog, with their revisions.
#[derive(Debug, Clone, Default)]
pub struct Assignments {
    /// reference → (definition, revision)
    assignments: BTreeMap<String, (AssignmentDef, String)>,
    states: HashMap<String, (ExpectedStateDef, String)>,
}

impl Assignments {
    pub fn from_catalog<'a>(items: impl IntoIterator<Item = &'a CatalogItem>) -> Self {
        let mut this = Self::default();
        for item in items {
            match &item.body {
                CatalogBody::Assignment(def) => {
                    this.assignments
                        .insert(def.reference(), (def.clone(), item.revision.clone()));
                }
                CatalogBody::ExpectedState(def) => {
                    this.states.insert(
                        format!("{}/{}", def.source_id, def.name),
                        (def.clone(), item.revision.clone()),
                    );
                }
                _ => {}
            }
        }
        this
    }

    pub fn get(&self, reference: &str) -> Option<&AssignmentDef> {
        self.assignments.get(reference).map(|(def, _)| def)
    }

    pub fn iter(&self) -> impl Iterator<Item = &AssignmentDef> {
        self.assignments.values().map(|(def, _)| def)
    }

    /// Effective state of a host (`inventories` as `{source_id}/{name}`).
    pub fn effective(
        &self,
        host_id: &str,
        inventories: &BTreeSet<String>,
        labels: &BTreeMap<String, String>,
    ) -> Effective {
        let mut selected: Vec<&(AssignmentDef, String)> = self
            .assignments
            .values()
            .filter(|(def, _)| def.selector.matches(host_id, inventories, labels))
            .collect();
        if selected.is_empty() {
            return Effective::Unassigned;
        }
        selected.sort_by(|(a, _), (b, _)| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.reference().cmp(&b.reference()))
        });
        let layers: Vec<Layer> = selected
            .iter()
            .map(|(def, _)| Layer {
                assignment: def.reference(),
                priority: def.priority,
                expected_state: def.expected_state.clone(),
            })
            .collect();
        let mut parts = Vec::with_capacity(selected.len());
        for (def, revision) in &selected {
            let Some((state, state_revision)) = self.states.get(&def.expected_state) else {
                return Effective::Invalid {
                    layers,
                    reason: format!(
                        "assignment {} refers to unknown expected state {}",
                        def.reference(),
                        def.expected_state
                    ),
                };
            };
            parts.push((
                def,
                revision.as_str(),
                &state.state,
                state_revision.as_str(),
            ));
        }
        match compose(&parts) {
            Ok((state, origins)) => {
                let fingerprint: Vec<String> = parts
                    .iter()
                    .map(|(def, rev, _, state_rev)| {
                        format!(
                            "{}@{rev}:{}:{}@{state_rev}",
                            def.reference(),
                            def.priority,
                            def.expected_state
                        )
                    })
                    .collect();
                Effective::Ready {
                    layers,
                    state,
                    origins,
                    revision: format!("{:016x}", stable_hash(&fingerprint.join("\n"))),
                }
            }
            Err(reason) => Effective::Invalid { layers, reason },
        }
    }
}

/// Compose layers already sorted lowest priority first.
fn compose(
    parts: &[(&AssignmentDef, &str, &ExpectedState, &str)],
) -> Result<(ExpectedState, Vec<String>), String> {
    // (attribute, priority, assignment)
    let mut result: Vec<(Attribute, i32, String)> = Vec::new();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for (def, _, state, _) in parts {
        let owner = def.reference();
        for attribute in &state.attributes {
            let named = attribute.name.as_ref().filter(|n| !n.is_empty());
            match named.and_then(|n| by_name.get(n).copied()) {
                Some(position) => {
                    let (_, priority, previous) = &result[position];
                    let same = serde_json::to_value(&result[position].0).ok()
                        == serde_json::to_value(attribute).ok();
                    if *priority == def.priority && *previous != owner && !same {
                        return Err(format!(
                            "attribute {} is set by {previous} and {owner}, both at priority {priority}",
                            named.map(String::as_str).unwrap_or_default()
                        ));
                    }
                    result[position] = (attribute.clone(), def.priority, owner.clone());
                }
                None => {
                    if let Some(name) = named {
                        by_name.insert(name.clone(), result.len());
                    }
                    result.push((attribute.clone(), def.priority, owner.clone()));
                }
            }
        }
    }
    let mut state = ExpectedState::new();
    let mut origins = Vec::with_capacity(result.len());
    state.attributes = result
        .into_iter()
        .map(|(attribute, _, owner)| {
            origins.push(owner);
            attribute
        })
        .collect();
    Ok((state, origins))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::HostSelector;

    fn state(yaml: &str) -> ExpectedState {
        yaml_serde::from_str(yaml).unwrap()
    }

    fn line(name: Option<&str>, text: &str) -> String {
        let name = name.map(|n| format!("    Name: {n}\n")).unwrap_or_default();
        format!(
            "  - Privilege: !None\n{name}    Detail: !LineInFile\n      FilePath: /etc/motd\n      State: !Present {{}}\n      Line: !Raw {text}\n"
        )
    }

    fn items(entries: Vec<(&str, i32, &str, ExpectedState, HostSelector)>) -> Vec<CatalogItem> {
        let mut out = Vec::new();
        for (name, priority, state_name, expected, selector) in entries {
            out.push(CatalogItem {
                body: CatalogBody::Assignment(AssignmentDef {
                    source_id: "s".into(),
                    name: name.into(),
                    selector,
                    expected_state: format!("s/{state_name}"),
                    priority,
                }),
                revision: "1".into(),
            });
            out.push(CatalogItem {
                body: CatalogBody::ExpectedState(ExpectedStateDef {
                    source_id: "s".into(),
                    name: state_name.into(),
                    state: expected,
                }),
                revision: "1".into(),
            });
        }
        out
    }

    /// The marker word of each attribute, in order.
    fn texts(state: &ExpectedState) -> Vec<&'static str> {
        state
            .attributes
            .iter()
            .map(|a| {
                let json = serde_json::to_string(a).unwrap();
                ["generic", "always", "pool", "extra", "web"]
                    .into_iter()
                    .find(|w| json.contains(&format!("\"{w}\"")))
                    .unwrap_or("?")
            })
            .collect()
    }

    #[test]
    fn layers_override_by_name() {
        let all = HostSelector::parse_terms(["inventory:s/all"]).unwrap();
        let web = HostSelector::parse_terms(["label:role=web"]).unwrap();
        let base = state(&format!(
            "Attributes:\n{}{}{}",
            line(Some("motd"), "generic"),
            line(None, "always"),
            line(Some("ntp"), "pool")
        ));
        let specific = state(&format!(
            "Attributes:\n{}{}",
            line(Some("motd"), "web"),
            line(None, "extra")
        ));
        let catalog = items(vec![
            ("web", 100, "web", specific, web),
            ("baseline", 0, "base", base, all),
        ]);
        let assignments = Assignments::from_catalog(&catalog);
        let inventories = BTreeSet::from(["s/all".to_string()]);
        let labels = BTreeMap::from([("role".to_string(), "web".to_string())]);

        let Effective::Ready {
            layers,
            state,
            origins,
            revision,
        } = assignments.effective("h", &inventories, &labels)
        else {
            panic!("not ready");
        };
        assert_eq!(layers[0].assignment, "s/baseline");
        assert_eq!(texts(&state), ["web", "always", "pool", "extra"]);
        assert_eq!(origins, ["s/web", "s/baseline", "s/baseline", "s/web"]);

        // a host without the label only gets the baseline, another revision
        let base_only = assignments.effective("h2", &inventories, &BTreeMap::new());
        assert_eq!(base_only.layers().len(), 1);
        assert_ne!(base_only.revision(), Some(revision.as_str()));

        assert!(matches!(
            assignments.effective("x", &BTreeSet::new(), &BTreeMap::new()),
            Effective::Unassigned
        ));
    }

    #[test]
    fn same_priority_same_name_conflicts() {
        let all = HostSelector::hosts(["h"]);
        let a = state(&format!("Attributes:\n{}", line(Some("motd"), "a")));
        let b = state(&format!("Attributes:\n{}", line(Some("motd"), "b")));
        let catalog = items(vec![("a", 10, "a", a, all.clone()), ("b", 10, "b", b, all)]);
        let assignments = Assignments::from_catalog(&catalog);
        match assignments.effective("h", &BTreeSet::new(), &BTreeMap::new()) {
            Effective::Invalid { reason, layers } => {
                assert_eq!(layers.len(), 2);
                assert!(reason.contains("motd"), "{reason}");
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
    }
}
