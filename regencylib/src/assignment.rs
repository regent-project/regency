//! Effective expected state of a host (D23, D27, D29): the final states of
//! every assignment whose selector matches its labels, added up.
//!
//! - An assignment stacks expected states like Helm values files: the
//!   first is the base, each next one overrides the previous ones (last
//!   wins). Attributes are matched by `Name`: a later attribute replaces the
//!   earlier one in its place; unnamed attributes are always added.
//! - Each final attribute keeps its origin (the state it comes from) and the
//!   states it overrides (blame).
//! - Suspected conflicts inside one stack (two attributes acting on the
//!   same thing differently, …) are warnings, never errors.
//! - A host's assignments add up, by priority (lower first) then name, like
//!   Salt's top file: an attribute identical to one of an earlier
//!   assignment (`Name` aside) is applied once; the same `Name` or the same
//!   target with another content is a conflict (no task), and so are two
//!   assignments with the same priority and name (ambiguous order).
//!
//! Shared by scheduler (tasks), backend (UI) and endpoint (regops).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use regent_sdk::state::ExpectedState;
use regent_sdk::state::attribute::Attribute;
use serde_json::Value;

use crate::hashing::stable_hash;
use crate::labels::HostLabels;
use crate::model::{AssignmentDef, CatalogBody, CatalogItem, ExpectedStateDef, ambiguity_message};

/// One layer of an effective state: an expected state of an assignment's
/// stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    /// `{source_id}/{name}` of the assignment.
    pub assignment: String,
    /// `{source_id}/{name}` of the expected state.
    pub expected_state: String,
}

/// Where a final attribute comes from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Blame {
    /// Expected state the attribute comes from.
    pub origin: String,
    /// Earlier states whose attribute of the same `Name` it replaced,
    /// oldest first.
    pub overrides: Vec<String>,
    /// Assignment it comes from (empty for a stack composed alone).
    pub assignment: String,
    /// Later assignments declaring the same attribute (identical content),
    /// applied once (D29).
    pub also_in: Vec<String>,
}

/// A suspected conflict: allowed, shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub message: String,
    /// Positions of the final attributes involved.
    pub attributes: Vec<usize>,
}

/// The final state of a stack of expected states.
#[derive(Debug, Clone)]
pub struct Composition {
    pub state: ExpectedState,
    /// One per attribute of `state`.
    pub blame: Vec<Blame>,
    pub warnings: Vec<Warning>,
}

/// Effective expected state of one host.
#[derive(Debug, Clone)]
pub enum Effective {
    /// No assignment selects the host: nothing to check (grey, no task).
    Unassigned,
    /// No effective state: conflicting or ambiguous assignments, or a state
    /// of a stack is unknown. No task.
    Invalid { layers: Vec<Layer>, reason: String },
    Ready {
        layers: Vec<Layer>,
        state: ExpectedState,
        blame: Vec<Blame>,
        warnings: Vec<Warning>,
        /// Changes whenever an assignment, its stack or one of its states
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

    /// The assignments involved (`{source_id}/{name}`), in order.
    pub fn assignments(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for layer in self.layers() {
            if out.last() != Some(&layer.assignment) {
                out.push(layer.assignment.clone());
            }
        }
        out
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

    /// The assignments selecting a host, in their order (priority, then
    /// name).
    pub fn matching(&self, labels: &HostLabels) -> Vec<&AssignmentDef> {
        let mut selected: Vec<&AssignmentDef> = self
            .iter()
            .filter(|def| def.selector.matches(labels))
            .collect();
        selected.sort_by(|a, b| a.order_key().cmp(&b.order_key()));
        selected
    }

    /// Final state of a stack of expected states (`{source_id}/{name}`,
    /// base first), e.g. an assignment being edited.
    pub fn compose_refs(&self, stack: &[String]) -> Result<Composition, String> {
        let mut parts = Vec::with_capacity(stack.len());
        for reference in stack {
            let (def, _) = self
                .states
                .get(reference)
                .ok_or_else(|| format!("unknown expected state {reference}"))?;
            parts.push((reference.as_str(), &def.state));
        }
        Ok(compose(&parts))
    }

    /// Effective state of a host, from its labels (own and built-in).
    pub fn effective(&self, labels: &HostLabels) -> Effective {
        let selected = self.matching(labels);
        if selected.is_empty() {
            return Effective::Unassigned;
        }
        let layers: Vec<Layer> = selected
            .iter()
            .flat_map(|def| {
                def.expected_states.iter().map(|state| Layer {
                    assignment: def.reference(),
                    expected_state: state.clone(),
                })
            })
            .collect();
        for pair in selected.windows(2) {
            if pair[0].ambiguous_with(pair[1]) {
                return Effective::Invalid {
                    layers,
                    reason: ambiguity_message(pair[0], pair[1]),
                };
            }
        }
        let mut fingerprint = Vec::new();
        let mut compositions = Vec::with_capacity(selected.len());
        for def in &selected {
            let revision = self
                .assignments
                .get(&def.reference())
                .map(|(_, r)| r.as_str())
                .unwrap_or_default();
            fingerprint.push(format!("{}@{revision}", def.reference()));
            for state in &def.expected_states {
                match self.states.get(state) {
                    Some((_, state_revision)) => {
                        fingerprint.push(format!("{state}@{state_revision}"))
                    }
                    None => {
                        return Effective::Invalid {
                            layers,
                            reason: format!(
                                "assignment {} refers to unknown expected state {state}",
                                def.reference()
                            ),
                        };
                    }
                }
            }
            match self.compose_refs(&def.expected_states) {
                Ok(composition) => compositions.push((def.reference(), composition)),
                Err(reason) => return Effective::Invalid { layers, reason },
            }
        }
        match add_up(compositions) {
            Ok(Composition {
                state,
                blame,
                warnings,
            }) => Effective::Ready {
                layers,
                state,
                blame,
                warnings,
                revision: format!("{:016x}", stable_hash(&fingerprint.join("\n"))),
            },
            Err(conflicts) => Effective::Invalid {
                layers,
                reason: format!(
                    "conflicting assignments: {}; fix the assignments or their states",
                    conflicts.join("; ")
                ),
            },
        }
    }
}

/// An attribute's serde form without its `Name`: two attributes with the
/// same content are the same attribute, whatever their names (D29).
fn content(attribute: &Attribute) -> Option<Value> {
    let mut json = serde_json::to_value(attribute).ok()?;
    if let Some(object) = json.as_object_mut() {
        object.remove("Name");
    }
    Some(json)
}

/// Add up the final states of a host's assignments, in order (D29):
/// identical attributes once, conflicts as errors.
fn add_up(compositions: Vec<(String, Composition)>) -> Result<Composition, Vec<String>> {
    let mut attributes: Vec<Attribute> = Vec::new();
    let mut blame: Vec<Blame> = Vec::new();
    let mut warnings = Vec::new();
    let mut contents: Vec<Option<Value>> = Vec::new();
    let mut conflicts = Vec::new();
    for (assignment, composition) in compositions {
        let earlier = attributes.len();
        let mut position = Vec::with_capacity(composition.state.attributes.len());
        for (attribute, mut why) in composition
            .state
            .attributes
            .into_iter()
            .zip(composition.blame)
        {
            let own = content(&attribute);
            if let Some(same) = (0..earlier).find(|&i| own.is_some() && contents[i] == own) {
                blame[same].also_in.push(assignment.clone());
                position.push(same);
                continue;
            }
            let name = attribute.name.as_deref().filter(|n| !n.is_empty());
            let clash = (0..earlier).find_map(|i| {
                let other = &attributes[i];
                let what = if name.is_some() && other.name.as_deref() == name {
                    format!("attribute {}", name.unwrap_or_default())
                } else {
                    let (Some(a), Some(b)) = (&own, &contents[i]) else {
                        return None;
                    };
                    let (na, da) = a.get("Detail")?.as_object()?.iter().next()?;
                    let (nb, db) = b.get("Detail")?.as_object()?.iter().next()?;
                    let (ta, tb) = (target(na, da)?, target(nb, db)?);
                    if na != nb || ta != tb {
                        return None;
                    }
                    if ta.is_empty() {
                        na.clone()
                    } else {
                        format!("{na} {ta}")
                    }
                };
                let diff = match (&own, &contents[i]) {
                    (Some(a), Some(b)) => difference("", b, a),
                    _ => None,
                }
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();
                Some(format!(
                    "{what} is set by {} ({}) and differently by {assignment} ({}){diff}",
                    blame[i].assignment,
                    label(other, i),
                    label(&attribute, attributes.len()),
                ))
            });
            if let Some(clash) = clash {
                conflicts.push(clash);
            }
            why.assignment = assignment.clone();
            position.push(attributes.len());
            attributes.push(attribute);
            blame.push(why);
            contents.push(own);
        }
        for warning in composition.warnings {
            warnings.push(Warning {
                message: warning.message,
                attributes: warning.attributes.iter().map(|&i| position[i]).collect(),
            });
        }
    }
    if !conflicts.is_empty() {
        return Err(conflicts);
    }
    let mut state = ExpectedState::new();
    state.attributes = attributes;
    Ok(Composition {
        state,
        blame,
        warnings,
    })
}

/// Compose a stack of expected states, base first (last wins).
pub fn compose(stack: &[(&str, &ExpectedState)]) -> Composition {
    let mut result: Vec<(Attribute, Blame)> = Vec::new();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    let mut warnings = Vec::new();
    for (origin, state) in stack {
        for attribute in &state.attributes {
            let named = attribute.name.as_ref().filter(|n| !n.is_empty());
            match named.and_then(|n| by_name.get(n).copied()) {
                Some(position) => {
                    let (previous, blame) = &result[position];
                    let mut overrides = blame.overrides.clone();
                    overrides.push(blame.origin.clone());
                    let (before, after) = (nature(previous), nature(attribute));
                    if before != after {
                        warnings.push(Warning {
                            message: format!(
                                "attribute {} is a {} in {}, overridden by a {} in {origin}: is that intended?",
                                named.map(String::as_str).unwrap_or_default(),
                                before.as_deref().unwrap_or("?"),
                                blame.origin,
                                after.as_deref().unwrap_or("?"),
                            ),
                            attributes: vec![position],
                        });
                    }
                    result[position] = (
                        attribute.clone(),
                        Blame {
                            origin: origin.to_string(),
                            overrides,
                            ..Blame::default()
                        },
                    );
                }
                None => {
                    if let Some(name) = named {
                        by_name.insert(name.clone(), result.len());
                    }
                    result.push((
                        attribute.clone(),
                        Blame {
                            origin: origin.to_string(),
                            ..Blame::default()
                        },
                    ));
                }
            }
        }
    }
    let attributes: Vec<Attribute> = result.iter().map(|(a, _)| a.clone()).collect();
    warnings.extend(suspected_conflicts(&attributes, |i| {
        result[i].1.origin.as_str()
    }));
    let mut state = ExpectedState::new();
    state.attributes = attributes;
    Composition {
        state,
        blame: result.into_iter().map(|(_, blame)| blame).collect(),
        warnings,
    }
}

/// The nature of an attribute (`Service`, `Apt`, …): its detail's tag.
fn nature(attribute: &Attribute) -> Option<String> {
    let json = serde_json::to_value(attribute).ok()?;
    json.get("Detail")?.as_object()?.keys().next().cloned()
}

/// What an attribute acts on, within its nature: `None` when unknown (no
/// check).
fn target(nature: &str, detail: &Value) -> Option<String> {
    // enum details (Service { State: {...} }, Docker states, …): look one
    // level down too
    let inner = detail
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.values().next())
        .filter(|v| v.is_object());
    let field = |key: &str| {
        detail
            .get(key)
            .or_else(|| inner.and_then(|i| i.get(key)))
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
    };
    match nature {
        // one per host
        "Hostname" => Some(String::new()),
        "LineInFile" => Some(format!("{} in {}", field("Line")?, field("FilePath")?)),
        "Template" => field("Destination"),
        // a restart or reload goes with a running state: not the same thing
        "Service" if detail.get("Action").is_some() => Some(format!("{} (action)", field("Name")?)),
        "Debug" | "Ping" | "Command" => None,
        _ => ["Name", "Package", "FilePath", "Destination", "Path"]
            .into_iter()
            .find_map(field),
    }
}

/// First difference between two JSON values, as `path: a vs b`.
fn difference(path: &str, a: &Value, b: &Value) -> Option<String> {
    if a == b {
        return None;
    }
    if let (Value::Object(x), Value::Object(y)) = (a, b) {
        let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
        // a different enum variant ({"Started": ..} vs {"Stopped": ..})
        if x.len() == 1 && y.len() == 1 && x.keys().ne(y.keys()) {
            return Some(format!(
                "{path}: {} vs {}",
                x.keys().next()?,
                y.keys().next()?
            ));
        }
        for key in keys {
            let (u, v) = (
                x.get(key).unwrap_or(&Value::Null),
                y.get(key).unwrap_or(&Value::Null),
            );
            let sub = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            if let Some(found) = difference(&sub, u, v) {
                return Some(found);
            }
        }
        return None;
    }
    let show = |v: &Value| match v {
        Value::Null => "(unset)".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    Some(format!("{path}: {} vs {}", show(a), show(b)))
}

fn label(attribute: &Attribute, position: usize) -> String {
    match attribute.name.as_deref().filter(|n| !n.is_empty()) {
        Some(name) => format!("attribute {name}"),
        None => format!("attribute #{}", position + 1),
    }
}

/// Attributes of one final state that likely contradict each other.
fn suspected_conflicts<'a>(
    attributes: &[Attribute],
    origin: impl Fn(usize) -> &'a str,
) -> Vec<Warning> {
    let forms: Vec<Option<(String, Value, String)>> = attributes
        .iter()
        .map(|attribute| {
            let json = serde_json::to_value(attribute).ok()?;
            let (nature, detail) = json.get("Detail")?.as_object()?.iter().next()?;
            let target = target(nature, detail)?;
            Some((nature.clone(), detail.clone(), target))
        })
        .collect();
    let mut warnings = Vec::new();
    for i in 0..attributes.len() {
        let Some((nature, detail, target)) = &forms[i] else {
            continue;
        };
        for j in i + 1..attributes.len() {
            let Some((other_nature, other_detail, other_target)) = &forms[j] else {
                continue;
            };
            if nature != other_nature || target != other_target {
                continue;
            }
            let (a, b) = (label(&attributes[i], i), label(&attributes[j], j));
            let what = if target.is_empty() {
                nature.clone()
            } else {
                format!("{nature} {target}")
            };
            let message = match difference("", detail, other_detail) {
                Some(diff) => format!(
                    "{what}: {a} ({}) and {b} ({}) disagree ({diff}): is that normal?",
                    origin(i),
                    origin(j)
                ),
                None => format!(
                    "{what} is set twice: {a} ({}) and {b} ({}) are identical",
                    origin(i),
                    origin(j)
                ),
            };
            warnings.push(Warning {
                message,
                attributes: vec![i, j],
            });
        }
    }
    warnings
}

/// Whether an applied attribute (or part of one) is the wanted one, the
/// wanted one's Tera templates read as patterns: the agent applies its state
/// rendered (regops renders it on the host, with its own variables), while
/// the assignment's holds the templates. `{{ expr }}` matches any text, the
/// same text wherever the same expression appears in the attribute
/// (`bound`); a `{% … %}` block matches anything.
fn same_attribute(wanted: &Value, applied: &Value, bound: &mut HashMap<String, String>) -> bool {
    match (wanted, applied) {
        (Value::Object(w), Value::Object(a)) => {
            w.len() == a.len()
                && w.iter()
                    .all(|(k, wv)| a.get(k).is_some_and(|av| same_attribute(wv, av, bound)))
        }
        (Value::Array(w), Value::Array(a)) => {
            w.len() == a.len()
                && w.iter()
                    .zip(a)
                    .all(|(wv, av)| same_attribute(wv, av, bound))
        }
        (Value::String(w), Value::String(a)) if w.contains("{{") || w.contains("{%") => {
            template_matches(w, a, bound)
        }
        _ => wanted == applied,
    }
}

/// `template` (Tera) read as a pattern matches `text`; see [`same_attribute`].
fn template_matches(template: &str, text: &str, bound: &mut HashMap<String, String>) -> bool {
    // literal text and placeholders: Some(expression) for {{ }}, None for {% %}
    let mut pattern = String::from("(?s)^");
    let mut holes: Vec<Option<String>> = Vec::new();
    let mut rest = template;
    loop {
        let next = [("{{", "}}"), ("{%", "%}")]
            .into_iter()
            .filter_map(|(open, close)| rest.find(open).map(|at| (at, open, close)))
            .min_by_key(|(at, _, _)| *at);
        let Some((at, open, close)) = next else {
            pattern.push_str(&regex::escape(rest));
            break;
        };
        let Some(end) = rest[at + 2..].find(close) else {
            pattern.push_str(&regex::escape(rest));
            break;
        };
        pattern.push_str(&regex::escape(&rest[..at]));
        pattern.push_str("(.*?)");
        let inner = rest[at + 2..at + 2 + end].trim_matches(['-', ' ']).trim();
        holes.push((open == "{{").then(|| inner.split_whitespace().collect::<Vec<_>>().join(" ")));
        rest = &rest[at + 2 + end + 2..];
    }
    pattern.push('$');
    let Ok(re) = regex::Regex::new(&pattern) else {
        return template == text;
    };
    let Some(found) = re.captures(text) else {
        return false;
    };
    for (i, hole) in holes.iter().enumerate() {
        let (Some(expression), Some(value)) = (hole, found.get(i + 1)) else {
            continue;
        };
        match bound.get(expression) {
            Some(known) if known != value.as_str() => return false,
            Some(_) => {}
            None => {
                bound.insert(expression.clone(), value.as_str().to_string());
            }
        }
    }
    true
}

/// How a proactive host's applied state (D28, the agent's `ExpectedState`
/// as serde JSON) differs from its assignment's final state: `None` when
/// they are the same. Attributes are paired by `Name`, unnamed ones by
/// content; templates of the final state match their rendered values
/// ([`same_attribute`]).
pub fn drift(final_state: &ExpectedState, applied: &Value) -> Option<String> {
    let wanted: Vec<Value> = final_state
        .attributes
        .iter()
        .filter_map(|a| serde_json::to_value(a).ok())
        .collect();
    let got: Vec<Value> = applied["Attributes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let name = |v: &Value| {
        v["Name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(str::to_string)
    };
    let label = |v: &Value| {
        name(v).unwrap_or_else(|| {
            v["Detail"]
                .as_object()
                .and_then(|o| o.keys().next().cloned())
                .map(|nature| format!("an unnamed {nature}"))
                .unwrap_or_else(|| "an unnamed attribute".to_string())
        })
    };
    let mut used = vec![false; got.len()];
    let (mut missing, mut different) = (Vec::new(), Vec::new());
    for w in &wanted {
        let found = got.iter().enumerate().position(|(i, g)| {
            !used[i]
                && match name(w) {
                    Some(n) => name(g).as_deref() == Some(n.as_str()),
                    None => name(g).is_none() && same_attribute(w, g, &mut HashMap::new()),
                }
        });
        match found {
            Some(i) => {
                used[i] = true;
                if !same_attribute(w, &got[i], &mut HashMap::new()) {
                    different.push(label(w));
                }
            }
            None => missing.push(label(w)),
        }
    }
    let extra: Vec<String> = got
        .iter()
        .zip(&used)
        .filter(|(_, u)| !**u)
        .map(|(g, _)| label(g))
        .collect();
    let list = |what: &str, items: &[String]| {
        (!items.is_empty()).then(|| {
            let shown: Vec<&str> = items.iter().take(3).map(String::as_str).collect();
            let more = items.len().saturating_sub(3);
            format!(
                "{} {what} ({}{})",
                items.len(),
                shown.join(", "),
                if more > 0 {
                    format!(", +{more}")
                } else {
                    String::new()
                }
            )
        })
    };
    let parts: Vec<String> = [
        list("missing", &missing),
        list("different", &different),
        list("not in the assignment", &extra),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(yaml: &str) -> ExpectedState {
        yaml_serde::from_str(yaml).unwrap()
    }

    fn line(name: Option<&str>, text: &str) -> String {
        let name = name.map(|n| format!("    Name: {n}\n")).unwrap_or_default();
        format!(
            "  - Privilege: !None\n{name}    Detail: !LineInFile\n      FilePath: /etc/motd\n      State: !Present {{}}\n      Line: !Raw {text}\n"
        )
    }

    fn service(name: &str, unit: &str, status: &str) -> String {
        format!(
            "  - Privilege: !None\n    Name: {name}\n    Detail: !Service\n      Name: {unit}\n      State: {status}\n      Enabled: true\n"
        )
    }

    fn catalog(
        assignments: Vec<(&str, &str, Vec<&str>)>,
        states: Vec<(&str, ExpectedState)>,
    ) -> Vec<CatalogItem> {
        let mut out = Vec::new();
        for (name, selector, stack) in assignments {
            out.push(CatalogItem {
                body: CatalogBody::Assignment(AssignmentDef {
                    source_id: "s".into(),
                    name: name.into(),
                    selector: crate::labels::LabelSelector::parse(selector).unwrap(),
                    priority: 0,
                    expected_states: stack.iter().map(|s| format!("s/{s}")).collect(),
                }),
                revision: "1".into(),
            });
        }
        for (name, expected) in states {
            out.push(CatalogItem {
                body: CatalogBody::ExpectedState(ExpectedStateDef {
                    source_id: "s".into(),
                    name: name.into(),
                    state: expected,
                }),
                revision: "1".into(),
            });
        }
        out
    }

    fn host(labels: &[(&str, &str)]) -> HostLabels {
        let labels: BTreeMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        HostLabels::new("h", ["s"], &labels)
    }

    /// The marker word of each attribute, in order.
    fn texts(state: &ExpectedState) -> Vec<&'static str> {
        state
            .attributes
            .iter()
            .map(|a| {
                let json = serde_json::to_string(a).unwrap();
                ["generic", "always", "pool", "extra", "web", "prod"]
                    .into_iter()
                    .find(|w| json.contains(&format!("\"{w}\"")))
                    .unwrap_or("?")
            })
            .collect()
    }

    #[test]
    fn last_state_wins_with_blame() {
        let base = state(&format!(
            "Attributes:\n{}{}{}",
            line(Some("motd"), "generic"),
            line(None, "always"),
            line(Some("ntp"), "pool")
        ));
        let web = state(&format!(
            "Attributes:\n{}{}",
            line(Some("motd"), "web"),
            line(None, "extra")
        ));
        let prod = state(&format!("Attributes:\n{}", line(Some("motd"), "prod")));
        let items = catalog(
            vec![("web", r#"{role="web"}"#, vec!["base", "web", "prod"])],
            vec![("base", base), ("web", web), ("prod", prod)],
        );
        let assignments = Assignments::from_catalog(&items);
        let Effective::Ready {
            layers,
            state,
            blame,
            revision,
            ..
        } = assignments.effective(&host(&[("role", "web")]))
        else {
            panic!("not ready");
        };
        assert_eq!(layers.len(), 3);
        assert_eq!(texts(&state), ["prod", "always", "pool", "extra"]);
        assert_eq!(blame[0].origin, "s/prod");
        assert_eq!(blame[0].overrides, ["s/base", "s/web"]);
        assert_eq!(blame[3].origin, "s/web");
        assert!(blame[1].overrides.is_empty());

        // another order, another state and revision
        let other = assignments
            .compose_refs(&["s/prod".into(), "s/web".into(), "s/base".into()])
            .unwrap();
        assert_eq!(texts(&other.state)[0], "generic");
        assert!(!revision.is_empty());
        assert!(assignments.compose_refs(&["s/nope".into()]).is_err());

        assert!(matches!(
            assignments.effective(&host(&[("role", "db")])),
            Effective::Unassigned
        ));
    }

    #[test]
    fn assignments_add_up_in_priority_order() {
        let base = state(&format!(
            "Attributes:\n{}{}",
            line(Some("motd"), "generic"),
            service("sshd", "sshd", "Started")
        ));
        // the same sshd attribute under another (typo'd) name: applied once
        let web = state(&format!(
            "Attributes:\n{}{}",
            line(Some("banner"), "web"),
            service("sshd-typo", "sshd", "Started")
        ));
        let mut items = catalog(
            vec![
                ("web", r#"{role="web"}"#, vec!["web"]),
                ("base", "{}", vec!["base"]),
            ],
            vec![("base", base), ("web", web)],
        );
        // base runs first: lower priority
        if let CatalogBody::Assignment(def) = &mut items[1].body {
            def.priority = -10;
        }
        let assignments = Assignments::from_catalog(&items);
        let effective = assignments.effective(&host(&[("role", "web")]));
        assert_eq!(effective.assignments(), ["s/base", "s/web"]);
        let Effective::Ready { state, blame, .. } = effective else {
            panic!("not ready: {effective:?}");
        };
        assert_eq!(texts(&state), ["generic", "?", "web"]);
        assert_eq!(blame[1].assignment, "s/base");
        assert_eq!(blame[1].also_in, ["s/web"]);
        assert_eq!(blame[2].assignment, "s/web");
        // without the label: base only
        assert_eq!(assignments.effective(&host(&[])).assignments(), ["s/base"]);
        // same priority: by name
        if let CatalogBody::Assignment(def) = &mut items[1].body {
            def.priority = 0;
            def.name = "zz".into();
        }
        let assignments = Assignments::from_catalog(&items);
        assert_eq!(
            assignments
                .effective(&host(&[("role", "web")]))
                .assignments(),
            ["s/web", "s/zz"]
        );
    }

    #[test]
    fn conflicts_between_assignments_block() {
        let on = state(&format!(
            "Attributes:\n{}",
            service("httpd", "httpd", "Started")
        ));
        let off = state(&format!(
            "Attributes:\n{}",
            service("web-off", "httpd", "Stopped")
        ));
        let renamed = state(&format!("Attributes:\n{}", line(Some("httpd"), "x")));
        let items = catalog(
            vec![
                ("a", "{}", vec!["on"]),
                ("b", r#"{off="1"}"#, vec!["off"]),
                ("c", r#"{named="1"}"#, vec!["renamed"]),
            ],
            vec![("on", on), ("off", off), ("renamed", renamed)],
        );
        let assignments = Assignments::from_catalog(&items);
        // same target, other content
        match assignments.effective(&host(&[("off", "1")])) {
            Effective::Invalid { reason, layers } => {
                assert!(reason.contains("Service httpd"), "{reason}");
                assert!(reason.contains("s/a") && reason.contains("s/b"), "{reason}");
                assert_eq!(layers.len(), 2);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        // same Name, other content
        match assignments.effective(&host(&[("named", "1")])) {
            Effective::Invalid { reason, .. } => {
                assert!(reason.contains("attribute httpd"), "{reason}")
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert!(matches!(
            assignments.effective(&host(&[])),
            Effective::Ready { .. }
        ));
    }

    #[test]
    fn same_priority_and_name_is_ambiguous() {
        let base = state(&format!("Attributes:\n{}", line(Some("motd"), "a")));
        let mut items = catalog(vec![("web", "{}", vec!["base"])], vec![("base", base)]);
        let mut other = items[0].clone();
        if let CatalogBody::Assignment(def) = &mut other.body {
            def.source_id = "t".into();
        }
        items.push(other);
        let assignments = Assignments::from_catalog(&items);
        match assignments.effective(&host(&[])) {
            Effective::Invalid { reason, .. } => {
                assert!(reason.contains("ambiguous"), "{reason}")
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn contradictions_are_warned() {
        let base = state(&format!(
            "Attributes:\n{}",
            service("httpd", "httpd", "Started")
        ));
        let maintenance = state(&format!(
            "Attributes:\n{}{}",
            service("httpd-off", "httpd", "Stopped"),
            service("sshd", "sshd", "Started")
        ));
        let c = compose(&[("s/base", &base), ("s/maint", &maintenance)]);
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        let message = &c.warnings[0].message;
        assert!(
            message.contains("httpd") && message.contains("Started"),
            "{message}"
        );
        assert!(message.contains("Stopped"), "{message}");
        assert_eq!(c.warnings[0].attributes, [0, 1]);

        // an override by Name is no conflict
        let off = state(&format!(
            "Attributes:\n{}",
            service("httpd", "httpd", "Stopped")
        ));
        let c = compose(&[("s/base", &base), ("s/off", &off)]);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.blame[0].overrides, ["s/base"]);

        // an override changing the nature is
        let other = state(&format!("Attributes:\n{}", line(Some("httpd"), "x")));
        let c = compose(&[("s/base", &base), ("s/other", &other)]);
        assert_eq!(c.warnings.len(), 1);
        assert!(
            c.warnings[0].message.contains("Service"),
            "{:?}",
            c.warnings
        );

        // the same line twice under two names
        let twice = state(&format!(
            "Attributes:\n{}{}",
            line(Some("a"), "same"),
            line(Some("b"), "same")
        ));
        let c = compose(&[("s/twice", &twice)]);
        assert_eq!(c.warnings.len(), 1);
        assert!(
            c.warnings[0].message.contains("identical"),
            "{:?}",
            c.warnings
        );
    }

    #[test]
    fn drift_of_an_applied_state() {
        let final_state = state(&format!(
            "Attributes:\n{}{}",
            line(Some("motd"), "web"),
            line(None, "always")
        ));
        let same = serde_json::to_value(&final_state).unwrap();
        assert_eq!(drift(&final_state, &same), None);
        let other = state(&format!(
            "Attributes:\n{}{}",
            line(Some("motd"), "old"),
            line(Some("ntp"), "pool")
        ));
        let report = drift(&final_state, &serde_json::to_value(&other).unwrap()).unwrap();
        assert!(
            report.contains("1 missing (an unnamed LineInFile)"),
            "{report}"
        );
        assert!(report.contains("1 different (motd)"), "{report}");
        assert!(report.contains("1 not in the assignment (ntp)"), "{report}");
    }

    #[test]
    fn rendered_templates_are_no_drift() {
        let wanted = |file: &str, line: &str| {
            state(&format!(
                "Attributes:\n  - Name: motd\n    Privilege: !None\n    Detail: !LineInFile\n      FilePath: \"{file}\"\n      State: !Present\n        Create: true\n      Line: !Raw \"{line}\"\n"
            ))
        };
        let template = wanted(
            "/srv/{{ host }}/motd",
            "hello {{host}} {% if x %}x{% endif %}",
        );
        let applied = |file: &str, line: &str| serde_json::to_value(wanted(file, line)).unwrap();
        assert_eq!(
            drift(&template, &applied("/srv/web-01/motd", "hello web-01 x")),
            None
        );
        // the same variable with two values, a changed literal part
        assert!(drift(&template, &applied("/srv/web-01/motd", "hello web-02 x")).is_some());
        assert!(drift(&template, &applied("/etc/web-01/motd", "hello web-01 x")).is_some());
        assert!(template_matches("a {{ b", "a {{ b", &mut HashMap::new()));
        assert!(!template_matches("a {{ b", "a c", &mut HashMap::new()));
    }
}
