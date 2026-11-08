//! CouchDB document model (D8, D11, D12, D16) and its deterministic merges.
//!
//! `dbwriter` is the only writer; backend, scheduler, endpoint and sourcesync
//! read these types. Every merge is commutative and idempotent, so concurrent
//! writers (and CouchDB `_conflicts` leaves) converge to the same document.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Datelike, Months, Utc};
use regent_sdk::hosts::handlers::ConnectionMethod;
use regent_sdk::hosts::managed_host::ManagedHostBuilder;
use regent_sdk::secrets::SecretReference;
use regent_sdk::state::ExpectedState;
use regent_sdk::state::compliance::ManagedHostStatus;
use regent_sdk::task::{Job, RegentTaskResult};
use serde::{Deserialize, Serialize};

use crate::messages::JobState;
use crate::schedule::Schedule;
use crate::selector::HostSelector;

pub const DB_SETTINGS: &str = "regency_settings";
pub const DB_ORDERS: &str = "regency_orders";
pub const DB_USERS: &str = "regency_users";
pub const DB_POLICIES: &str = "regency_policies";
pub const DB_CATALOG: &str = "regency_catalog";
pub const DB_STATE: &str = "regency_state";
/// Central service configuration (D21).
pub const DB_CONFIG: &str = "regency_config";
pub const JOBS_PREFIX: &str = "regency_jobs_";
pub const OUTCOMES_PREFIX: &str = "regency_outcomes_";

/// Databases that are never rotated.
pub const STATIC_DATABASES: [&str; 6] = [
    DB_SETTINGS,
    DB_ORDERS,
    DB_USERS,
    DB_POLICIES,
    DB_CATALOG,
    DB_STATE,
];

/// `YYYYMM` of a time (D8: from the job's `created_at`).
pub fn month_of(at: DateTime<Utc>) -> String {
    format!("{:04}{:02}", at.year(), at.month())
}

/// `YYYYMM` `offset` months away from `at` (negative: earlier).
pub fn month_offset(at: DateTime<Utc>, offset: i32) -> String {
    let shifted = if offset >= 0 {
        at.checked_add_months(Months::new(offset as u32))
    } else {
        at.checked_sub_months(Months::new(offset.unsigned_abs()))
    };
    month_of(shifted.unwrap_or(at))
}

pub fn jobs_db(month: &str) -> String {
    format!("{JOBS_PREFIX}{month}")
}

pub fn outcomes_db(month: &str) -> String {
    format!("{OUTCOMES_PREFIX}{month}")
}

/// A stored document: CouchDB metadata + body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stored<T> {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "_rev", default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(rename = "_conflicts", default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    #[serde(
        rename = "_deleted",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub deleted: bool,
    #[serde(flatten)]
    pub doc: T,
}

impl<T> Stored<T> {
    pub fn new(id: impl Into<String>, doc: T) -> Self {
        Self {
            id: id.into(),
            rev: None,
            conflicts: Vec::new(),
            deleted: false,
            doc,
        }
    }
}

// ---------------------------------------------------------------- jobs (D7)

pub fn job_doc_id(job_id: &str) -> String {
    format!("job:{job_id}")
}

/// Reference to an outcome document, with its month (D8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeRef {
    pub id: String,
    pub month: String,
    pub finished_at: DateTime<Utc>,
    /// Reset by retention (D8): the outcome no longer exists.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cleared: bool,
}

impl OutcomeRef {
    /// Newer `finished_at` wins; at equal times a cleared ref wins (a reset
    /// only ever applies to that same outcome); then the id decides.
    fn key(&self) -> (DateTime<Utc>, bool, &str) {
        (self.finished_at, self.cleared, &self.id)
    }
}

fn newest_ref(a: Option<OutcomeRef>, b: Option<OutcomeRef>) -> Option<OutcomeRef> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.key() > a.key() { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// `job:{key}` in `regency_jobs_YYYYMM`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobDoc {
    #[serde(rename = "type")]
    pub kind: String,
    pub job_id: String,
    pub host_id: String,
    pub job: Job,
    pub origin: String,
    pub created_at: DateTime<Utc>,
    /// Current state (highest rank; first terminal one).
    pub state: JobState,
    /// Earliest time each state was seen.
    pub states: BTreeMap<JobState, DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<OutcomeRef>,
    /// Compliance summary of the outcome (see [`HostState`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<HostState>,
    /// Details per state (who aborted, dead-letter reason…).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<JobState, String>,
}

impl JobDoc {
    pub fn from_event(event: &crate::messages::LifecycleEvent) -> Self {
        let mut doc = Self {
            kind: "job".to_string(),
            job_id: event.job_id.clone(),
            host_id: event.host_id.clone(),
            job: event.job,
            origin: event.origin.clone(),
            created_at: event.created_at,
            state: event.state,
            states: BTreeMap::from([(event.state, event.at)]),
            outcome: None,
            result: None,
            details: BTreeMap::new(),
        };
        if let Some(details) = &event.details {
            doc.details.insert(event.state, details.clone());
        }
        doc
    }

    /// Job doc carried by an outcome (`FINISHED`, or `ABORTED` for a
    /// cancelled run).
    pub fn from_outcome(
        result: &RegentTaskResult,
        created_at: DateTime<Utc>,
        origin: &str,
        outcome: OutcomeRef,
    ) -> Self {
        let state = if result
            .host_status()
            .is_some_and(ManagedHostStatus::is_aborted)
        {
            JobState::Aborted
        } else {
            JobState::Finished
        };
        let mut states = BTreeMap::from([(state, result.finished_at())]);
        states.insert(JobState::Started, result.started_at());
        Self {
            kind: "job".to_string(),
            job_id: result.idempotency_key().to_string(),
            host_id: result.host_id().to_string(),
            job: result.job(),
            origin: origin.to_string(),
            created_at,
            state,
            states,
            outcome: Some(outcome),
            result: Some(HostState::of_result(result)),
            details: BTreeMap::new(),
        }
    }

    /// Job doc from a stored outcome (used when the outcome already existed:
    /// the first recorded outcome wins, D4).
    pub fn from_outcome_doc(doc: &OutcomeDoc, outcome: OutcomeRef) -> Self {
        let state = if doc.state.compliance == "aborted" {
            JobState::Aborted
        } else {
            JobState::Finished
        };
        let mut states = BTreeMap::from([(state, doc.finished_at)]);
        states.insert(JobState::Started, doc.started_at);
        Self {
            kind: "job".to_string(),
            job_id: doc.job_id.clone(),
            host_id: doc.host_id.clone(),
            job: doc.job,
            origin: doc.origin.clone(),
            created_at: doc.created_at,
            state,
            states,
            outcome: Some(outcome),
            result: Some(doc.state.clone()),
            details: BTreeMap::new(),
        }
    }

    /// Commutative, idempotent merge (D7 forward-only rule).
    pub fn merge(&self, other: &Self) -> Self {
        let mut states = self.states.clone();
        for (state, at) in &other.states {
            states
                .entry(*state)
                .and_modify(|current| *current = (*current).min(*at))
                .or_insert(*at);
        }
        let mut details = self.details.clone();
        for (state, text) in &other.details {
            details
                .entry(*state)
                .and_modify(|current| {
                    if *text < *current {
                        *current = text.clone()
                    }
                })
                .or_insert_with(|| text.clone());
        }
        // highest rank wins; among terminal states, the earliest one recorded
        let state = states
            .iter()
            .max_by(|(a, a_at), (b, b_at)| {
                a.rank()
                    .cmp(&b.rank())
                    .then_with(|| {
                        if a.is_terminal() {
                            b_at.cmp(a_at)
                        } else {
                            std::cmp::Ordering::Equal
                        }
                    })
                    .then_with(|| b.as_str().cmp(a.as_str()))
            })
            .map(|(state, _)| *state)
            .unwrap_or(self.state);
        let outcome = newest_ref(self.outcome.clone(), other.outcome.clone());
        let result = match (&self.result, &other.result, &outcome) {
            (Some(a), _, Some(o)) if self.outcome.as_ref() == Some(o) => Some(a.clone()),
            (_, Some(b), Some(o)) if other.outcome.as_ref() == Some(o) => Some(b.clone()),
            (a, b, _) => a.clone().or_else(|| b.clone()),
        };
        // identity fields: the smaller value, for determinism (they are equal
        // unless a writer is buggy)
        Self {
            kind: "job".to_string(),
            job_id: self.job_id.clone(),
            host_id: self.host_id.clone().min(other.host_id.clone()),
            job: self.job,
            origin: self.origin.clone().min(other.origin.clone()),
            created_at: self.created_at.min(other.created_at),
            state,
            states,
            outcome,
            result,
            details,
        }
    }
}

// ---------------------------------------------------------------- outcomes

pub fn outcome_doc_id(job_id: &str) -> String {
    format!("outcome:{job_id}")
}

/// Name of the attachment holding the original protobuf bytes (D6, D8).
pub const OUTCOME_ATTACHMENT: &str = "result.pb";

/// Compliance summary of an outcome, shared by job, host and outcome docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostState {
    /// `compliant`, `not_compliant`, `reach_success`, `reach_failed`,
    /// `aborted`, `error` (the task failed), or `unknown` (no data within
    /// retention, D8).
    pub compliance: String,
    /// Remediations still needed (Assess) or actions taken (Reach), as their
    /// SDK JSON form.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl HostState {
    pub fn unknown() -> Self {
        Self {
            compliance: "unknown".to_string(),
            items: Vec::new(),
            error: None,
        }
    }

    pub fn of_result(result: &RegentTaskResult) -> Self {
        match result.outcome() {
            Err(error) => Self {
                compliance: "error".to_string(),
                items: Vec::new(),
                error: Some(error.to_string()),
            },
            Ok(status) => {
                let compliance = if status.is_aborted() {
                    "aborted"
                } else if status.is_already_compliant() {
                    "compliant"
                } else if status.is_not_compliant() {
                    "not_compliant"
                } else if status.is_reach_compliance_success() {
                    "reach_success"
                } else if status.is_reach_compliance_failed() {
                    "reach_failed"
                } else {
                    "unknown"
                };
                let items = if status.is_not_compliant() {
                    status
                        .all_remediations()
                        .iter()
                        .filter_map(|r| serde_json::to_value(r).ok())
                        .collect()
                } else {
                    status
                        .actions_taken()
                        .iter()
                        .filter_map(|(r, outcome)| {
                            Some(serde_json::json!({
                                "remediation": serde_json::to_value(r).ok()?,
                                "result": serde_json::to_value(outcome).ok()?,
                            }))
                        })
                        .collect()
                };
                Self {
                    compliance: compliance.to_string(),
                    items,
                    error: None,
                }
            }
        }
    }
}

/// `outcome:{key}` in `regency_outcomes_YYYYMM`: immutable, created once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeDoc {
    #[serde(rename = "type")]
    pub kind: String,
    pub job_id: String,
    pub host_id: String,
    pub job: Job,
    pub origin: String,
    pub publisher: String,
    pub created_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub state: HostState,
    /// Revision of the effective expected state (D23).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_revision: Option<String>,
    /// A one-shot raw attribute (D23): history only, never the host state.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub one_shot: bool,
    /// The decoded result (SDK JSON), for display. The attachment is the
    /// source of truth.
    pub result: serde_json::Value,
}

impl OutcomeDoc {
    pub fn from_result(
        result: &RegentTaskResult,
        created_at: DateTime<Utc>,
        origin: &str,
        publisher: &str,
    ) -> Self {
        Self {
            kind: "outcome".to_string(),
            job_id: result.idempotency_key().to_string(),
            host_id: result.host_id().to_string(),
            job: result.job(),
            origin: origin.to_string(),
            publisher: publisher.to_string(),
            created_at,
            started_at: result.started_at(),
            finished_at: result.finished_at(),
            state: HostState::of_result(result),
            state_revision: None,
            one_shot: false,
            result: serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
        }
    }
}

// ---------------------------------------------------------------- hosts

pub fn host_doc_id(host_id: &str) -> String {
    format!("host:{host_id}")
}

/// Client-certificate pinning of a self-signed host (D13).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertPins {
    /// Fingerprints seen and awaiting approval.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub pending: BTreeSet<String>,
    /// The approved fingerprint and when it was approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<(String, DateTime<Utc>)>,
}

impl CertPins {
    fn merge(&self, other: &Self) -> Self {
        let pinned = match (&self.pinned, &other.pinned) {
            (Some(a), Some(b)) => Some(if (b.1, &b.0) > (a.1, &a.0) {
                b.clone()
            } else {
                a.clone()
            }),
            (a, b) => a.clone().or_else(|| b.clone()),
        };
        let mut pending: BTreeSet<String> = self.pending.union(&other.pending).cloned().collect();
        if let Some((fingerprint, _)) = &pinned {
            pending.remove(fingerprint);
        }
        Self { pending, pinned }
    }
}

/// `host:{host_id}` in `regency_state`: existence + latest state (D8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostDoc {
    #[serde(rename = "type")]
    pub kind: String,
    pub host_id: String,
    pub first_seen: DateTime<Utc>,
    /// Newest `finished_at` seen.
    pub last_seen: DateTime<Utc>,
    /// Latest outcome summary; `unknown` when reset by retention.
    pub state: HostState,
    /// `finished_at` of the outcome the state comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_at: Option<DateTime<Utc>>,
    /// Reset by retention (wins over the same outcome at equal `state_at`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub state_cleared: bool,
    /// Revision of the effective expected state the state was checked
    /// against (D23); `None`: unknown (e.g. regops pulling from Git).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_assess: Option<OutcomeRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reach: Option<OutcomeRef>,
    #[serde(default)]
    pub certs: CertPins,
}

impl HostDoc {
    /// A host known only by its certificate (self-signed, pending).
    pub fn empty(host_id: &str, now: DateTime<Utc>) -> Self {
        Self {
            kind: "host".to_string(),
            host_id: host_id.to_string(),
            first_seen: now,
            last_seen: now,
            state: HostState::unknown(),
            state_at: None,
            state_cleared: false,
            state_revision: None,
            last_assess: None,
            last_reach: None,
            certs: CertPins::default(),
        }
    }

    pub fn from_outcome(
        result: &RegentTaskResult,
        outcome: OutcomeRef,
        state_revision: Option<String>,
    ) -> Self {
        let mut doc = Self::empty(result.host_id(), result.finished_at());
        doc.state = HostState::of_result(result);
        doc.state_at = Some(result.finished_at());
        doc.state_revision = state_revision;
        match result.job() {
            Job::Assess => doc.last_assess = Some(outcome),
            Job::Reach => doc.last_reach = Some(outcome),
        }
        doc
    }

    /// Host doc from a stored outcome.
    pub fn from_outcome_doc(doc: &OutcomeDoc, outcome: OutcomeRef) -> Self {
        let mut host = Self::empty(&doc.host_id, doc.finished_at);
        host.state = doc.state.clone();
        host.state_at = Some(doc.finished_at);
        host.state_revision = doc.state_revision.clone();
        match doc.job {
            Job::Assess => host.last_assess = Some(outcome),
            Job::Reach => host.last_reach = Some(outcome),
        }
        host
    }

    /// Commutative, idempotent merge (newest `finished_at` wins).
    pub fn merge(&self, other: &Self) -> Self {
        let a_key = (self.state_at, self.state_cleared);
        let b_key = (other.state_at, other.state_cleared);
        let tie = |d: &Self| serde_json::to_string(&(&d.state, &d.state_revision)).ok();
        let (state, state_at, state_cleared, state_revision) =
            if b_key > a_key || (b_key == a_key && tie(other) > tie(self)) {
                (
                    other.state.clone(),
                    other.state_at,
                    other.state_cleared,
                    other.state_revision.clone(),
                )
            } else {
                (
                    self.state.clone(),
                    self.state_at,
                    self.state_cleared,
                    self.state_revision.clone(),
                )
            };
        Self {
            kind: "host".to_string(),
            host_id: self.host_id.clone(),
            first_seen: self.first_seen.min(other.first_seen),
            last_seen: self.last_seen.max(other.last_seen),
            state,
            state_at,
            state_cleared,
            state_revision,
            last_assess: newest_ref(self.last_assess.clone(), other.last_assess.clone()),
            last_reach: newest_ref(self.last_reach.clone(), other.last_reach.clone()),
            certs: self.certs.merge(&other.certs),
        }
    }

    /// Retention reset (D8): clear what refers to `month`; returns whether
    /// anything changed.
    pub fn clear_month(&mut self, month: &str) -> bool {
        let mut changed = false;
        for reference in [&mut self.last_assess, &mut self.last_reach]
            .into_iter()
            .flatten()
        {
            if reference.month == month && !reference.cleared {
                reference.cleared = true;
                changed = true;
            }
        }
        let state_in_month = [&self.last_assess, &self.last_reach]
            .into_iter()
            .flatten()
            .any(|r| r.month == month && Some(r.finished_at) == self.state_at);
        if state_in_month && !self.state_cleared {
            self.state = HostState::unknown();
            self.state_cleared = true;
            self.state_revision = None;
            changed = true;
        }
        changed
    }
}

// ---------------------------------------------------------------- settings

/// The single settings document of `regency_settings`.
pub const SETTINGS_DOC_ID: &str = "settings:global";
pub const DEFAULT_RETENTION_MONTHS: i64 = 3;

/// Settings: free-form keys set by `settings.set` (D8, D13).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsDoc {
    #[serde(rename = "type", default = "settings_kind")]
    pub kind: String,
    #[serde(default)]
    pub values: BTreeMap<String, serde_json::Value>,
}

fn settings_kind() -> String {
    "settings".to_string()
}

impl SettingsDoc {
    pub fn with_defaults() -> Self {
        Self {
            kind: settings_kind(),
            values: BTreeMap::from([(
                "retention_months".to_string(),
                serde_json::Value::from(DEFAULT_RETENTION_MONTHS),
            )]),
        }
    }

    /// `None` if absent or not an integer (D8 guard: no drop then).
    pub fn retention_months(&self) -> Option<i64> {
        self.values
            .get("retention_months")
            .and_then(serde_json::Value::as_i64)
    }

    fn list(&self, key: &str) -> BTreeSet<String> {
        self.values
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// D13 deny list.
    pub fn revoked_hosts(&self) -> BTreeSet<String> {
        self.list("revoked_hosts")
    }
    pub fn revoked_serials(&self) -> BTreeSet<String> {
        self.list("revoked_serials")
    }
    pub fn revoked_fingerprints(&self) -> BTreeSet<String> {
        self.list("revoked_fingerprints")
    }
}

// ---------------------------------------------------------------- orders (D16)

pub fn order_doc_id(order_id: &str) -> String {
    format!("order:{order_id}")
}

/// A recurring order in `regency_orders`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub order_id: String,
    pub schedule: Schedule,
    /// `{source_id}/{name}` of an assignment (D23): its hosts, each with
    /// its effective expected state.
    pub assignment: String,
    pub job: Job,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// Principal re-checked at fire time (D12).
    pub author: String,
    /// The author's groups when the order was written (the scheduler does not
    /// read accounts; group changes apply at the next upsert).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub author_groups: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

impl Order {
    /// Checks shared by backend (before publishing) and dbwriter (before
    /// writing).
    pub fn validate(&self) -> Result<(), String> {
        if self.order_id.is_empty() || self.order_id.contains(['/', ':']) {
            return Err("order_id must be non-empty, without '/' or ':'".to_string());
        }
        self.schedule.validate().map_err(|e| e.to_string())?;
        if split_ref(&self.assignment).is_none() {
            return Err("assignment must be <source_id>/<name>".to_string());
        }
        if self.author.is_empty() {
            return Err("author is required".to_string());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- catalog (D11)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Git,
    Api,
    Frontend,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceDef {
    pub source_id: String,
    pub kind: SourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Directory inside the repository holding `inventories/` and
    /// `expected_states/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default = "default_poll")]
    pub poll_interval_sec: u64,
    /// Git credentials (D9: a reference only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<SecretReference>,
}

fn default_poll() -> u64 {
    300
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryDef {
    pub source_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_connection_method: Option<ConnectionMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_vars: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostDef {
    pub source_id: String,
    pub host_id: String,
    pub builder: ManagedHostBuilder,
    /// Names of this source's inventories the host belongs to.
    #[serde(default)]
    pub inventories: BTreeSet<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedStateDef {
    pub source_id: String,
    pub name: String,
    pub state: ExpectedState,
}

/// Association of hosts and an expected state (D23). Every assignment
/// selecting a host is one layer of its effective expected state, lowest
/// `priority` first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssignmentDef {
    pub source_id: String,
    pub name: String,
    pub selector: HostSelector,
    /// `{source_id}/{name}` of an expected state.
    pub expected_state: String,
    #[serde(default)]
    pub priority: i32,
}

impl AssignmentDef {
    /// `{source_id}/{name}`.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.source_id, self.name)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() || self.name.contains(['/', ':']) {
            return Err("assignment name must be non-empty, without '/' or ':'".to_string());
        }
        if self.selector.is_empty() {
            return Err("the host selector is empty".to_string());
        }
        if split_ref(&self.expected_state).is_none() {
            return Err("expected_state must be <source_id>/<name>".to_string());
        }
        Ok(())
    }
}

/// A catalog item (one CouchDB doc each).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CatalogBody {
    Source(SourceDef),
    Inventory(InventoryDef),
    Hostdef(HostDef),
    ExpectedState(ExpectedStateDef),
    Assignment(AssignmentDef),
}

/// A catalog item with its source revision (commit sha, content hash).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogItem {
    #[serde(flatten)]
    pub body: CatalogBody,
    pub revision: String,
}

impl CatalogItem {
    pub fn source_id(&self) -> &str {
        match &self.body {
            CatalogBody::Source(d) => &d.source_id,
            CatalogBody::Inventory(d) => &d.source_id,
            CatalogBody::Hostdef(d) => &d.source_id,
            CatalogBody::ExpectedState(d) => &d.source_id,
            CatalogBody::Assignment(d) => &d.source_id,
        }
    }

    pub fn doc_id(&self) -> String {
        match &self.body {
            CatalogBody::Source(d) => format!("source:{}", d.source_id),
            CatalogBody::Inventory(d) => format!("inventory:{}:{}", d.source_id, d.name),
            CatalogBody::Hostdef(d) => format!("hostdef:{}:{}", d.source_id, d.host_id),
            CatalogBody::ExpectedState(d) => format!("expected_state:{}:{}", d.source_id, d.name),
            CatalogBody::Assignment(d) => format!("assignment:{}:{}", d.source_id, d.name),
        }
    }
}

/// Splits `{source_id}/{name}`.
pub fn split_ref(reference: &str) -> Option<(&str, &str)> {
    reference
        .split_once('/')
        .filter(|(s, n)| !s.is_empty() && !n.is_empty())
}

pub fn expected_state_doc_id(reference: &str) -> Option<String> {
    split_ref(reference).map(|(source, name)| format!("expected_state:{source}:{name}"))
}

pub fn assignment_doc_id(reference: &str) -> Option<String> {
    split_ref(reference).map(|(source, name)| format!("assignment:{source}:{name}"))
}

// ---------------------------------------------------------------- users, policies (D12)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    User,
    ServiceAccount,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenHash {
    pub token_id: String,
    /// argon2id PHC string.
    pub hash: String,
}

/// A local account in `regency_users` (`user:{name}`); hashes only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserDoc {
    pub name: String,
    pub account: AccountKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tokens: Vec<TokenHash>,
    #[serde(default)]
    pub groups: BTreeSet<String>,
    #[serde(default)]
    pub disabled: bool,
    /// The bootstrap admin: can be disabled, not deleted (D12).
    #[serde(default)]
    pub bootstrap: bool,
}

pub fn user_doc_id(name: &str) -> String {
    format!("user:{name}")
}

/// A Cedar policy in `regency_policies` (`policy:{id}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDoc {
    pub policy_id: String,
    pub text: String,
    #[serde(default)]
    pub description: String,
}

pub fn policy_doc_id(policy_id: &str) -> String {
    format!("policy:{policy_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::LifecycleEvent;
    use chrono::TimeZone;

    fn at(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + s, 0).unwrap()
    }

    fn event(state: JobState, t: i64) -> JobDoc {
        let mut e = LifecycleEvent::new("k", state, at(0), "o", "h", Job::Reach);
        e.at = at(t);
        JobDoc::from_event(&e)
    }

    #[test]
    fn job_merge_is_commutative_and_forward_only() {
        let docs = [
            event(JobState::Pending, 0),
            event(JobState::Started, 1),
            event(JobState::AbortRequired, 2),
            event(JobState::Aborted, 4),
            event(JobState::Finished, 3),
        ];
        let forward = docs
            .iter()
            .skip(1)
            .fold(docs[0].clone(), |acc, d| acc.merge(d));
        let backward = docs
            .iter()
            .rev()
            .skip(1)
            .fold(docs[4].clone(), |acc, d| acc.merge(d));
        assert_eq!(forward, backward);
        // first terminal state recorded wins
        assert_eq!(forward.state, JobState::Finished);
        assert_eq!(forward.merge(&forward), forward);
        // a late ABORT_REQUIRED never moves a terminal job back
        assert_eq!(
            forward.merge(&event(JobState::AbortRequired, 9)).state,
            JobState::Finished
        );
    }

    #[test]
    fn host_merge_keeps_newest_state_and_retention_reset() {
        let mk = |t: i64, compliance: &str| {
            let mut doc = HostDoc::empty("h", at(t));
            doc.state = HostState {
                compliance: compliance.to_string(),
                items: vec![],
                error: None,
            };
            doc.state_at = Some(at(t));
            doc.last_assess = Some(OutcomeRef {
                id: format!("outcome:{t}"),
                month: month_of(at(t)),
                finished_at: at(t),
                cleared: false,
            });
            doc
        };
        let old = mk(0, "not_compliant");
        let new = mk(10, "compliant");
        assert_eq!(old.merge(&new), new.merge(&old));
        assert_eq!(old.merge(&new).state.compliance, "compliant");
        assert_eq!(old.merge(&new).first_seen, at(0));

        let mut reset = new.clone();
        assert!(reset.clear_month(&month_of(at(10))));
        assert_eq!(reset.state.compliance, "unknown");
        // the reset wins over the very outcome it cleared, in any order
        assert_eq!(reset.merge(&new), new.merge(&reset));
        assert_eq!(new.merge(&reset).state.compliance, "unknown");
        // but a newer outcome always wins
        assert_eq!(
            reset.merge(&mk(20, "compliant")).state.compliance,
            "compliant"
        );
    }

    #[test]
    fn months() {
        let nov = Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap();
        assert_eq!(month_of(nov), "202611");
        assert_eq!(month_offset(nov, -3), "202608");
        assert_eq!(month_offset(nov, 2), "202701");
        assert_eq!(jobs_db("202611"), "regency_jobs_202611");
    }

    #[test]
    fn catalog_item_json_is_flat() {
        let item = CatalogItem {
            body: CatalogBody::ExpectedState(ExpectedStateDef {
                source_id: "git1".into(),
                name: "web".into(),
                state: ExpectedState::new(),
            }),
            revision: "abc".into(),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["type"], "expected_state");
        assert_eq!(json["revision"], "abc");
        assert_eq!(item.doc_id(), "expected_state:git1:web");
        let stored: Stored<CatalogItem> =
            serde_json::from_value(serde_json::json!({"_id": "x", "_rev": "1-a", "type": "expected_state",
                "source_id": "git1", "name": "web", "state": {"Attributes": []}, "revision": "abc"}))
            .unwrap();
        assert_eq!(stored.doc.source_id(), "git1");
    }
}
