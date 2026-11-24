//! Model ↔ API messages.

use regencylib::assignment::Effective;
use regencylib::model::{HostDoc, JobDoc, Order, OutcomeDoc};
use regencylib::proto as api;
use regencylib::schedule::Schedule;
use regencylib::selector::HostSelector;
use regent_sdk::state::ExpectedState;
use regent_sdk::state::attribute::Attribute;
use regent_sdk::task::Job;
use tonic::Status;

pub fn selector_from(message: api::HostSelectorView<'_>) -> HostSelector {
    HostSelector {
        hosts: message.hosts().iter().map(|h| h.to_string()).collect(),
        assignments: message
            .assignments()
            .iter()
            .map(|i| i.to_string())
            .collect(),
        labels: message
            .labels()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        selector: None,
    }
}

/// [`selector_from`], with its label selector (refused when unreadable).
pub fn selector_checked(message: api::HostSelectorView<'_>) -> Result<HostSelector, Status> {
    let mut selector = selector_from(message);
    let text = message.selector().to_string();
    if !text.trim().is_empty() {
        selector.selector = Some(
            regencylib::labels::LabelSelector::parse(&text)
                .map_err(|e| Status::invalid_argument(format!("selector: {e}")))?,
        );
    }
    Ok(selector)
}

pub fn job_from(kind: api::JobKind) -> Result<Job, Status> {
    match kind {
        api::JobKind::Assess => Ok(Job::Assess),
        api::JobKind::Reach => Ok(Job::Reach),
        _ => Err(Status::invalid_argument("job must be ASSESS or REACH")),
    }
}

pub fn job_to(job: Job) -> api::JobKind {
    match job {
        Job::Assess => api::JobKind::Assess,
        Job::Reach => api::JobKind::Reach,
    }
}

fn ms(at: chrono::DateTime<chrono::Utc>) -> i64 {
    at.timestamp_millis()
}

/// What the catalog says about a host (D11, D23).
pub struct HostMeta {
    pub labels: Vec<(String, String)>,
    pub conflicting: bool,
    pub effective: Effective,
    /// D30: the current revision of each assignment selecting the host.
    pub revisions: Vec<(String, Option<String>)>,
    /// D28: a proactive host's repository freshness and drift.
    pub agent: Option<AgentMeta>,
}

/// D28: how a proactive host's state source compares with regency's.
pub struct AgentMeta {
    /// `up_to_date`, `behind` or `unknown`
    pub repo_status: &'static str,
    pub repo_head: String,
    pub repo_source: String,
    pub drift: Option<String>,
}

pub fn host_summary(doc: &HostDoc, meta: &HostMeta) -> api::HostSummary {
    let HostMeta {
        labels,
        conflicting,
        effective,
        revisions,
        agent,
    } = meta;
    let mut summary = api::HostSummary::new();
    summary.set_host_id(doc.host_id.as_str());
    summary.set_compliance(doc.state.compliance.as_str());
    summary.set_state_at(doc.state_at.map(ms).unwrap_or(0));
    summary.set_first_seen(ms(doc.first_seen));
    summary.set_last_seen(ms(doc.last_seen));
    summary.set_error(doc.state.error.clone().unwrap_or_default());
    summary.set_items_json(serde_json::to_string(&doc.state.items).unwrap_or_default());
    if let Some(r) = &doc.last_assess {
        summary.set_last_assess_job(r.id.trim_start_matches("outcome:"));
    }
    if let Some(r) = &doc.last_reach {
        summary.set_last_reach_job(r.id.trim_start_matches("outcome:"));
    }
    // D24: the newest of the two, for its run log
    let newest = [&doc.last_assess, &doc.last_reach]
        .into_iter()
        .flatten()
        .filter(|r| !r.cleared)
        .max_by_key(|r| r.finished_at);
    if let Some(r) = newest {
        summary.set_last_job(r.id.trim_start_matches("outcome:"));
        summary.set_last_job_month(r.month.as_str());
    }
    for fingerprint in &doc.certs.pending {
        summary
            .pending_fingerprints_mut()
            .push(fingerprint.as_str());
    }
    if let Some((fingerprint, _)) = &doc.certs.pinned {
        summary.set_pinned_fingerprint(fingerprint.as_str());
    }
    for (k, v) in labels {
        summary.labels_mut().insert(k.as_str(), v.as_str());
    }
    summary.set_conflicting(*conflicting);
    // D29: every assignment selecting the host, in order
    for assignment in effective.assignments() {
        summary.assignments_mut().push(assignment.as_str());
    }
    summary.set_effective_revision(effective.revision().unwrap_or_default());
    summary.set_state_revision(doc.state_revision.as_deref().unwrap_or_default());
    if let Effective::Invalid { reason, .. } = effective {
        summary.set_assignment_error(reason.as_str());
    }
    for (assignment, revision) in revisions {
        summary.assignment_states_mut().push(assignment_status(
            doc,
            assignment,
            revision.as_deref(),
            effective,
        ));
    }
    if let Some(report) = &doc.agent {
        summary.set_agent(report.agent.as_str());
        summary.set_agent_version(report.agent_version.as_str());
        summary.set_agent_repo(report.repo.as_str());
        summary.set_agent_branch(report.branch.as_str());
        summary.set_agent_path(report.path.as_str());
        summary.set_agent_revision(report.revision.as_str());
    }
    if let Some(agent) = agent {
        summary.set_repo_status(agent.repo_status);
        summary.set_repo_head(agent.repo_head.as_str());
        summary.set_repo_source(agent.repo_source.as_str());
        if let Some(drift) = &agent.drift {
            summary.set_state_drift(drift.as_str());
        }
    }
    summary
}

/// D30: a host's state for one assignment: from the newest job of that
/// assignment, else from an older job that checked the whole effective state
/// while it is unchanged (before D30), else unknown.
pub fn assignment_status(
    doc: &HostDoc,
    assignment: &str,
    revision: Option<&str>,
    effective: &Effective,
) -> api::AssignmentStatus {
    let mut status = api::AssignmentStatus::new();
    status.set_assignment(assignment);
    status.set_revision(revision.unwrap_or_default());
    if let Some(own) = doc.assignments.get(assignment) {
        status.set_compliance(own.state.compliance.as_str());
        status.set_error(own.state.error.clone().unwrap_or_default());
        if !own.cleared {
            status.set_state_at(ms(own.at));
            status.set_state_revision(own.revision.as_deref().unwrap_or_default());
            status.set_job_id(own.outcome.id.trim_start_matches("outcome:"));
            status.set_job_month(own.outcome.month.as_str());
        }
        return status;
    }
    // the newest check of the whole effective state (outcomes without
    // assignment), while that state is still the current one
    if let Some(whole) = doc.assignments.get(regencylib::model::WHOLE_STATE) {
        if !whole.cleared
            && effective.revision().is_some()
            && whole.revision.as_deref() == effective.revision()
        {
            status.set_compliance(whole.state.compliance.as_str());
            status.set_error(whole.state.error.clone().unwrap_or_default());
            status.set_state_at(ms(whole.at));
            // checked in full: up to date for this assignment
            status.set_state_revision(revision.unwrap_or_default());
            status.set_job_id(whole.outcome.id.trim_start_matches("outcome:"));
            status.set_job_month(whole.outcome.month.as_str());
        } else {
            status.set_compliance("unknown");
        }
        return status;
    }
    // a doc from before D30: its latest state is a whole-state check
    let whole = doc.assignments.is_empty()
        && effective.revision().is_some()
        && doc.state_revision.as_deref() == effective.revision()
        && !doc.state_cleared
        && doc.state_at.is_some();
    if whole {
        status.set_compliance(doc.state.compliance.as_str());
        status.set_error(doc.state.error.clone().unwrap_or_default());
        status.set_state_at(doc.state_at.map(ms).unwrap_or(0));
        // checked in full: up to date for this assignment
        status.set_state_revision(revision.unwrap_or_default());
        let newest = [&doc.last_assess, &doc.last_reach]
            .into_iter()
            .flatten()
            .find(|r| Some(r.finished_at) == doc.state_at);
        if let Some(r) = newest {
            status.set_job_id(r.id.trim_start_matches("outcome:"));
            status.set_job_month(r.month.as_str());
        }
    } else {
        status.set_compliance("unknown");
    }
    status
}

/// A one-shot state from YAML (D23): one attribute, a list of attributes,
/// or a whole expected state.
pub fn one_shot_from_yaml(yaml: &str) -> Result<ExpectedState, String> {
    // the error reported is the one of the form the YAML looks like
    let shape: yaml_serde::Value = yaml_serde::from_str(yaml).map_err(|e| e.to_string())?;
    if shape.is_sequence() {
        let attributes = yaml_serde::from_str::<Vec<Attribute>>(yaml).map_err(|e| e.to_string())?;
        let mut state = ExpectedState::new();
        state.attributes = attributes;
        return Ok(state);
    }
    if shape.get("Attributes").is_some() {
        return yaml_serde::from_str::<ExpectedState>(yaml).map_err(|e| e.to_string());
    }
    let attribute = yaml_serde::from_str::<Attribute>(yaml).map_err(|e| e.to_string())?;
    let mut state = ExpectedState::new();
    state.attributes = vec![attribute];
    Ok(state)
}

/// Input of the attribute builder (D23 one-shot): YAML as written by hand,
/// or the builder's JSON.
pub enum AttributesInput<'a> {
    Yaml(&'a str),
    Json(&'a str),
}

/// Attributes in both forms, read and written by the SDK: `(yaml, json)`,
/// each a list of attributes. JSON is a list or a single attribute.
pub fn convert_attributes(input: AttributesInput) -> Result<(String, String), String> {
    let attributes: Vec<Attribute> = match input {
        AttributesInput::Yaml(yaml) => one_shot_from_yaml(yaml)?.attributes,
        AttributesInput::Json(json) => match serde_json::from_str::<Vec<Attribute>>(json) {
            Ok(list) => list,
            Err(list_error) => match serde_json::from_str::<Attribute>(json) {
                Ok(one) => vec![one],
                Err(_) => return Err(list_error.to_string()),
            },
        },
    };
    let yaml = yaml_serde::to_string(&attributes).map_err(|e| e.to_string())?;
    let json = serde_json::to_string(&attributes).map_err(|e| e.to_string())?;
    Ok((yaml, json))
}

pub fn job_summary(doc: &JobDoc) -> api::JobSummary {
    let mut summary = api::JobSummary::new();
    summary.set_job_id(doc.job_id.as_str());
    summary.set_host_id(doc.host_id.as_str());
    summary.set_job(job_to(doc.job));
    summary.set_origin(doc.origin.as_str());
    summary.set_state(doc.state.as_str());
    summary.set_created_at(ms(doc.created_at));
    summary.set_updated_at(doc.states.values().max().copied().map(ms).unwrap_or(0));
    if let Some(result) = &doc.result {
        summary.set_compliance(result.compliance.as_str());
        summary.set_error(result.error.clone().unwrap_or_default());
    }
    if let Some(outcome) = &doc.outcome {
        summary.set_outcome_month(outcome.month.as_str());
    }
    summary.set_states_json(serde_json::to_string(&doc.states).unwrap_or_default());
    summary
}

pub fn outcome_summary(doc: &OutcomeDoc, month: &str) -> api::OutcomeSummary {
    let mut summary = api::OutcomeSummary::new();
    summary.set_job_id(doc.job_id.as_str());
    summary.set_host_id(doc.host_id.as_str());
    summary.set_job(job_to(doc.job));
    summary.set_origin(doc.origin.as_str());
    summary.set_started_at(ms(doc.started_at));
    summary.set_finished_at(ms(doc.finished_at));
    summary.set_compliance(doc.state.compliance.as_str());
    summary.set_error(doc.state.error.clone().unwrap_or_default());
    summary.set_month(month);
    summary.set_one_shot(doc.one_shot);
    summary.set_state_revision(doc.state_revision.as_deref().unwrap_or_default());
    if let Some(run_log) = &doc.run_log {
        summary.set_run_log(crate::live::run_log_info(run_log));
    }
    summary
}

pub fn order_to(order: &Order, rev: &str) -> api::Order {
    let mut message = api::Order::new();
    message.set_order_id(order.order_id.as_str());
    match &order.schedule {
        Schedule::Every { seconds } => message.set_every_seconds(*seconds),
        Schedule::Cron {
            expression,
            timezone,
        } => {
            let mut cron = api::CronSchedule::new();
            cron.set_expression(expression.as_str());
            cron.set_timezone(timezone.as_str());
            message.set_cron(cron);
        }
    }
    message.set_assignment(order.assignment.as_str());
    message.set_job(job_to(order.job));
    message.set_pool(order.pool.clone().unwrap_or_default());
    message.set_enabled(order.enabled);
    message.set_author(order.author.as_str());
    message.set_rev(rev);
    message
}

/// Order from the API; `author` is set by the caller (never from the client).
pub fn order_from(
    message: api::OrderView<'_>,
    author: String,
    author_groups: Vec<String>,
) -> Result<Order, Status> {
    use api::order::ScheduleOneof;
    let schedule = match message.schedule() {
        ScheduleOneof::EverySeconds(seconds) => Schedule::Every { seconds },
        ScheduleOneof::Cron(cron) => Schedule::Cron {
            expression: cron.expression().to_string(),
            timezone: if cron.timezone().is_empty() {
                "UTC".to_string()
            } else {
                cron.timezone().to_string()
            },
        },
        _ => {
            return Err(Status::invalid_argument(
                "order needs every_seconds or cron",
            ));
        }
    };
    let pool = message.pool().to_string();
    Ok(Order {
        order_id: message.order_id().to_string(),
        schedule,
        assignment: message.assignment().to_string(),
        job: job_from(message.job())?,
        pool: (!pool.is_empty()).then_some(pool),
        author,
        author_groups,
        enabled: message.enabled(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_convert_both_ways() {
        let yaml = "Name: motd\nPrivilege: !None\nDetail: !LineInFile\n  FilePath: /etc/motd\n  State: !Present\n    Create: true\n  Line: !Raw hello\n";
        let (list_yaml, json) = convert_attributes(AttributesInput::Yaml(yaml)).unwrap();
        assert!(list_yaml.starts_with("- Name: motd"), "{list_yaml}");
        assert!(list_yaml.contains("!LineInFile"));
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value[0]["Detail"]["LineInFile"]["FilePath"], "/etc/motd");
        // and back: the builder's JSON gives the same YAML
        let (again, _) = convert_attributes(AttributesInput::Json(&json)).unwrap();
        assert_eq!(again, list_yaml);
        // a single JSON attribute is accepted too
        let one = value[0].to_string();
        assert!(convert_attributes(AttributesInput::Json(&one)).is_ok());
        assert!(convert_attributes(AttributesInput::Yaml("Detail: !Nope {}")).is_err());
        // a list reports the list's error, not a single attribute's
        let error = convert_attributes(AttributesInput::Yaml("- Name: x\n  Detail: !Nope {}\n"))
            .unwrap_err();
        assert!(!error.contains("expected struct Attribute"), "{error}");
    }

    /// D30 upgrade: a whole-state check counts for an assignment never
    /// checked alone, even once another assignment was, while the effective
    /// revision is unchanged.
    #[test]
    fn whole_state_check_counts_while_current() {
        use chrono::TimeZone;
        use regencylib::model::{AssignmentState, HostState, OutcomeRef, WHOLE_STATE};
        let state = |compliance: &str, t: i64, revision: &str, id: &str| AssignmentState {
            state: HostState {
                compliance: compliance.into(),
                items: Vec::new(),
                error: None,
            },
            at: chrono::Utc.timestamp_opt(t, 0).unwrap(),
            revision: Some(revision.into()),
            job: regent_sdk::task::Job::Assess,
            outcome: OutcomeRef {
                id: format!("outcome:{id}"),
                month: "202611".into(),
                finished_at: chrono::Utc.timestamp_opt(t, 0).unwrap(),
                cleared: false,
            },
            cleared: false,
        };
        let mut doc = HostDoc::empty("h", chrono::Utc.timestamp_opt(1, 0).unwrap());
        doc.assignments.insert(
            WHOLE_STATE.into(),
            state("not_compliant", 10, "eff-1", "whole"),
        );
        doc.assignments
            .insert("s/web".into(), state("compliant", 20, "web-1", "web"));
        let ready = |revision: &str| Effective::Ready {
            layers: Vec::new(),
            state: ExpectedState::new(),
            blame: Vec::new(),
            warnings: Vec::new(),
            revision: revision.into(),
        };
        // never checked alone: the whole-state check
        let edge = assignment_status(&doc, "s/edge", Some("edge-1"), &ready("eff-1"));
        assert_eq!(edge.compliance(), "not_compliant");
        assert_eq!(edge.job_id(), "whole");
        assert_eq!(edge.state_revision(), "edge-1", "up to date");
        // its own check wins
        let web = assignment_status(&doc, "s/web", Some("web-1"), &ready("eff-1"));
        assert_eq!(web.compliance(), "compliant");
        // the effective state changed since: not checked
        let edge = assignment_status(&doc, "s/edge", Some("edge-2"), &ready("eff-2"));
        assert_eq!(edge.compliance(), "unknown");
    }
}
