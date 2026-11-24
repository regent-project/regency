//! What backend follows (D1): `_changes` of `regency_state`, the current and
//! previous month's jobs databases, the catalog, accounts and policies. Kept
//! in memory (hosts, catalog, accounts, policies) or streamed (jobs), and fanned
//! out to subscriptions through broadcast channels.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::Utc;
use regencylib::assignment::Assignments;
use regencylib::couch::{Change, Couch, Since, follow};
use regencylib::model::{
    AssignmentDef, CatalogBody, CatalogItem, DB_CATALOG, DB_POLICIES, DB_STATE, DB_USERS, HostDef,
    HostDoc, JobDoc, PolicyDoc, SourceHeadDoc, UserDoc, jobs_db, month_offset,
};
use regencylib::policy::Authorizer;
use regencylib::selector::{HostSelector, conflicting_hosts, resolve};
use regencylib::telemetry::Health;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(Clone, Debug)]
pub enum HostChange {
    Upsert(Box<HostDoc>),
    Removed(String),
}

#[derive(Default)]
struct Catalog {
    items: HashMap<String, CatalogItem>,
    hostdefs: Vec<HostDef>,
    assignment_defs: Vec<AssignmentDef>,
    conflicting: HashSet<String>,
    /// host id → (own labels, all labels with the built-in ones), all
    /// sources merged (D26)
    by_host: HashMap<
        String,
        (
            std::collections::BTreeMap<String, String>,
            regencylib::labels::HostLabels,
        ),
    >,
    assignments: Assignments,
    /// `items` changed since the derived fields were last rebuilt.
    dirty: bool,
}

pub struct View {
    pub couch: Couch,
    hosts: RwLock<HashMap<String, HostDoc>>,
    /// D28: the commit each Git source was last synchronized at.
    source_heads: RwLock<HashMap<String, SourceHeadDoc>>,
    catalog: RwLock<Catalog>,
    catalog_version: AtomicU64,
    users: RwLock<HashMap<String, UserDoc>>,
    policies: RwLock<HashMap<String, PolicyDoc>>,
    /// D21 instance statuses, by doc id.
    instances: RwLock<std::collections::BTreeMap<String, regencylib::central::InstanceStatus>>,
    authorizer: RwLock<Authorizer>,
    bootstrap_admin: String,
    pub host_changes: broadcast::Sender<HostChange>,
    pub job_changes: broadcast::Sender<JobDoc>,
}

fn parse<T: serde::de::DeserializeOwned>(change: &Change) -> Option<T> {
    let doc = change.doc.as_ref()?;
    serde_json::from_value(doc.clone())
        .map_err(|e| warn!(doc_id = %change.id, error = %e, "unreadable document ignored"))
        .ok()
}

impl View {
    /// Every known instance (D21).
    pub fn instances(&self) -> Vec<regencylib::central::InstanceStatus> {
        self.instances
            .read()
            .expect("instances")
            .values()
            .cloned()
            .collect()
    }

    pub fn new(couch: Couch, bootstrap_admin: String) -> Arc<Self> {
        Arc::new(Self {
            couch,
            hosts: RwLock::default(),
            source_heads: RwLock::default(),
            catalog: RwLock::default(),
            catalog_version: AtomicU64::new(0),
            users: RwLock::default(),
            policies: RwLock::default(),
            instances: RwLock::default(),
            authorizer: RwLock::new(Authorizer::new(&[], Some(bootstrap_admin.clone()))),
            bootstrap_admin,
            host_changes: broadcast::channel(4096).0,
            job_changes: broadcast::channel(16384).0,
        })
    }

    pub fn spawn(self: &Arc<Self>, health: &Health, cancel: &CancellationToken) {
        for (db, check) in [
            (DB_STATE, "hosts"),
            (DB_CATALOG, "catalog"),
            (DB_USERS, "users"),
            (DB_POLICIES, "policies"),
        ] {
            health.register(check);
            let (view, health, cancel) = (self.clone(), health.clone(), cancel.clone());
            tokio::spawn(async move {
                let couch = view.couch.clone();
                follow(
                    &couch,
                    db,
                    Since::Start,
                    &cancel,
                    |change| view.apply(db, change),
                    || {
                        view.batch_applied(db);
                        health.ready(check)
                    },
                )
                .await;
            });
        }
        // jobs: current and previous month, re-evaluated every minute
        let (view, cancel) = (self.clone(), cancel.clone());
        tokio::spawn(async move {
            let mut followed: HashMap<String, CancellationToken> = HashMap::new();
            loop {
                let now = Utc::now();
                let wanted: HashSet<String> = [month_offset(now, 0), month_offset(now, -1)]
                    .iter()
                    .map(|m| jobs_db(m))
                    .collect();
                followed.retain(|db, token| {
                    let keep = wanted.contains(db);
                    if !keep {
                        token.cancel();
                    }
                    keep
                });
                for db in wanted {
                    if followed.contains_key(&db) {
                        continue;
                    }
                    let token = cancel.child_token();
                    followed.insert(db.clone(), token.clone());
                    let view = view.clone();
                    tokio::spawn(async move {
                        info!(db, "following jobs");
                        let couch = view.couch.clone();
                        follow(
                            &couch,
                            &db,
                            Since::Now,
                            &token,
                            |change| {
                                if !change.deleted
                                    && change.id.starts_with("job:")
                                    && let Some(job) = parse::<JobDoc>(&change)
                                {
                                    let _ = view.job_changes.send(job);
                                }
                            },
                            || {},
                        )
                        .await;
                    });
                }
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                }
            }
        });
    }

    fn apply(&self, db: &str, change: Change) {
        if change.id.starts_with("_design/") {
            return;
        }
        match db {
            DB_STATE if change.id.starts_with("instance:") => {
                let mut instances = self.instances.write().expect("instances");
                if change.deleted {
                    instances.remove(&change.id);
                } else if let Some(doc) = parse::<regencylib::central::InstanceDoc>(&change) {
                    instances.insert(change.id.clone(), doc.status);
                }
            }
            DB_STATE if change.id.starts_with("source_head:") => {
                let mut heads = self.source_heads.write().expect("source heads");
                if change.deleted {
                    heads.remove(&change.id);
                } else if let Some(doc) = parse::<SourceHeadDoc>(&change) {
                    heads.insert(change.id.clone(), doc);
                }
            }
            DB_STATE => {
                let Some(host_id) = change.id.strip_prefix("host:") else {
                    return;
                };
                if change.deleted {
                    self.hosts.write().expect("hosts").remove(host_id);
                    let _ = self
                        .host_changes
                        .send(HostChange::Removed(host_id.to_string()));
                } else if let Some(doc) = parse::<HostDoc>(&change) {
                    self.hosts
                        .write()
                        .expect("hosts")
                        .insert(host_id.to_string(), doc.clone());
                    let _ = self.host_changes.send(HostChange::Upsert(Box::new(doc)));
                }
            }
            DB_CATALOG => {
                let mut catalog = self.catalog.write().expect("catalog");
                catalog.items.remove(&change.id);
                if !change.deleted
                    && let Some(item) = parse::<CatalogItem>(&change)
                {
                    catalog.items.insert(change.id.clone(), item);
                }
                // derived fields: once per `_changes` batch (batch_applied),
                // not per change (quadratic on a large catalog's catch-up)
                catalog.dirty = true;
            }
            DB_USERS => {
                let Some(name) = change.id.strip_prefix("user:") else {
                    return;
                };
                let mut users = self.users.write().expect("users");
                users.remove(name);
                if !change.deleted
                    && let Some(user) = parse::<UserDoc>(&change)
                {
                    users.insert(name.to_string(), user);
                }
            }
            DB_POLICIES => {
                let mut policies = self.policies.write().expect("policies");
                policies.remove(&change.id);
                if !change.deleted
                    && let Some(policy) = parse::<PolicyDoc>(&change)
                {
                    policies.insert(change.id.clone(), policy);
                }
                let docs: Vec<PolicyDoc> = policies.values().cloned().collect();
                *self.authorizer.write().expect("authorizer") =
                    Authorizer::new(&docs, Some(self.bootstrap_admin.clone()));
            }
            _ => {}
        }
    }

    /// After a batch of changes of `db`: rebuild what derives from it.
    fn batch_applied(&self, db: &str) {
        if db != DB_CATALOG {
            return;
        }
        let mut catalog = self.catalog.write().expect("catalog");
        if !catalog.dirty {
            return;
        }
        catalog.dirty = false;
        catalog.hostdefs = catalog
            .items
            .values()
            .filter_map(|i| match &i.body {
                CatalogBody::Hostdef(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        catalog.assignment_defs = catalog
            .items
            .values()
            .filter_map(|i| match &i.body {
                CatalogBody::Assignment(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        catalog.conflicting = conflicting_hosts(&catalog.hostdefs).into_iter().collect();
        // D26: a host's labels, own and built-in
        let mut defs_of: HashMap<&str, Vec<&HostDef>> = HashMap::new();
        for def in &catalog.hostdefs {
            defs_of.entry(def.host_id.as_str()).or_default().push(def);
        }
        let by_host = defs_of
            .into_iter()
            .map(|(host_id, defs)| {
                let own: std::collections::BTreeMap<String, String> =
                    defs.iter().flat_map(|d| d.labels.clone()).collect();
                let labels = regencylib::selector::host_labels(host_id, &defs);
                (host_id.to_string(), (own, labels))
            })
            .collect();
        catalog.by_host = by_host;
        catalog.assignments = Assignments::from_catalog(catalog.items.values());
        self.catalog_version.fetch_add(1, Ordering::Relaxed);
    }

    pub fn authorizer(&self) -> Authorizer {
        self.authorizer.read().expect("authorizer").clone()
    }

    pub fn user(&self, name: &str) -> Option<UserDoc> {
        self.users.read().expect("users").get(name).cloned()
    }

    pub fn users(&self) -> Vec<UserDoc> {
        self.users
            .read()
            .expect("users")
            .values()
            .cloned()
            .collect()
    }

    pub fn policies(&self) -> Vec<PolicyDoc> {
        self.policies
            .read()
            .expect("policies")
            .values()
            .cloned()
            .collect()
    }

    pub fn hosts(&self) -> Vec<HostDoc> {
        self.hosts
            .read()
            .expect("hosts")
            .values()
            .cloned()
            .collect()
    }

    pub fn catalog_items(&self) -> Vec<(String, CatalogItem)> {
        self.catalog
            .read()
            .expect("catalog")
            .items
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn catalog_item(&self, doc_id: &str) -> Option<CatalogItem> {
        self.catalog
            .read()
            .expect("catalog")
            .items
            .get(doc_id)
            .cloned()
    }

    pub fn hostdefs(&self) -> (Vec<HostDef>, Vec<AssignmentDef>) {
        let catalog = self.catalog.read().expect("catalog");
        (catalog.hostdefs.clone(), catalog.assignment_defs.clone())
    }

    pub fn catalog_version(&self) -> u64 {
        self.catalog_version.load(Ordering::Relaxed)
    }

    /// Labels, conflict flag and effective expected state (every assignment
    /// selecting it, D29) of a host, from the catalog (D11, D23).
    pub fn host_meta(&self, host_id: &str) -> crate::convert::HostMeta {
        let report = self
            .hosts
            .read()
            .expect("hosts")
            .get(host_id)
            .and_then(|h| h.agent.clone());
        let catalog = self.catalog.read().expect("catalog");
        let (labels, all_labels) = match &report {
            // D28: the agent's labels join the definitions' (which win)
            Some(report) => Self::agent_labels(&catalog, host_id, report),
            None => catalog.by_host.get(host_id).cloned().unwrap_or_else(|| {
                (
                    Default::default(),
                    regencylib::labels::HostLabels::new(
                        host_id,
                        std::iter::empty::<&str>(),
                        &Default::default(),
                    ),
                )
            }),
        };
        let effective = catalog.assignments.effective(&all_labels);
        let revisions = effective
            .assignments()
            .into_iter()
            .map(|a| {
                let revision = catalog.assignments.revision_of(&a);
                (a, revision)
            })
            .collect();
        let agent = report.as_ref().map(|report| {
            let head = self
                .source_heads
                .read()
                .expect("source heads")
                .values()
                .find(|h| h.serves(&report.repo, &report.branch))
                .cloned();
            let repo_status = match &head {
                None => "unknown",
                Some(h) if !report.revision.is_empty() && h.revision == report.revision => {
                    "up_to_date"
                }
                Some(_) => "behind",
            };
            let drift = match (&effective, &report.expected_state) {
                (regencylib::assignment::Effective::Ready { state, .. }, Some(applied)) => {
                    regencylib::assignment::drift(state, applied)
                }
                _ => None,
            };
            crate::convert::AgentMeta {
                repo_status,
                repo_head: head
                    .as_ref()
                    .map(|h| h.revision.clone())
                    .unwrap_or_default(),
                repo_source: head.map(|h| h.source_id).unwrap_or_default(),
                drift,
            }
        });
        crate::convert::HostMeta {
            labels: labels.into_iter().collect(),
            conflicting: catalog.conflicting.contains(host_id),
            effective,
            revisions,
            agent,
        }
    }

    /// A proactive host's own labels (its agent's, then its definitions',
    /// which win on the same key) and its labels with the built-in ones
    /// (`source` gains the agent's name), D28.
    fn agent_labels(
        catalog: &Catalog,
        host_id: &str,
        report: &regencylib::model::AgentReport,
    ) -> (
        std::collections::BTreeMap<String, String>,
        regencylib::labels::HostLabels,
    ) {
        let defs: Vec<&HostDef> = catalog
            .hostdefs
            .iter()
            .filter(|d| d.host_id == host_id)
            .collect();
        let mut own = report.labels.clone();
        own.extend(defs.iter().flat_map(|d| d.labels.clone()));
        let sources = defs
            .iter()
            .map(|d| d.source_id.as_str())
            .chain([report.agent.as_str()]);
        let labels = regencylib::labels::HostLabels::new(host_id, sources, &own);
        (own, labels)
    }

    /// Cedar resource of a host (D12), the assignments selecting it
    /// computed from its definitions' and agent's labels (D28, D29).
    pub fn host_resource(&self, host_id: &str) -> regencylib::policy::Resource {
        let meta = self.host_meta(host_id);
        let catalog = self.catalog.read().expect("catalog");
        let mut sources: std::collections::BTreeSet<String> = catalog
            .hostdefs
            .iter()
            .filter(|d| d.host_id == host_id)
            .map(|d| d.source_id.clone())
            .collect();
        if let Some(report) = self
            .hosts
            .read()
            .expect("hosts")
            .get(host_id)
            .and_then(|h| h.agent.as_ref())
        {
            sources.insert(report.agent.clone());
        }
        regencylib::policy::Resource::Host {
            id: host_id.to_string(),
            assignments: meta.effective.assignments(),
            sources: sources.into_iter().collect(),
        }
    }

    /// Assignments and expected states of the catalog (D23).
    pub fn assignments(&self) -> Assignments {
        self.catalog.read().expect("catalog").assignments.clone()
    }

    /// Host ids a selector designates (empty selector: `None` = all).
    pub fn selected(&self, selector: &HostSelector) -> Option<HashSet<String>> {
        if selector.is_empty() {
            return None;
        }
        let catalog = self.catalog.read().expect("catalog");
        let resolution = resolve(selector, &catalog.hostdefs, &catalog.assignment_defs, None);
        let mut ids: HashSet<String> = selector.hosts.iter().cloned().collect();
        ids.extend(resolution.targets.into_iter().map(|t| t.host_id));
        ids.extend(resolution.conflicting);
        // D28: proactive hosts, by their agent's labels
        for host in self.hosts.read().expect("hosts").values() {
            let Some(report) = &host.agent else { continue };
            let (_, labels) = Self::agent_labels(&catalog, &host.host_id, report);
            let assignments =
                regencylib::selector::assignments_of(&labels, &catalog.assignment_defs);
            if selector.matches(&host.host_id, &assignments, &labels) {
                ids.insert(host.host_id.clone());
            }
        }
        Some(ids)
    }
}
