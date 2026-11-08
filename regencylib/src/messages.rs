//! Message formats on RabbitMQ (D2, D6, D7, D10).
//!
//! | Exchange / stream | Format | Type |
//! |---|---|---|
//! | `regency.tasks` | JSON | [`TaskEnvelope`] (tasks stay JSON, D6) |
//! | `regency.outcomes` | protobuf | [`crate::proto::OutcomeEnvelope`] |
//! | `regency.lifecycle` | JSON | [`LifecycleEvent`] |
//! | `regency.commands` | JSON | [`Command`] |
//! | stream `regency.control` | JSON | [`AbortRequest`] |
//!
//! Every message carries `schema_version`; a reader refuses a higher major
//! version than it knows (it is dead-lettered and the services are upgraded).

use chrono::{DateTime, TimeZone, Utc};
use protobuf::{Parse, Serialize as _};
use regent_sdk::task::{Job, RegentTask, RegentTaskResult};
use serde::{Deserialize, Serialize};

use crate::model::{CatalogItem, Order, PolicyDoc, UserDoc};
use crate::selector::HostSelector;

/// Current version of every regency message layout.
pub const SCHEMA_VERSION: u32 = 1;

/// Message (de)serialization error.
#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    #[error("invalid JSON message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid protobuf message: {0}")]
    Proto(String),
    #[error("unsupported schema_version {0} (this build knows {SCHEMA_VERSION})")]
    Version(u32),
    #[error("task serialized by regent-sdk {0}, this build has {sdk}", sdk = regent_sdk::VERSION)]
    Sdk(String),
    #[error("task header does not match its task")]
    Header,
}

fn check_version(version: u32) -> Result<(), MessageError> {
    if version == 0 || version > SCHEMA_VERSION {
        Err(MessageError::Version(version))
    } else {
        Ok(())
    }
}

/// `origin` of regops passes (D10).
pub const ORIGIN_REGOPS: &str = "regops";

/// What every reader of a task can decode without the SDK (D20): the job's
/// identity and the regent-sdk version that serialized the task. Readers with
/// another SDK (independent releases) use it to record the job instead of
/// misreading the task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskHeader {
    pub schema_version: u32,
    /// `regent_sdk::VERSION` of the producer: the task's serde layout.
    pub sdk_version: String,
    /// The task's idempotency key (= job id, D7).
    pub job_id: String,
    pub host_id: String,
    pub job: Job,
    /// Job creation time (D8): scheduler's time for `run`, slot start for
    /// recurring orders.
    pub created_at: DateTime<Utc>,
    /// Command id or order id (D10).
    pub origin: String,
    /// Revision of the host's effective expected state the task runs (D23).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_revision: Option<String>,
    /// A one-shot raw attribute (D23): its outcome never sets the host state.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub one_shot: bool,
}

impl TaskHeader {
    /// Decode only the header of a task message (any SDK version).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MessageError> {
        let header: Self = serde_json::from_slice(bytes)?;
        check_version(header.schema_version)?;
        Ok(header)
    }

    /// Whether this build can read the task (same SDK serde layout).
    pub fn same_sdk(&self) -> bool {
        self.sdk_version == regent_sdk::VERSION
    }
}

/// A task on `regency.tasks` (JSON): [`TaskHeader`] fields plus the task.
#[derive(Clone, Serialize, Deserialize)]
pub struct TaskEnvelope {
    #[serde(flatten)]
    pub header: TaskHeader,
    pub task: RegentTask,
}

impl TaskEnvelope {
    pub fn new(task: RegentTask, created_at: DateTime<Utc>, origin: impl Into<String>) -> Self {
        Self {
            header: TaskHeader {
                schema_version: SCHEMA_VERSION,
                sdk_version: regent_sdk::VERSION.to_string(),
                job_id: task.idempotency_key().to_string(),
                host_id: task.host_id().to_string(),
                job: task.job(),
                created_at,
                origin: origin.into(),
                state_revision: None,
                one_shot: false,
            },
            task,
        }
    }

    /// What the task checks (D23): the host's effective state at `revision`,
    /// or a one-shot attribute (`None`).
    pub fn checking(mut self, revision: Option<String>) -> Self {
        self.header.one_shot = revision.is_none();
        self.header.state_revision = revision;
        self
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, MessageError> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Decode a task; refuses one serialized by another regent-sdk version
    /// ([`MessageError::Sdk`]: decode its [`TaskHeader`] instead).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MessageError> {
        let header = TaskHeader::from_bytes(bytes)?;
        if !header.same_sdk() {
            return Err(MessageError::Sdk(header.sdk_version));
        }
        let envelope: Self = serde_json::from_slice(bytes)?;
        if envelope.header.job_id != envelope.task.idempotency_key()
            || envelope.header.host_id != envelope.task.host_id()
            || envelope.header.job != envelope.task.job()
        {
            return Err(MessageError::Header);
        }
        Ok(envelope)
    }
}

/// Routing key of an outcome on `regency.outcomes` (D4).
pub fn outcome_routing_key(job: Job) -> &'static str {
    match job {
        Job::Assess => "assess",
        Job::Reach => "reach",
    }
}

/// Build an outcome envelope from the SDK's protobuf bytes of a result.
pub fn outcome_envelope(
    result_bytes: Vec<u8>,
    reporter_bytes: Vec<u8>,
    created_at: DateTime<Utc>,
    origin: &str,
    publisher: &str,
) -> crate::proto::OutcomeEnvelope {
    let mut envelope = crate::proto::OutcomeEnvelope::new();
    envelope.set_schema_version(SCHEMA_VERSION);
    envelope.set_created_at_ms(created_at.timestamp_millis());
    envelope.set_origin(origin);
    envelope.set_result(result_bytes);
    envelope.set_reporter(reporter_bytes);
    envelope.set_publisher(publisher);
    envelope
}

/// Copy what a task checked (D23) into its outcome envelope.
pub fn set_checked(envelope: &mut crate::proto::OutcomeEnvelope, header: &TaskHeader) {
    if let Some(revision) = &header.state_revision {
        envelope.set_state_revision(revision.as_str());
    }
    envelope.set_one_shot(header.one_shot);
}

pub fn encode_outcome(envelope: &crate::proto::OutcomeEnvelope) -> Result<Vec<u8>, MessageError> {
    envelope
        .serialize()
        .map_err(|e| MessageError::Proto(format!("{e:?}")))
}

/// A decoded outcome: the envelope fields, the SDK result as Rust types, and
/// the original result bytes (kept as the CouchDB attachment, D8).
pub struct DecodedOutcome {
    pub created_at: DateTime<Utc>,
    pub origin: String,
    pub publisher: String,
    pub result: RegentTaskResult,
    pub result_bytes: Vec<u8>,
    pub reporter_bytes: Vec<u8>,
    /// Revision of the effective expected state checked (D23), if known.
    pub state_revision: Option<String>,
    pub one_shot: bool,
}

pub fn decode_outcome(bytes: &[u8]) -> Result<DecodedOutcome, MessageError> {
    let envelope = crate::proto::OutcomeEnvelope::parse(bytes)
        .map_err(|e| MessageError::Proto(format!("{e:?}")))?;
    check_version(envelope.schema_version())?;
    let result_bytes = envelope.result().to_vec();
    let result = RegentTaskResult::from_proto_bytes(&result_bytes)
        .map_err(|e| MessageError::Proto(e.to_string()))?;
    Ok(DecodedOutcome {
        created_at: millis_to_datetime(envelope.created_at_ms()),
        origin: envelope.origin().to_string(),
        publisher: envelope.publisher().to_string(),
        result,
        result_bytes,
        reporter_bytes: envelope.reporter().to_vec(),
        state_revision: Some(envelope.state_revision().to_string()).filter(|r| !r.is_empty()),
        one_shot: envelope.one_shot(),
    })
}

pub fn millis_to_datetime(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
}

/// Job lifecycle state (D7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JobState {
    Pending,
    Started,
    AbortRequired,
    Finished,
    Aborted,
    Expired,
    Failed,
}

impl JobState {
    /// Forward-only rank (D7).
    pub fn rank(self) -> u8 {
        match self {
            JobState::Pending => 0,
            JobState::Started => 1,
            JobState::AbortRequired => 2,
            JobState::Finished | JobState::Aborted | JobState::Expired | JobState::Failed => 3,
        }
    }

    pub fn is_terminal(self) -> bool {
        self.rank() == 3
    }

    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Pending => "PENDING",
            JobState::Started => "STARTED",
            JobState::AbortRequired => "ABORT_REQUIRED",
            JobState::Finished => "FINISHED",
            JobState::Aborted => "ABORTED",
            JobState::Expired => "EXPIRED",
            JobState::Failed => "FAILED",
        }
    }
}

/// An event on `regency.lifecycle` (JSON), routing key = state in lowercase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleEvent {
    pub schema_version: u32,
    /// Job id = the task's idempotency key (D7).
    pub job_id: String,
    pub state: JobState,
    /// When the event happened.
    pub at: DateTime<Utc>,
    /// Copied from the job (D8).
    pub created_at: DateTime<Utc>,
    pub origin: String,
    pub host_id: String,
    pub job: Job,
    /// Free text: who aborted, dead-letter reason, runner instance…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl LifecycleEvent {
    pub fn new(
        envelope_job_id: &str,
        state: JobState,
        created_at: DateTime<Utc>,
        origin: &str,
        host_id: &str,
        job: Job,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            job_id: envelope_job_id.to_string(),
            state,
            at: Utc::now(),
            created_at,
            origin: origin.to_string(),
            host_id: host_id.to_string(),
            job,
            details: None,
        }
    }

    /// The event for a queued task.
    pub fn for_task(header: &TaskHeader, state: JobState) -> Self {
        Self::new(
            &header.job_id,
            state,
            header.created_at,
            &header.origin,
            &header.host_id,
            header.job,
        )
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    pub fn routing_key(&self) -> String {
        self.state.as_str().to_ascii_lowercase()
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, MessageError> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MessageError> {
        let event: Self = serde_json::from_slice(bytes)?;
        check_version(event.schema_version)?;
        Ok(event)
    }
}

/// An abort request on the stream `regency.control` (JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbortRequest {
    pub schema_version: u32,
    pub job_id: String,
    pub requested_at: DateTime<Utc>,
    pub requested_by: String,
}

impl AbortRequest {
    pub fn new(job_id: &str, requested_by: &str) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            job_id: job_id.to_string(),
            requested_at: Utc::now(),
            requested_by: requested_by.to_string(),
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, MessageError> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MessageError> {
        let request: Self = serde_json::from_slice(bytes)?;
        check_version(request.schema_version)?;
        Ok(request)
    }
}

/// `run` command body (D10, D16).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCommand {
    pub selector: HostSelector,
    /// `None`: every selected host runs its effective expected state (D23).
    /// `Some`: a one-shot raw state (hand-written attributes), run as is;
    /// its outcomes are history only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_shot: Option<regent_sdk::ExpectedState>,
    pub job: Job,
    /// Runner pool override (D16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
}

/// Command bodies; the serde tag is the routing key on `regency.commands`.
#[allow(clippy::large_enum_variant)] // short-lived, serialized at once
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "body")]
pub enum CommandBody {
    #[serde(rename = "run")]
    Run(RunCommand),
    #[serde(rename = "order.upsert")]
    OrderUpsert(Order),
    #[serde(rename = "order.delete")]
    OrderDelete { order_id: String },
    /// Sets one key of a settings document (D8), e.g. `retention_months`.
    #[serde(rename = "settings.set")]
    SettingsSet {
        key: String,
        value: serde_json::Value,
    },
    #[serde(rename = "host.delete")]
    HostDelete { host_id: String },
    /// A self-signed client certificate seen for the first time (D13).
    #[serde(rename = "host.cert_pending")]
    HostCertPending {
        host_id: String,
        fingerprint: String,
        /// Trust-on-first-use: pin at once.
        auto_approve: bool,
    },
    #[serde(rename = "host.approve_cert")]
    HostApproveCert {
        host_id: String,
        fingerprint: String,
    },
    #[serde(rename = "catalog.upsert")]
    CatalogUpsert(CatalogItem),
    #[serde(rename = "catalog.delete")]
    CatalogDelete {
        doc_id: String,
        /// Source of the deleting party; Git items can only be deleted by
        /// their sourcesync (D11).
        source_id: String,
    },
    #[serde(rename = "source.refresh")]
    SourceRefresh { source_id: String },
    #[serde(rename = "user.upsert")]
    UserUpsert(UserDoc),
    #[serde(rename = "user.delete")]
    UserDelete { name: String },
    #[serde(rename = "policy.upsert")]
    PolicyUpsert(PolicyDoc),
    #[serde(rename = "policy.delete")]
    PolicyDelete { policy_id: String },
    /// Replaces a central configuration document (D21); empty `settings`
    /// deletes it.
    #[serde(rename = "config.set")]
    ConfigSet {
        service: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pool: Option<String>,
        settings: serde_json::Map<String, serde_json::Value>,
    },
}

impl CommandBody {
    /// Routing key on `regency.commands` (D10).
    pub fn kind(&self) -> &'static str {
        match self {
            CommandBody::Run(_) => "run",
            CommandBody::OrderUpsert(_) => "order.upsert",
            CommandBody::OrderDelete { .. } => "order.delete",
            CommandBody::SettingsSet { .. } => "settings.set",
            CommandBody::HostDelete { .. } => "host.delete",
            CommandBody::HostCertPending { .. } => "host.cert_pending",
            CommandBody::HostApproveCert { .. } => "host.approve_cert",
            CommandBody::CatalogUpsert(_) => "catalog.upsert",
            CommandBody::CatalogDelete { .. } => "catalog.delete",
            CommandBody::SourceRefresh { .. } => "source.refresh",
            CommandBody::UserUpsert(_) => "user.upsert",
            CommandBody::UserDelete { .. } => "user.delete",
            CommandBody::PolicyUpsert(_) => "policy.upsert",
            CommandBody::PolicyDelete { .. } => "policy.delete",
            CommandBody::ConfigSet { .. } => "config.set",
        }
    }
}

/// A command on `regency.commands` (JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    pub schema_version: u32,
    /// Chosen by the client, reused on retry (D10).
    pub command_id: String,
    pub issued_at: DateTime<Utc>,
    /// Principal that issued it (`User::"alice"`, `service:sourcesync`…).
    pub issued_by: String,
    #[serde(flatten)]
    pub body: CommandBody,
}

impl Command {
    pub fn new(
        command_id: impl Into<String>,
        issued_by: impl Into<String>,
        body: CommandBody,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            command_id: command_id.into(),
            issued_at: Utc::now(),
            issued_by: issued_by.into(),
            body,
        }
    }

    pub fn routing_key(&self) -> &'static str {
        self.body.kind()
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, MessageError> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MessageError> {
        let command: Self = serde_json::from_slice(bytes)?;
        check_version(command.schema_version)?;
        Ok(command)
    }
}

/// Idempotency key of a `run` task (D10).
pub fn run_key(command_id: &str, host_id: &str) -> String {
    format!("run:{command_id}:{host_id}")
}

/// Idempotency key of a recurring task (D3, D16): `slot` is the interval slot
/// number or the cron fire time (Unix seconds). One key per host, since an
/// order targets several hosts.
pub fn recurring_key(order_id: &str, rev: &str, slot: i64, host_id: &str) -> String {
    format!("rec:{order_id}:{rev}:{slot}:{host_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use regent_sdk::ExpectedState;
    use regent_sdk::hosts::managed_host::ManagedHostBuilder;
    use regent_sdk::state::compliance::ManagedHostStatus;

    #[test]
    fn job_state_ranks_are_forward_only() {
        assert!(JobState::Pending.rank() < JobState::Started.rank());
        assert!(JobState::Started.rank() < JobState::AbortRequired.rank());
        assert!(JobState::AbortRequired.rank() < JobState::Finished.rank());
        assert!(JobState::Expired.is_terminal());
        assert_eq!(
            serde_json::to_string(&JobState::AbortRequired).unwrap(),
            "\"ABORT_REQUIRED\""
        );
    }

    #[test]
    fn command_json_has_kind_and_body() {
        let command = Command::new(
            "c1",
            "User::\"alice\"",
            CommandBody::HostDelete {
                host_id: "web-01".to_string(),
            },
        );
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["kind"], "host.delete");
        assert_eq!(json["body"]["host_id"], "web-01");
        let back = Command::from_bytes(&command.to_bytes().unwrap()).unwrap();
        assert_eq!(back.routing_key(), "host.delete");
    }

    #[test]
    fn task_envelope_roundtrips() {
        let builder = ManagedHostBuilder::new("web-01", "10.0.0.1:22", None);
        let task = RegentTask::from(builder, ExpectedState::new(), Job::Assess)
            .with_idempotency_key("run:c1:web-01");
        let envelope = TaskEnvelope::new(task, Utc::now(), "c1");
        let back = TaskEnvelope::from_bytes(&envelope.to_bytes().unwrap()).unwrap();
        assert_eq!(back.task.idempotency_key(), "run:c1:web-01");
        assert_eq!(back.header.origin, "c1");
        assert_eq!(back.header.job_id, "run:c1:web-01");
        assert_eq!(back.header.sdk_version, regent_sdk::VERSION);
    }

    #[test]
    fn task_from_another_sdk_is_refused_but_its_header_reads() {
        let builder = ManagedHostBuilder::new("web-01", "10.0.0.1:22", None);
        let task = RegentTask::from(builder, ExpectedState::new(), Job::Reach)
            .with_idempotency_key("run:c1:web-01");
        let mut json = serde_json::to_value(TaskEnvelope::new(task, Utc::now(), "c1")).unwrap();
        json["sdk_version"] = "0.0.1".into();
        json["task"] = serde_json::json!({"layout": "unknown to this SDK"});
        let bytes = serde_json::to_vec(&json).unwrap();
        assert!(matches!(
            TaskEnvelope::from_bytes(&bytes),
            Err(MessageError::Sdk(v)) if v == "0.0.1"
        ));
        let header = TaskHeader::from_bytes(&bytes).unwrap();
        assert_eq!(header.job_id, "run:c1:web-01");
        assert_eq!(header.host_id, "web-01");
        assert_eq!(header.job, Job::Reach);
    }

    #[test]
    fn task_header_must_match_its_task() {
        let builder = ManagedHostBuilder::new("web-01", "10.0.0.1:22", None);
        let task = RegentTask::from(builder, ExpectedState::new(), Job::Assess);
        let mut json = serde_json::to_value(TaskEnvelope::new(task, Utc::now(), "c1")).unwrap();
        json["host_id"] = "db-01".into();
        let bytes = serde_json::to_vec(&json).unwrap();
        assert!(matches!(
            TaskEnvelope::from_bytes(&bytes),
            Err(MessageError::Header)
        ));
    }

    #[test]
    fn outcome_envelope_roundtrips_with_original_bytes() {
        let now = Utc::now();
        let result = RegentTaskResult::from(
            "k".to_string(),
            "web-01".to_string(),
            Job::Assess,
            now,
            now,
            Ok(ManagedHostStatus::already_compliant()),
        );
        let mut bytes = result.to_proto_bytes().unwrap();
        // an unknown field from a newer regent-sdk must survive
        bytes.extend_from_slice(&[0xba, 0x3e, 3, b'n', b'e', b'w']);
        let envelope = outcome_envelope(bytes.clone(), Vec::new(), now, "regops", "endpoint/a");
        let decoded = decode_outcome(&encode_outcome(&envelope).unwrap()).unwrap();
        assert_eq!(decoded.result_bytes, bytes);
        assert_eq!(decoded.result.host_id(), "web-01");
        assert_eq!(decoded.origin, "regops");
        assert_eq!(
            decoded.created_at.timestamp_millis(),
            now.timestamp_millis()
        );
    }

    #[test]
    fn newer_schema_is_refused() {
        let raw = br#"{"schema_version":99,"job_id":"k","requested_at":"2026-01-01T00:00:00Z","requested_by":"x"}"#;
        assert!(matches!(
            AbortRequest::from_bytes(raw),
            Err(MessageError::Version(99))
        ));
    }
}
