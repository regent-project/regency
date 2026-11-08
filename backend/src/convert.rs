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
        inventories: message
            .inventories()
            .iter()
            .map(|i| i.to_string())
            .collect(),
        labels: message
            .labels()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
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
    pub inventories: Vec<String>,
    pub conflicting: bool,
    pub effective: Effective,
}

pub fn host_summary(doc: &HostDoc, meta: &HostMeta) -> api::HostSummary {
    let HostMeta {
        labels,
        inventories,
        conflicting,
        effective,
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
    for inventory in inventories {
        summary.inventories_mut().push(inventory.as_str());
    }
    summary.set_conflicting(*conflicting);
    for layer in effective.layers() {
        summary.assignments_mut().push(layer.assignment.as_str());
    }
    summary.set_effective_revision(effective.revision().unwrap_or_default());
    summary.set_state_revision(doc.state_revision.as_deref().unwrap_or_default());
    if let Effective::Invalid { reason, .. } = effective {
        summary.set_assignment_error(reason.as_str());
    }
    summary
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
}
