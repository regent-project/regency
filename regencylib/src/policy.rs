//! Cedar authorization (D12), embedded in backend and scheduler.
//!
//! - Principals: `User`, `ServiceAccount`, members of `Group`s.
//! - Resources: `Source` ⊃ `Inventory` ⊃ `Host`, plus `ExpectedState`,
//!   `Order`, `Settings` (singleton `Settings::"global"`, also the resource of
//!   user management) and `Policy`.
//! - Actions: `read`, and one per command (D10/D17): `assess` / `reach` (run
//!   and orders, per resolved host, D16), `abort`, `order.upsert`, …
//! - Shipped role templates (always loaded): groups `viewers`, `operators`
//!   (Assess, abort), `enforcers` (+ Reach), `admins` (everything).
//! - The bootstrap admin is always permitted (lockout guard).
//!
//! Live-view filtering evaluates each host once per subscription and caches
//! the decision (Cedar's partial evaluation is an experimental feature).

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use cedar_policy::{
    Context, Decision, Entities, Entity, EntityId, EntityTypeName, EntityUid, PolicyId, PolicySet,
    Request, Schema, ValidationMode, Validator,
};

use crate::model::PolicyDoc;

pub const SCHEMA: &str = r#"
entity Group;
entity User in [Group];
entity ServiceAccount in [Group];
entity Source;
entity Inventory in [Source];
entity Host in [Inventory, Source];
entity ExpectedState in [Source];
entity Order;
entity Settings;
entity Policy;

action "read" appliesTo {
  principal: [User, ServiceAccount],
  resource: [Host, Source, Inventory, ExpectedState, Order, Settings, Policy],
};
action "assess", "reach", "abort", "host.delete", "host.approve_cert", "host.enroll" appliesTo {
  principal: [User, ServiceAccount],
  resource: [Host],
};
action "order.upsert", "order.delete" appliesTo {
  principal: [User, ServiceAccount],
  resource: [Order],
};
action "settings.set", "config.set", "user.upsert", "user.delete" appliesTo {
  principal: [User, ServiceAccount],
  resource: [Settings],
};
action "catalog.upsert", "catalog.delete" appliesTo {
  principal: [User, ServiceAccount],
  resource: [Source, Inventory, Host, ExpectedState],
};
action "source.refresh" appliesTo {
  principal: [User, ServiceAccount],
  resource: [Source],
};
action "policy.upsert", "policy.delete" appliesTo {
  principal: [User, ServiceAccount],
  resource: [Policy],
};
"#;

/// Shipped role templates (D12).
pub const TEMPLATES: &[(&str, &str)] = &[
    (
        "builtin-viewer",
        r#"permit(principal in Group::"viewers", action == Action::"read", resource);"#,
    ),
    (
        "builtin-operator",
        r#"permit(principal in Group::"operators", action in [Action::"read", Action::"assess", Action::"abort"], resource);"#,
    ),
    (
        "builtin-enforcer",
        r#"permit(principal in Group::"enforcers", action in [Action::"read", Action::"assess", Action::"reach", Action::"abort"], resource);"#,
    ),
    (
        "builtin-admin",
        r#"permit(principal in Group::"admins", action, resource);"#,
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrincipalKind {
    User,
    ServiceAccount,
}

/// An authenticated caller.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Principal {
    pub kind: PrincipalKind,
    pub name: String,
    pub groups: Vec<String>,
}

impl Principal {
    pub fn user(name: &str, groups: &[&str]) -> Self {
        Self {
            kind: PrincipalKind::User,
            name: name.to_string(),
            groups: groups.iter().map(|g| g.to_string()).collect(),
        }
    }

    /// `User::"alice"` (the form stored as `issued_by` / order `author`).
    pub fn display(&self) -> String {
        format!("{}::{:?}", self.type_name(), self.name)
    }

    fn type_name(&self) -> &'static str {
        match self.kind {
            PrincipalKind::User => "User",
            PrincipalKind::ServiceAccount => "ServiceAccount",
        }
    }

    /// Parse [`Principal::display`] (groups are not part of it).
    pub fn parse(text: &str) -> Option<Self> {
        let (kind, rest) = text.split_once("::")?;
        let kind = match kind {
            "User" => PrincipalKind::User,
            "ServiceAccount" => PrincipalKind::ServiceAccount,
            _ => return None,
        };
        let name: String = serde_json::from_str(rest).ok()?;
        Some(Self {
            kind,
            name,
            groups: Vec::new(),
        })
    }
}

/// What an action applies to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Resource {
    /// A host with its inventories (`source/name`) and sources.
    Host {
        id: String,
        inventories: Vec<String>,
        sources: Vec<String>,
    },
    Source(String),
    /// `source/name`.
    Inventory(String),
    /// `source/name`.
    ExpectedState(String),
    Order(String),
    Settings,
    Policy(String),
}

fn uid(kind: &str, id: &str) -> EntityUid {
    EntityUid::from_type_name_and_id(
        EntityTypeName::from_str(kind).expect("valid type"),
        EntityId::new(id),
    )
}

fn source_of(reference: &str) -> Option<&str> {
    reference.split_once('/').map(|(s, _)| s)
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("invalid Cedar policy {id}: {details}")]
    Parse { id: String, details: String },
    #[error("Cedar policy {id} does not match the schema: {details}")]
    Validation { id: String, details: String },
}

/// Policy set + schema, evaluated in-process.
#[derive(Clone)]
pub struct Authorizer {
    schema: Schema,
    policies: PolicySet,
    bootstrap_admin: Option<String>,
}

impl Authorizer {
    /// Templates + `docs`. An invalid doc is skipped (logged): it was validated
    /// at acceptance, so this only happens across schema changes.
    pub fn new(docs: &[PolicyDoc], bootstrap_admin: Option<String>) -> Self {
        let schema = schema();
        let mut policies = PolicySet::new();
        let builtins = TEMPLATES
            .iter()
            .map(|(id, text)| (id.to_string(), text.to_string()));
        let stored = docs.iter().map(|d| (d.policy_id.clone(), d.text.clone()));
        for (id, text) in builtins.chain(stored) {
            match parse(&id, &text).and_then(|set| validate(&schema, &id, &set).map(|_| set)) {
                Ok(set) => {
                    for policy in set.policies() {
                        let _ = policies.add(policy.clone());
                    }
                }
                Err(e) => tracing::warn!(error = %e, "ignoring policy"),
            }
        }
        Self {
            schema,
            policies,
            bootstrap_admin,
        }
    }

    /// Check a policy text before accepting it (backend, D12).
    pub fn validate_policy(id: &str, text: &str) -> Result<(), PolicyError> {
        let set = parse(id, text)?;
        validate(&schema(), id, &set)
    }

    pub fn is_bootstrap_admin(&self, principal: &Principal) -> bool {
        principal.kind == PrincipalKind::User
            && self.bootstrap_admin.as_deref() == Some(principal.name.as_str())
    }

    pub fn is_authorized(&self, principal: &Principal, action: &str, resource: &Resource) -> bool {
        if self.is_bootstrap_admin(principal) {
            return true;
        }
        let mut entities: Vec<Entity> = Vec::new();
        let groups: HashSet<EntityUid> = principal.groups.iter().map(|g| uid("Group", g)).collect();
        for group in &groups {
            entities.push(Entity::new_no_attrs(group.clone(), HashSet::new()));
        }
        let principal_uid = uid(principal.type_name(), &principal.name);
        entities.push(Entity::new_no_attrs(principal_uid.clone(), groups));

        let resource_uid = match resource {
            Resource::Host {
                id,
                inventories,
                sources,
            } => {
                let mut parents = HashSet::new();
                let mut all_sources: HashSet<String> = sources.iter().cloned().collect();
                for inventory in inventories {
                    let source = source_of(inventory).unwrap_or_default().to_string();
                    let inv = uid("Inventory", inventory);
                    entities.push(Entity::new_no_attrs(
                        inv.clone(),
                        HashSet::from([uid("Source", &source)]),
                    ));
                    parents.insert(inv);
                    all_sources.insert(source);
                }
                for source in &all_sources {
                    let s = uid("Source", source);
                    entities.push(Entity::new_no_attrs(s.clone(), HashSet::new()));
                    parents.insert(s);
                }
                let host = uid("Host", id);
                entities.push(Entity::new_no_attrs(host.clone(), parents));
                host
            }
            Resource::Source(id) => {
                let s = uid("Source", id);
                entities.push(Entity::new_no_attrs(s.clone(), HashSet::new()));
                s
            }
            Resource::Inventory(reference) | Resource::ExpectedState(reference) => {
                let kind = if matches!(resource, Resource::Inventory(_)) {
                    "Inventory"
                } else {
                    "ExpectedState"
                };
                let source = uid("Source", source_of(reference).unwrap_or_default());
                entities.push(Entity::new_no_attrs(source.clone(), HashSet::new()));
                let item = uid(kind, reference);
                entities.push(Entity::new_no_attrs(item.clone(), HashSet::from([source])));
                item
            }
            Resource::Order(id) => uid("Order", id),
            Resource::Settings => uid("Settings", "global"),
            Resource::Policy(id) => uid("Policy", id),
        };
        let Ok(entities) = Entities::from_entities(entities, Some(&self.schema)) else {
            return false;
        };
        let Ok(request) = Request::new(
            principal_uid,
            uid("Action", action),
            resource_uid,
            Context::empty(),
            Some(&self.schema),
        ) else {
            return false;
        };
        cedar_policy::Authorizer::new()
            .is_authorized(&request, &self.policies, &entities)
            .decision()
            == Decision::Allow
    }
}

fn schema() -> Schema {
    Schema::from_cedarschema_str(SCHEMA)
        .expect("valid built-in schema")
        .0
}

fn parse(id: &str, text: &str) -> Result<PolicySet, PolicyError> {
    let parsed = PolicySet::from_str(text).map_err(|e| PolicyError::Parse {
        id: id.to_string(),
        details: e.to_string(),
    })?;
    // unique ids across documents: `<doc id>#<n>`
    let mut renamed = PolicySet::new();
    for (n, policy) in parsed.policies().enumerate() {
        let policy = policy.new_id(PolicyId::new(format!("{id}#{n}")));
        renamed.add(policy).map_err(|e| PolicyError::Parse {
            id: id.to_string(),
            details: e.to_string(),
        })?;
    }
    if parsed.templates().next().is_some() {
        return Err(PolicyError::Parse {
            id: id.to_string(),
            details: "Cedar templates are not supported, write plain policies".to_string(),
        });
    }
    Ok(renamed)
}

fn validate(schema: &Schema, id: &str, set: &PolicySet) -> Result<(), PolicyError> {
    let result = Validator::new(schema.clone()).validate(set, ValidationMode::Strict);
    if result.validation_passed() {
        Ok(())
    } else {
        Err(PolicyError::Validation {
            id: id.to_string(),
            details: result
                .validation_errors()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        })
    }
}

/// Per-subscription cache of read decisions (live-view filtering).
#[derive(Default)]
pub struct DecisionCache(HashMap<Resource, bool>);

impl DecisionCache {
    pub fn allowed(
        &mut self,
        authorizer: &Authorizer,
        principal: &Principal,
        resource: &Resource,
    ) -> bool {
        *self
            .0
            .entry(resource.clone())
            .or_insert_with(|| authorizer.is_authorized(principal, "read", resource))
    }
}

impl Resource {
    /// The `Host` resource of `host_id` with its inventories and sources from
    /// the catalog's host definitions (D11 hierarchy).
    pub fn host(host_id: &str, hostdefs: &[crate::model::HostDef]) -> Self {
        Self::host_of(host_id, hostdefs.iter().filter(|d| d.host_id == host_id))
    }

    /// Every host's resource at once: [`Resource::host`] scans all the
    /// definitions, which is quadratic over a fleet.
    pub fn hosts(
        hostdefs: &[crate::model::HostDef],
    ) -> std::collections::HashMap<String, Resource> {
        let mut by_host: std::collections::HashMap<&str, Vec<&crate::model::HostDef>> =
            std::collections::HashMap::new();
        for def in hostdefs {
            by_host.entry(def.host_id.as_str()).or_default().push(def);
        }
        by_host
            .into_iter()
            .map(|(id, defs)| (id.to_string(), Self::host_of(id, defs)))
            .collect()
    }

    /// The resource of a host from its definitions (all of `host_id`).
    pub fn host_of<'a>(
        host_id: &str,
        defs: impl IntoIterator<Item = &'a crate::model::HostDef>,
    ) -> Self {
        let mut inventories = std::collections::BTreeSet::new();
        let mut sources = std::collections::BTreeSet::new();
        for def in defs {
            sources.insert(def.source_id.clone());
            for name in &def.inventories {
                inventories.insert(format!("{}/{}", def.source_id, name));
            }
        }
        Resource::Host {
            id: host_id.to_string(),
            inventories: inventories.into_iter().collect(),
            sources: sources.into_iter().collect(),
        }
    }
}

/// Cedar action of a job (D12).
pub fn job_action(job: regent_sdk::task::Job) -> &'static str {
    match job {
        regent_sdk::task::Job::Assess => "assess",
        regent_sdk::task::Job::Reach => "reach",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, inventory: &str) -> Resource {
        Resource::Host {
            id: id.into(),
            inventories: vec![inventory.into()],
            sources: vec![],
        }
    }

    #[test]
    fn templates_give_roles() {
        let authz = Authorizer::new(&[], Some("admin".into()));
        let viewer = Principal::user("v", &["viewers"]);
        let operator = Principal::user("o", &["operators"]);
        let enforcer = Principal::user("e", &["enforcers"]);
        let nobody = Principal::user("n", &[]);
        let h = host("web-01", "git/web");
        assert!(authz.is_authorized(&viewer, "read", &h));
        assert!(!authz.is_authorized(&viewer, "assess", &h));
        assert!(authz.is_authorized(&operator, "assess", &h));
        assert!(!authz.is_authorized(&operator, "reach", &h));
        assert!(authz.is_authorized(&enforcer, "reach", &h));
        assert!(!authz.is_authorized(&nobody, "read", &h));
        assert!(authz.is_authorized(
            &Principal::user("admin", &[]),
            "settings.set",
            &Resource::Settings
        ));
    }

    #[test]
    fn stored_policies_follow_the_hierarchy() {
        let doc = PolicyDoc {
            policy_id: "web-team".into(),
            text: r#"permit(principal == User::"alice", action == Action::"reach", resource in Inventory::"git/web");"#.into(),
            description: String::new(),
        };
        Authorizer::validate_policy(&doc.policy_id, &doc.text).unwrap();
        let authz = Authorizer::new(&[doc], None);
        let alice = Principal::user("alice", &[]);
        assert!(authz.is_authorized(&alice, "reach", &host("web-01", "git/web")));
        assert!(!authz.is_authorized(&alice, "reach", &host("db-01", "git/db")));
    }

    #[test]
    fn invalid_policies_are_refused() {
        assert!(Authorizer::validate_policy("x", "permit(").is_err());
        assert!(
            Authorizer::validate_policy(
                "x",
                r#"permit(principal, action == Action::"fly", resource);"#
            )
            .is_err()
        );
    }

    #[test]
    fn principal_display_roundtrip() {
        let p = Principal::user("al\"ice", &["g"]);
        let back = Principal::parse(&p.display()).unwrap();
        assert_eq!(back.name, "al\"ice");
        assert_eq!(back.kind, PrincipalKind::User);
    }
}
