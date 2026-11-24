//! Turning resolved hosts into published tasks (D4, D9, D16), shared by
//! recurring orders and `run` commands.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use lapin::BasicProperties;
use regencylib::amqp::Publisher;
use regencylib::assignment::{Assignments, Effective};
use regencylib::hashing::bucket;
use regencylib::messages::{JobState, LifecycleEvent, TaskEnvelope};
use regencylib::selector::ResolvedHost;
use regencylib::topology::{self, EX_LIFECYCLE, EX_TASKS, TopologyConfig};
use regencylib::validate::reject_inline_secrets;
use regent_sdk::ExpectedState;
use regent_sdk::task::{Job, RegentTask};
use tracing::{debug, warn};

/// Tasks of one dispatch waiting for their publisher confirm at once.
const IN_FLIGHT: usize = 64;

/// What each host runs (D23, D30).
pub enum Body<'a> {
    /// One task per assignment of the host in `scope` (all of them when
    /// `None`), each running that assignment's final state. The host's
    /// effective state must be valid (D29).
    Assignments {
        assignments: &'a Assignments,
        scope: Option<&'a BTreeSet<String>>,
    },
    /// A one-shot raw state, the same for every host.
    OneShot(&'a ExpectedState),
}

/// One task to publish: a host and what it checks.
struct Item<'a> {
    target: &'a ResolvedHost,
    state: ExpectedState,
    /// (assignment, revision); `None` for a one-shot state.
    checked: Option<(String, String)>,
}

/// What to build for every host.
pub struct Plan<'a> {
    pub job: Job,
    pub body: Body<'a>,
    /// Job creation time (D8): slot start, or the command's issue time.
    pub created_at: DateTime<Utc>,
    /// Order id or command id (D10).
    pub origin: &'a str,
    /// Per-message TTL (recurring tasks: until the next slot, D4).
    pub ttl: Option<Duration>,
    /// Idempotency key of a host's task for an assignment (D30).
    pub key: &'a (dyn Fn(&str, Option<&str>) -> String + Sync),
    /// Called after each published task: firing a large order is slow but
    /// alive, which the shard watchdog (D3) must see.
    pub progress: &'a (dyn Fn() + Sync),
}

pub struct Dispatcher {
    pub publisher: Publisher,
    /// For disposable channels: a refused declaration closes its channel,
    /// which must never be the publisher's.
    connection: Arc<lapin::Connection>,
    pub topology: TopologyConfig,
    declared: Mutex<HashSet<String>>,
    pub published: prometheus::IntCounterVec,
}

impl Dispatcher {
    pub fn new(
        publisher: Publisher,
        connection: Arc<lapin::Connection>,
        topology: TopologyConfig,
        published: prometheus::IntCounterVec,
    ) -> Self {
        Self {
            publisher,
            connection,
            topology,
            declared: Mutex::new(HashSet::new()),
            published,
        }
    }

    /// Declare a pool's queues the first time it is used, so tasks for a pool
    /// without any runner yet wait (until their TTL) instead of being
    /// unroutable.
    async fn ensure_pool(&self, pool: &str) -> Result<(), String> {
        if self.declared.lock().expect("pools lock").contains(pool) {
            return Ok(());
        }
        // on a disposable channel: a refusal (e.g. a pool this account may
        // not use) closes it, and must not close the publisher's
        let declared = async {
            let channel = self.connection.create_channel().await?;
            topology::declare_pool(&channel, &self.topology, pool).await?;
            let _ = channel.close(200, "declared".into()).await;
            Ok::<_, regencylib::amqp::AmqpError>(())
        };
        declared
            .await
            .map_err(|e| format!("cannot declare pool {pool}: {e}"))?;
        self.declared
            .lock()
            .expect("pools lock")
            .insert(pool.to_string());
        Ok(())
    }

    fn routing_key(&self, pool: &str, job: Job, host_id: &str) -> String {
        match job {
            Job::Assess => topology::assess_key(pool),
            Job::Reach => {
                let partitions = self.topology.reach_partitions;
                topology::reach_key(pool, partitions, bucket(host_id, partitions))
            }
        }
    }

    /// The tasks of the targets: one per host and assignment in scope
    /// (D30), or one per host for a one-shot state. Hosts without a valid
    /// effective state get none (D23, D29).
    fn items<'t>(&self, targets: &'t [ResolvedHost], plan: &Plan<'_>) -> Vec<Item<'t>> {
        let mut items = Vec::new();
        let (assignments, scope) = match &plan.body {
            Body::OneShot(state) => {
                return targets
                    .iter()
                    .map(|target| Item {
                        target,
                        state: (*state).clone(),
                        checked: None,
                    })
                    .collect();
            }
            Body::Assignments { assignments, scope } => (*assignments, *scope),
        };
        // the same for every host (D30), composed once
        let mut finals: HashMap<String, Option<(ExpectedState, String)>> = HashMap::new();
        for target in targets {
            let effective = assignments.effective(&target.labels);
            match &effective {
                Effective::Ready { .. } => {}
                Effective::Unassigned => {
                    debug!(host_id = %target.host_id, "host skipped: no assignment");
                    self.published.with_label_values(&["unassigned"]).inc();
                    continue;
                }
                Effective::Invalid { reason, .. } => {
                    warn!(host_id = %target.host_id, reason, "host skipped: invalid assignments (D29)");
                    self.published
                        .with_label_values(&["invalid_assignments"])
                        .inc();
                    continue;
                }
            }
            for assignment in effective.assignments() {
                if scope.is_some_and(|scope| !scope.contains(&assignment)) {
                    continue;
                }
                let entry = finals.entry(assignment.clone()).or_insert_with(|| {
                    assignments
                        .final_state(&assignment)
                        .inspect_err(|reason| warn!(assignment, reason, "assignment skipped"))
                        .ok()
                });
                if let Some((state, revision)) = entry {
                    items.push(Item {
                        target,
                        state: state.clone(),
                        checked: Some((assignment, revision.clone())),
                    });
                }
            }
        }
        items
    }

    /// Publish the targets' tasks, then their `PENDING` (D7). Returns how many
    /// were published; stops at the first broker error (the caller retries:
    /// keys are deterministic, duplicates are harmless). Up to
    /// [`IN_FLIGHT`] tasks wait for their confirm at once: one at a time
    /// would cap a large order at a few dozen tasks per second.
    pub async fn dispatch(
        &self,
        targets: &[ResolvedHost],
        plan: &Plan<'_>,
    ) -> Result<usize, String> {
        let pools: BTreeSet<&str> = targets.iter().map(|t| t.pool.as_str()).collect();
        for pool in pools {
            self.ensure_pool(pool).await?;
        }
        let items = self.items(targets, plan);
        let mut results =
            futures::stream::iter(items.into_iter().map(|i| self.dispatch_one(i, plan)))
                .buffer_unordered(IN_FLIGHT);
        let mut count = 0;
        while let Some(result) = results.next().await {
            if result? {
                count += 1;
                (plan.progress)();
            }
        }
        Ok(count)
    }

    /// One task, then its `PENDING` once the task's publish is confirmed.
    /// `Ok(false)`: refused (D9).
    async fn dispatch_one(&self, item: Item<'_>, plan: &Plan<'_>) -> Result<bool, String> {
        let Item {
            target,
            state: expected_state,
            checked,
        } = item;
        let key = (plan.key)(&target.host_id, checked.as_ref().map(|(a, _)| a.as_str()));
        let task = RegentTask::from(target.builder.clone(), expected_state, plan.job)
            .with_idempotency_key(key.clone());
        // D9, again at the scheduler
        if let Err(reason) = reject_inline_secrets(&task) {
            warn!(host_id = %target.host_id, job_id = %key, reason, "task not built");
            self.published.with_label_values(&["refused"]).inc();
            return Ok(false);
        }
        let envelope = TaskEnvelope::new(task, plan.created_at, plan.origin).checking(checked);
        let payload = envelope.to_bytes().map_err(|e| e.to_string())?;
        let mut properties =
            BasicProperties::default().with_content_type("application/json".into());
        if let Some(ttl) = plan.ttl {
            properties = properties.with_expiration(ttl.as_millis().max(1).to_string().into());
        }
        let routing_key = self.routing_key(&target.pool, plan.job, &target.host_id);
        self.publisher
            .publish(EX_TASKS, &routing_key, &payload, properties)
            .await
            .map_err(|e| e.to_string())?;
        let event = LifecycleEvent::for_task(&envelope.header, JobState::Pending);
        self.publisher
            .publish_json(
                EX_LIFECYCLE,
                &event.routing_key(),
                &event.to_bytes().map_err(|e| e.to_string())?,
            )
            .await
            .map_err(|e| e.to_string())?;
        debug!(host_id = %target.host_id, job_id = %key, pool = %target.pool, routing_key, "task published");
        self.published.with_label_values(&["published"]).inc();
        Ok(true)
    }
}
