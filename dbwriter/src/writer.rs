//! Recording outcomes and lifecycle events (D7, D8).

use std::collections::HashSet;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use regencylib::couch::{Couch, CouchError};
use regencylib::messages::{DecodedOutcome, JobState, LifecycleEvent, TaskHeader};
use regencylib::model::{
    DB_STATE, HostDoc, JobDoc, OUTCOME_ATTACHMENT, OutcomeDoc, OutcomeRef, Stored, host_doc_id,
    job_doc_id, jobs_db, month_of, outcome_doc_id, outcomes_db,
};
use tracing::{debug, info};

/// Transient = worth retrying (CouchDB unreachable, 5xx, lost race).
pub fn is_transient(error: &CouchError) -> bool {
    match error {
        CouchError::Http(_) | CouchError::Conflict(_) => true,
        CouchError::Status { status, .. } => *status >= 500 || *status == 429 || *status == 408,
        CouchError::Json(_) | CouchError::Config(_) => false,
    }
}

pub struct Writer {
    pub couch: Couch,
    known: Mutex<HashSet<String>>,
    pub written: prometheus::IntCounterVec,
}

impl Writer {
    pub fn new(couch: Couch, written: prometheus::IntCounterVec) -> Self {
        Self {
            couch,
            known: Mutex::new(HashSet::new()),
            written,
        }
    }

    /// Make sure the month's jobs and outcomes databases exist (normally
    /// pre-created by the cleanup; created on demand otherwise).
    pub async fn ensure_month(&self, month: &str) -> Result<(), CouchError> {
        for db in [jobs_db(month), outcomes_db(month)] {
            if self.known.lock().expect("known lock").contains(&db) {
                continue;
            }
            if regencylib::databases::ensure(&self.couch, &db).await? {
                info!(db, "database created");
            }
            self.known.lock().expect("known lock").insert(db);
        }
        Ok(())
    }

    /// Forget a dropped database (retention).
    pub fn forget(&self, db: &str) {
        self.known.lock().expect("known lock").remove(db);
    }

    /// Outcome → `outcome:` (created once), `job:` and `host:` (merged).
    pub async fn outcome(&self, decoded: &DecodedOutcome) -> Result<(), CouchError> {
        let month = month_of(decoded.created_at);
        self.ensure_month(&month).await?;
        let key = decoded.result.idempotency_key();
        let id = outcome_doc_id(key);
        let db = outcomes_db(&month);
        let mut fresh = OutcomeDoc::from_result(
            &decoded.result,
            decoded.created_at,
            &decoded.origin,
            &decoded.publisher,
        );
        fresh.state_revision = decoded.state_revision.clone();
        fresh.one_shot = decoded.one_shot;
        let doc = match self
            .couch
            .create_with_attachment(
                &db,
                &Stored::new(&id, fresh.clone()),
                OUTCOME_ATTACHMENT,
                "application/x-protobuf",
                &decoded.result_bytes,
            )
            .await
        {
            Ok(_) => {
                self.written.with_label_values(&["outcome"]).inc();
                fresh
            }
            // 409: already recorded, the first outcome wins (D4)
            Err(CouchError::Conflict(_)) => {
                self.written.with_label_values(&["outcome_duplicate"]).inc();
                debug!("duplicate outcome, keeping the first one");
                match self.couch.get::<OutcomeDoc>(&db, &id).await? {
                    Some(stored) => stored.doc,
                    None => return Err(CouchError::Conflict(id)),
                }
            }
            Err(e) => return Err(e),
        };
        let reference = OutcomeRef {
            id: id.clone(),
            month: month.clone(),
            finished_at: doc.finished_at,
            cleared: false,
        };
        let job = JobDoc::from_outcome_doc(&doc, reference.clone());
        self.couch
            .merge_upsert(&jobs_db(&month), &job_doc_id(key), &job, JobDoc::merge)
            .await?;
        // a one-shot attribute is history only (D23): it proves the host
        // exists, never sets its state
        let host = if doc.one_shot {
            HostDoc::empty(&doc.host_id, doc.finished_at)
        } else {
            HostDoc::from_outcome_doc(&doc, reference)
        };
        self.couch
            .merge_upsert(DB_STATE, &host_doc_id(&doc.host_id), &host, HostDoc::merge)
            .await?;
        Ok(())
    }

    /// Instance status (D21) → `instance:` doc in `regency_state`, newest wins.
    pub async fn instance_status(
        &self,
        status: &regencylib::central::InstanceStatus,
    ) -> Result<(), CouchError> {
        use regencylib::central::{INSTANCE_DOC_TYPE, InstanceDoc, instance_doc_id};
        let doc = InstanceDoc {
            kind: INSTANCE_DOC_TYPE.to_string(),
            status: status.clone(),
        };
        self.couch
            .merge_upsert(
                DB_STATE,
                &instance_doc_id(&status.service, &status.instance),
                &doc,
                |a: &InstanceDoc, b: &InstanceDoc| {
                    if b.status.at > a.status.at {
                        b.clone()
                    } else {
                        a.clone()
                    }
                },
            )
            .await?;
        self.written.with_label_values(&["instance"]).inc();
        Ok(())
    }

    /// Lifecycle event → merged `job:` doc.
    pub async fn lifecycle(&self, event: &LifecycleEvent) -> Result<(), CouchError> {
        let month = month_of(event.created_at);
        self.ensure_month(&month).await?;
        let job = JobDoc::from_event(event);
        self.couch
            .merge_upsert(
                &jobs_db(&month),
                &job_doc_id(&event.job_id),
                &job,
                JobDoc::merge,
            )
            .await?;
        self.written.with_label_values(&["lifecycle"]).inc();
        Ok(())
    }

    /// Dead-lettered task (D7): `EXPIRED` (TTL) or `FAILED` (delivery limit or
    /// other).
    pub async fn dead_task(
        &self,
        envelope: &TaskHeader,
        reason: &str,
        at: DateTime<Utc>,
    ) -> Result<(), CouchError> {
        let state = if reason == "expired" {
            JobState::Expired
        } else {
            JobState::Failed
        };
        let mut event = LifecycleEvent::for_task(envelope, state)
            .with_details(format!("dead-lettered: {reason}"));
        event.at = at;
        self.lifecycle(&event).await
    }
}
