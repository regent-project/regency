//! Live view of running jobs (D24): every replica follows `regency.live` and
//! keeps, in memory, the recent updates of each job; `WatchLive` serves them
//! (buffered past, then as they come). Also converts regent-sdk `LiveUpdate`s
//! into the API's `LiveEntry` (for `GetRunLog` too).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regencylib::proto as api;
use regent_sdk::hosts::live::{AttributeVerdict, LiveEvent, LiveUpdate};
use tokio::sync::broadcast;
use tracing::debug;

use crate::convert::job_to;

/// A job unseen for this long leaves the memory.
const IDLE: Duration = Duration::from_secs(15 * 60);
/// At most this many jobs are kept (the least recently updated go first).
const MAX_JOBS: usize = 20_000;

/// Turns one run's `LiveUpdate`s into `LiveEntry`s, naming the attribute each
/// step belongs to (the SDK gives its name only when it starts).
#[derive(Default)]
pub struct Converter {
    names: HashMap<usize, String>,
    current: Option<usize>,
}

impl Converter {
    pub fn entry(&mut self, update: &LiveUpdate) -> api::LiveEntry {
        let mut entry = api::LiveEntry::new();
        entry.set_job_id(update.run_id.as_str());
        entry.set_host_id(update.host_id.as_str());
        entry.set_job(job_to(update.job));
        entry.set_seq(update.seq);
        entry.set_at(update.at.timestamp_millis());
        let mut index = self.current;
        let kind = match &update.event {
            LiveEvent::AttributeStarted { index: i, name } => {
                self.names.insert(*i, name.clone());
                self.current = Some(*i);
                index = Some(*i);
                "attribute_started"
            }
            LiveEvent::AttributeAssessed { index: i, verdict } => {
                index = Some(*i);
                let (name, text, count) = match verdict {
                    AttributeVerdict::Compliant => ("compliant", String::new(), 0),
                    AttributeVerdict::NonCompliant { remediations } => {
                        ("non_compliant", String::new(), *remediations as u64)
                    }
                    AttributeVerdict::NonCompliantFatal { details } => {
                        ("non_compliant_fatal", details.clone(), 0)
                    }
                    AttributeVerdict::Failed { error } => ("failed", error.clone(), 0),
                    _ => ("unknown", String::new(), 0),
                };
                entry.set_verdict(name);
                entry.set_text(text);
                entry.set_count(count);
                "attribute_assessed"
            }
            LiveEvent::RemediationStarted {
                index: i,
                remediation,
            } => {
                index = Some(*i);
                self.current = Some(*i);
                entry.set_text(serde_json::to_string(remediation).unwrap_or_default());
                "remediation_started"
            }
            LiveEvent::RemediationFinished { index: i, outcome } => {
                index = Some(*i);
                entry.set_text(match outcome {
                    Some(outcome) => serde_json::to_string(outcome).unwrap_or_default(),
                    None => String::new(),
                });
                "remediation_finished"
            }
            LiveEvent::CommandStarted { exec_id, command } => {
                entry.set_exec_id(*exec_id);
                entry.set_text(command.as_str());
                "command_started"
            }
            LiveEvent::Output {
                exec_id,
                stream,
                line,
            } => {
                entry.set_exec_id(*exec_id);
                entry.set_stream(stream.as_str());
                entry.set_text(line.as_str());
                "output"
            }
            LiveEvent::CommandFinished {
                exec_id,
                return_code,
            } => {
                entry.set_exec_id(*exec_id);
                if let Some(code) = return_code {
                    entry.set_return_code(*code);
                }
                "command_finished"
            }
            LiveEvent::Aborted => "aborted",
            LiveEvent::Dropped { updates } => {
                entry.set_count(*updates);
                "dropped"
            }
            _ => "unknown",
        };
        entry.set_kind(kind);
        match index {
            Some(i) => {
                entry.set_attribute_index(i as i64);
                entry.set_attribute(self.names.get(&i).cloned().unwrap_or_default());
            }
            None => entry.set_attribute_index(-1),
        }
        entry
    }
}

struct JobBuffer {
    host_id: String,
    entries: VecDeque<Arc<api::LiveEntry>>,
    /// Entries dropped from the front (per-job limit).
    trimmed: u64,
    last_seq: u64,
    /// `at` of the run's first update: a later first update is a new run of
    /// the same job (redelivered task), the same one is a replay.
    run_started_ms: i64,
    seen: Instant,
    converter: Converter,
}

/// What `WatchLive` follows.
#[derive(Clone, Debug)]
pub enum Target {
    Job(String),
    Host(String),
}

impl Target {
    pub fn matches(&self, entry: &api::LiveEntry) -> bool {
        match self {
            Target::Job(id) => entry.job_id() == id.as_str(),
            Target::Host(id) => entry.host_id() == id.as_str(),
        }
    }
}

/// The recent live updates of every job, from `regency.live`.
pub struct LiveHub {
    jobs: Mutex<HashMap<String, JobBuffer>>,
    per_job: usize,
    pub updates: broadcast::Sender<Arc<api::LiveEntry>>,
}

impl LiveHub {
    pub fn new(per_job: usize) -> Arc<Self> {
        let (updates, _) = broadcast::channel(8192);
        Arc::new(Self {
            jobs: Mutex::new(HashMap::new()),
            per_job: per_job.max(1),
            updates,
        })
    }

    /// One batch of `regency.live` (regent-sdk `ReportLiveRequest`).
    pub fn ingest(&self, bytes: &[u8]) {
        let batch = match regencylib::live::decode_batch(bytes) {
            Ok(batch) => batch,
            Err(e) => {
                debug!(error = %e, "unreadable live batch skipped");
                return;
            }
        };
        let mut jobs = self.jobs.lock().expect("live jobs");
        let now = Instant::now();
        for message in batch.updates() {
            let update = match LiveUpdate::from_proto(&message.to_owned()) {
                Ok(update) => update,
                Err(e) => {
                    // e.g. an event added by a newer regent-sdk
                    debug!(error = %e, "live update skipped");
                    continue;
                }
            };
            let job = jobs
                .entry(update.run_id.clone())
                .or_insert_with(|| JobBuffer {
                    host_id: update.host_id.clone(),
                    entries: VecDeque::new(),
                    trimmed: 0,
                    last_seq: 0,
                    run_started_ms: i64::MIN,
                    seen: now,
                    converter: Converter::default(),
                });
            // the stream is read again from the look-back on resubscription,
            // and a redelivered task runs again: keep each step once
            let dropped = matches!(update.event, LiveEvent::Dropped { .. });
            let at_ms = update.at.timestamp_millis();
            if !dropped && update.seq <= job.last_seq {
                if update.seq == 1 && at_ms > job.run_started_ms {
                    // a new run of the same job (redelivery): start over
                    job.entries.clear();
                    job.converter = Converter::default();
                } else {
                    continue;
                }
            }
            if !dropped {
                if update.seq == 1 {
                    job.run_started_ms = at_ms;
                }
                job.last_seq = update.seq;
            }
            job.seen = now;
            let entry = Arc::new(job.converter.entry(&update));
            job.entries.push_back(entry.clone());
            if job.entries.len() > self.per_job {
                job.entries.pop_front();
                job.trimmed += 1;
            }
            let _ = self.updates.send(entry);
        }
        if jobs.len() > MAX_JOBS {
            let mut by_age: Vec<(Instant, String)> = jobs
                .iter()
                .map(|(id, job)| (job.seen, id.clone()))
                .collect();
            by_age.sort();
            for (_, id) in by_age.into_iter().take(jobs.len() - MAX_JOBS) {
                jobs.remove(&id);
            }
        }
    }

    /// Forget jobs unseen for a while (called periodically).
    pub fn evict_idle(&self) {
        let now = Instant::now();
        self.jobs
            .lock()
            .expect("live jobs")
            .retain(|_, job| now.duration_since(job.seen) < IDLE);
    }

    /// Buffered entries of the target, in order per job, and how many were
    /// already trimmed.
    pub fn snapshot(&self, target: &Target) -> (Vec<Arc<api::LiveEntry>>, u64) {
        let jobs = self.jobs.lock().expect("live jobs");
        let mut selected: Vec<&JobBuffer> = match target {
            Target::Job(id) => jobs.get(id).into_iter().collect(),
            Target::Host(id) => jobs.values().filter(|job| &job.host_id == id).collect(),
        };
        selected.sort_by_key(|job| job.entries.front().map(|e| e.at()).unwrap_or_default());
        let trimmed = selected.iter().map(|job| job.trimmed).sum();
        let entries = selected
            .into_iter()
            .flat_map(|job| job.entries.iter().cloned())
            .collect();
        (entries, trimmed)
    }

    /// Host of a buffered job.
    pub fn host_of(&self, job_id: &str) -> Option<String> {
        self.jobs
            .lock()
            .expect("live jobs")
            .get(job_id)
            .map(|job| job.host_id.clone())
    }
}

pub fn run_log_info(summary: &regencylib::model::RunLogSummary) -> api::RunLogInfo {
    let mut info = api::RunLogInfo::new();
    info.set_updates(summary.updates);
    info.set_size_bytes(summary.size_bytes);
    info.set_limit_bytes(summary.limit_bytes);
    info.set_omitted_updates(summary.omitted_updates);
    info.set_dropped_updates(summary.dropped_updates);
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use regent_sdk::hosts::live::OutputStream;
    use regent_sdk::proto::ReportLiveRequest;
    use regent_sdk::task::Job;

    fn update(job: &str, seq: u64, event: LiveEvent) -> LiveUpdate {
        LiveUpdate {
            run_id: job.into(),
            host_id: "web-01".into(),
            job: Job::Reach,
            seq,
            at: chrono::Utc::now(),
            event,
        }
    }

    fn batch(updates: &[LiveUpdate]) -> Vec<u8> {
        use protobuf::Serialize;
        let mut request = ReportLiveRequest::new();
        for u in updates {
            request.updates_mut().push(u.to_proto());
        }
        request.serialize().unwrap()
    }

    fn started(job: &str, seq: u64) -> LiveUpdate {
        update(
            job,
            seq,
            LiveEvent::AttributeStarted {
                index: 0,
                name: "nginx".into(),
            },
        )
    }

    fn output(job: &str, seq: u64) -> LiveUpdate {
        update(
            job,
            seq,
            LiveEvent::Output {
                exec_id: 1,
                stream: OutputStream::Stdout,
                line: format!("line {seq}"),
            },
        )
    }

    #[test]
    fn entries_name_their_attribute() {
        let mut converter = Converter::default();
        let first = converter.entry(&started("j", 1));
        assert_eq!(
            (first.kind().to_string(), first.attribute().to_string()),
            ("attribute_started".to_string(), "nginx".to_string())
        );
        let line = converter.entry(&output("j", 2));
        assert_eq!(
            (
                line.kind().to_string(),
                line.text().to_string(),
                line.stream().to_string()
            ),
            (
                "output".to_string(),
                "line 2".to_string(),
                "stdout".to_string()
            )
        );
        assert_eq!(
            (line.attribute_index(), line.attribute().to_string()),
            (0, "nginx".to_string())
        );
        let done = converter.entry(&update(
            "j",
            3,
            LiveEvent::CommandFinished {
                exec_id: 1,
                return_code: Some(2),
            },
        ));
        assert_eq!(done.return_code(), 2);
        assert!(done.has_return_code());
        let lost = converter.entry(&update(
            "j",
            4,
            LiveEvent::CommandFinished {
                exec_id: 2,
                return_code: None,
            },
        ));
        assert!(!lost.has_return_code());
    }

    #[test]
    fn hub_keeps_each_step_once_and_bounds_jobs() {
        let hub = LiveHub::new(3);
        let mut rx = hub.updates.subscribe();
        let (first, second) = (started("j1", 1), output("j1", 2));
        hub.ingest(&batch(&[first.clone(), second.clone()]));
        // resubscription from the look-back: the same updates again
        hub.ingest(&batch(&[first, second, output("j1", 3)]));
        let (entries, trimmed) = hub.snapshot(&Target::Job("j1".into()));
        assert_eq!(
            entries.iter().map(|e| e.seq()).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(trimmed, 0);
        hub.ingest(&batch(&[output("j1", 4), output("j1", 5)]));
        let (entries, trimmed) = hub.snapshot(&Target::Host("web-01".into()));
        assert_eq!(
            entries.iter().map(|e| e.seq()).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert_eq!(trimmed, 2);
        assert_eq!(hub.host_of("j1").as_deref(), Some("web-01"));
        let mut seen = 0;
        while rx.try_recv().is_ok() {
            seen += 1;
        }
        assert_eq!(seen, 5, "duplicates are not broadcast");
        assert!(hub.snapshot(&Target::Job("other".into())).0.is_empty());
    }

    #[test]
    fn a_redelivered_job_starts_over() {
        let hub = LiveHub::new(100);
        hub.ingest(&batch(&[
            started("j1", 1),
            output("j1", 2),
            output("j1", 3),
        ]));
        let mut again = started("j1", 1);
        again.at += chrono::Duration::seconds(5);
        hub.ingest(&batch(&[again]));
        let (entries, _) = hub.snapshot(&Target::Job("j1".into()));
        assert_eq!(entries.iter().map(|e| e.seq()).collect::<Vec<_>>(), vec![1]);
    }
}
