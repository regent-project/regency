//! Running one task (D4, D7, D9).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lapin::BasicProperties;
use lapin::message::Delivery;
use regencylib::amqp::{Handled, Publisher};
use regencylib::control::Aborts;
use regencylib::messages::{
    JobState, LifecycleEvent, MessageError, TaskEnvelope, TaskHeader, encode_outcome,
    outcome_envelope, outcome_routing_key, set_checked,
};
use regencylib::topology::{EX_LIFECYCLE, EX_OUTCOMES};
use regencylib::validate::reject_inline_secrets;
use regent_sdk::RegentError;
use regent_sdk::secrets::SecretProvidersPool;
use regent_sdk::task::{Job, RegentTaskResult};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span, warn};

pub struct Metrics {
    pub tasks: prometheus::IntCounterVec,
    pub running: prometheus::IntGauge,
}

pub struct Executor {
    pub publisher: Publisher,
    pub secrets: SecretProvidersPool,
    pub aborts: Aborts,
    /// Cancelled on connection loss or at the end of the shutdown grace:
    /// running tasks stop and are handed back (never recorded as aborted).
    pub stop_tasks: CancellationToken,
    pub instance: String,
    pub pool: String,
    pub metrics: Arc<Metrics>,
    /// D19 external level: minimum security of host connections.
    pub host_level: regencylib::security::Level,
    /// D4: one Reach at a time per host within this runner.
    host_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Executor {
    #[allow(clippy::too_many_arguments)] // plain wiring, called once
    pub fn new(
        publisher: Publisher,
        secrets: SecretProvidersPool,
        aborts: Aborts,
        stop_tasks: CancellationToken,
        instance: String,
        pool: String,
        metrics: Arc<Metrics>,
        host_level: regencylib::security::Level,
    ) -> Self {
        Self {
            publisher,
            secrets,
            aborts,
            stop_tasks,
            instance,
            pool,
            metrics,
            host_level,
            host_locks: Mutex::new(HashMap::new()),
        }
    }

    fn host_lock(&self, host_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.host_locks.lock().expect("host locks");
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        locks.entry(host_id.to_string()).or_default().clone()
    }

    async fn lifecycle(
        &self,
        envelope: &TaskHeader,
        state: JobState,
        details: Option<String>,
    ) -> Result<(), String> {
        let mut event = LifecycleEvent::for_task(envelope, state);
        event.details = details;
        let bytes = event.to_bytes().map_err(|e| e.to_string())?;
        self.publisher
            .publish_json(EX_LIFECYCLE, &event.routing_key(), &bytes)
            .await
            .map_err(|e| e.to_string())
    }

    async fn outcome(
        &self,
        envelope: &TaskHeader,
        result: &RegentTaskResult,
    ) -> Result<(), String> {
        let bytes = result.to_proto_bytes().map_err(|e| e.to_string())?;
        let mut message = outcome_envelope(
            bytes,
            Vec::new(),
            envelope.created_at,
            &envelope.origin,
            &format!("runner/{}", self.instance),
        );
        set_checked(&mut message, envelope);
        let payload = encode_outcome(&message).map_err(|e| e.to_string())?;
        self.publisher
            .publish(
                EX_OUTCOMES,
                outcome_routing_key(result.job()),
                &payload,
                BasicProperties::default().with_content_type("application/x-protobuf".into()),
            )
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn handle(&self, delivery: Delivery) -> Handled {
        let envelope = match TaskEnvelope::from_bytes(&delivery.data) {
            Ok(envelope) => envelope,
            Err(MessageError::Sdk(_)) if TaskHeader::from_bytes(&delivery.data).is_ok() => {
                let header = TaskHeader::from_bytes(&delivery.data).expect("checked");
                return self.refuse_other_sdk(header).await;
            }
            Err(e) => {
                error!(error = %e, routing_key = %delivery.routing_key, "undecodable task, dead-lettered");
                self.metrics
                    .tasks
                    .with_label_values(&["?", "undecodable"])
                    .inc();
                return Handled::Reject;
            }
        };
        let job = match envelope.task.job() {
            Job::Assess => "assess",
            Job::Reach => "reach",
        };
        let span = info_span!(
            "task",
            job_id = envelope.task.idempotency_key(),
            host_id = envelope.task.host_id(),
            job,
            origin = %envelope.header.origin,
            pool = %self.pool,
            redelivered = delivery.redelivered,
        );
        self.run(envelope, job).instrument(span).await
    }

    async fn run(&self, envelope: TaskEnvelope, job: &'static str) -> Handled {
        let key = envelope.task.idempotency_key().to_string();
        // D7: queued and aborted → not run
        if self.aborts.is_aborted(&key) {
            return self.skip_aborted(&envelope.header, job).await;
        }
        // D9: never run a task carrying inline credentials; D19: nor one whose
        // host connection is weaker than this runner's external level
        let refusal = reject_inline_secrets(&envelope.task).and_then(|()| {
            regencylib::security::check_host(self.host_level, envelope.task.managed_host_builder())
        });
        if let Err(reason) = refusal {
            warn!(reason, "task refused");
            let now = chrono::Utc::now();
            let result = RegentTaskResult::from(
                key.clone(),
                envelope.task.host_id().to_string(),
                envelope.task.job(),
                now,
                now,
                Err(RegentError::WrongInitialization(reason)),
            );
            return self.finish(&envelope.header, &result, job, "refused").await;
        }
        // D4: a second Reach for a busy host waits here
        let host_lock = self.host_lock(envelope.task.host_id());
        let _host_guard = if envelope.task.job() == Job::Reach {
            tokio::select! {
                guard = host_lock.lock() => Some(guard),
                _ = self.stop_tasks.cancelled() => return Handled::Requeue,
            }
        } else {
            None
        };
        let token = self.stop_tasks.child_token();
        let Some(_running) = self.aborts.start(&key, token.clone()) else {
            return self.skip_aborted(&envelope.header, job).await;
        };
        if let Err(e) = self
            .lifecycle(
                &envelope.header,
                JobState::Started,
                Some(format!("runner/{}", self.instance)),
            )
            .await
        {
            warn!(error = %e, "cannot publish STARTED, handing the task back");
            return Handled::Requeue;
        }
        info!("task started");
        let started = Instant::now();
        self.metrics.running.inc();
        let mut task = envelope.task.clone();
        let result = task
            .run_with_cancellation(Some(self.secrets.clone()), token.clone())
            .await;
        self.metrics.running.dec();
        if token.is_cancelled() && !self.aborts.is_aborted(&key) {
            // connection lost or shutdown: another runner takes it (D4)
            warn!(
                duration_ms = started.elapsed().as_millis() as u64,
                "task stopped (runner stopping), handed back"
            );
            return Handled::Requeue;
        }
        let summary = regencylib::model::HostState::of_result(&result);
        info!(
            duration_ms = started.elapsed().as_millis() as u64,
            compliance = %summary.compliance,
            error = summary.error.as_deref().unwrap_or(""),
            "task finished"
        );
        self.finish(&envelope.header, &result, job, &summary.compliance)
            .await
    }

    async fn finish(
        &self,
        envelope: &TaskHeader,
        result: &RegentTaskResult,
        job: &'static str,
        label: &str,
    ) -> Handled {
        // ack only after the outcome's publish is confirmed (D4)
        match self.outcome(envelope, result).await {
            Ok(()) => {
                self.metrics.tasks.with_label_values(&[job, label]).inc();
                Handled::Ack
            }
            Err(e) => {
                warn!(error = %e, "cannot publish the outcome, handing the task back");
                Handled::Requeue
            }
        }
    }

    /// D20: a task serialized by another regent-sdk version is never run
    /// (its fields could be misread); its outcome records why.
    async fn refuse_other_sdk(&self, header: TaskHeader) -> Handled {
        let job = match header.job {
            Job::Assess => "assess",
            Job::Reach => "reach",
        };
        let span = info_span!(
            "task",
            job_id = %header.job_id,
            host_id = %header.host_id,
            job,
            origin = %header.origin,
            pool = %self.pool,
        );
        async {
            let reason = format!(
                "task built with regent-sdk {}, this runner has {}: release the scheduler and runners with the same regent-sdk (D20)",
                header.sdk_version,
                regent_sdk::VERSION
            );
            warn!(reason, "task refused");
            let now = chrono::Utc::now();
            let result = RegentTaskResult::from(
                header.job_id.clone(),
                header.host_id.clone(),
                header.job,
                now,
                now,
                Err(RegentError::WrongInitialization(reason)),
            );
            self.finish(&header, &result, job, "refused").await
        }
        .instrument(span)
        .await
    }

    async fn skip_aborted(&self, envelope: &TaskHeader, job: &'static str) -> Handled {
        match self
            .lifecycle(
                envelope,
                JobState::Aborted,
                Some("aborted before it started".to_string()),
            )
            .await
        {
            Ok(()) => {
                info!("aborted task skipped");
                self.metrics
                    .tasks
                    .with_label_values(&[job, "skipped_aborted"])
                    .inc();
                Handled::Ack
            }
            Err(e) => {
                warn!(error = %e, "cannot publish ABORTED");
                Handled::Requeue
            }
        }
    }
}
