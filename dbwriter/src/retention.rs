//! Retention (D8): drop whole monthly databases older than
//! `retention_months`, after resetting the host states that refer to them;
//! pre-create next month's databases. Every replica runs it (idempotent: 404
//! on drop, 412 on create), no election.

use chrono::{DateTime, Utc};
use regencylib::couch::{Couch, CouchError};
use regencylib::model::{
    DB_SETTINGS, DB_STATE, HostDoc, JOBS_PREFIX, OUTCOMES_PREFIX, SETTINGS_DOC_ID, SettingsDoc,
    Stored, month_offset,
};
use serde_json::{Value, json};
use tracing::{error, info, warn};

use crate::writer::Writer;

/// Create the settings document with its defaults if absent (create only).
pub async fn ensure_settings(couch: &Couch) -> Result<(), CouchError> {
    match couch
        .put(
            DB_SETTINGS,
            &Stored::new(SETTINGS_DOC_ID, SettingsDoc::with_defaults()),
        )
        .await
    {
        Ok(_) => {
            info!("default settings created");
            Ok(())
        }
        Err(CouchError::Conflict(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Months (`YYYYMM`) whose databases must be dropped, given the existing
/// databases. Guards: current and previous months are never dropped;
/// `retention_months` < 1 or unknown → nothing.
pub fn months_to_drop(
    databases: &[String],
    now: chrono::DateTime<Utc>,
    retention: Option<i64>,
) -> Vec<String> {
    let Some(retention) = retention.filter(|r| *r >= 1) else {
        return Vec::new();
    };
    let cutoff = month_offset(now, -(retention.min(1200) as i32));
    let protected = month_offset(now, -1);
    let mut months: Vec<String> = databases
        .iter()
        .filter_map(|db| {
            db.strip_prefix(JOBS_PREFIX)
                .or_else(|| db.strip_prefix(OUTCOMES_PREFIX))
        })
        .filter(|m| m.len() == 6 && m.chars().all(|c| c.is_ascii_digit()))
        .filter(|m| **m < *cutoff.as_str() && **m < *protected.as_str())
        .map(str::to_string)
        .collect();
    months.sort();
    months.dedup();
    months
}

pub async fn run_once(writer: &Writer) -> Result<(), CouchError> {
    let couch = &writer.couch;
    let now = Utc::now();
    // pre-create this and next month (with design docs and _security)
    writer.ensure_month(&month_offset(now, 0)).await?;
    writer.ensure_month(&month_offset(now, 1)).await?;

    ensure_settings(couch).await?;
    let retention = couch
        .get::<SettingsDoc>(DB_SETTINGS, SETTINGS_DOC_ID)
        .await?
        .and_then(|stored| stored.doc.retention_months());
    if retention.is_none_or(|r| r < 1) {
        warn!(retention = ?retention, "retention_months unreadable or < 1: nothing dropped");
        return Ok(());
    }
    forget_instances(couch, now).await?;
    let databases = couch.all_dbs().await?;
    for month in months_to_drop(&databases, now, retention) {
        let reset = reset_hosts(couch, &month).await?;
        for db in [
            format!("{JOBS_PREFIX}{month}"),
            format!("{OUTCOMES_PREFIX}{month}"),
        ] {
            if couch.delete_db(&db).await? {
                info!(
                    db,
                    month,
                    retention,
                    hosts_reset = reset,
                    "database dropped by retention"
                );
            }
            writer.forget(&db);
        }
    }
    Ok(())
}

/// D21: remove instance docs unseen for [`INSTANCE_FORGOTTEN_AFTER`].
async fn forget_instances(couch: &Couch, now: DateTime<Utc>) -> Result<(), CouchError> {
    use regencylib::central::{INSTANCE_FORGOTTEN_AFTER, InstanceDoc};
    for stored in couch
        .all_docs_prefix::<InstanceDoc>(DB_STATE, "instance:")
        .await?
    {
        if now - stored.doc.status.at > INSTANCE_FORGOTTEN_AFTER
            && let Some(rev) = &stored.rev
        {
            couch.delete(DB_STATE, &stored.id, rev).await?;
            info!(doc_id = %stored.id, "instance forgotten (unseen for 7 days)");
        }
    }
    Ok(())
}

/// Reset hosts whose state refers to `month` to `unknown` (merge-based: a
/// newer outcome always wins). Returns how many were reset.
async fn reset_hosts(couch: &Couch, month: &str) -> Result<usize, CouchError> {
    let rows = couch
        .view::<Value, Value>(DB_STATE, "regency", "by_month", &[("key", json!(month))])
        .await?;
    let mut count = 0;
    for row in rows {
        let Some(id) = row.id else { continue };
        let Some(stored) = couch.get::<HostDoc>(DB_STATE, &id).await? else {
            continue;
        };
        let mut cleared = stored.doc.clone();
        if cleared.clear_month(month) {
            couch
                .merge_upsert(DB_STATE, &id, &cleared, HostDoc::merge)
                .await?;
            count += 1;
        }
    }
    Ok(count)
}

/// Daily (jittered) loop.
pub async fn run(
    writer: std::sync::Arc<Writer>,
    first_delay: std::time::Duration,
    interval: std::time::Duration,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut delay = first_delay + regencylib::amqp::jitter(first_delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        let result =
            tracing::Instrument::instrument(run_once(&writer), tracing::info_span!("retention"))
                .await;
        if let Err(e) = result {
            error!(error = %e, "retention cleanup failed (retried next run)");
        }
        delay = interval + regencylib::amqp::jitter(interval / 10);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn drops_follow_the_d8_example() {
        let now = Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap();
        let dbs: Vec<String> = [
            "202606", "202607", "202608", "202609", "202610", "202611", "202612",
        ]
        .iter()
        .flat_map(|m| [format!("regency_jobs_{m}"), format!("regency_outcomes_{m}")])
        .chain([
            "regency_state".to_string(),
            "regency_jobs_bogus".to_string(),
        ])
        .collect();
        assert_eq!(months_to_drop(&dbs, now, Some(3)), ["202606", "202607"]);
        // guards
        assert!(months_to_drop(&dbs, now, Some(0)).is_empty());
        assert!(months_to_drop(&dbs, now, None).is_empty());
        assert_eq!(
            months_to_drop(&dbs, now, Some(1)),
            ["202606", "202607", "202608", "202609"]
        );
    }
}
