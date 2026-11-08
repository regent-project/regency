//! CouchDB databases (D8): design documents, `_security` and roles, shared by
//! dbwriter (monthly databases) and `regencyctl init` (everything else).
//!
//! Roles (CouchDB users get them in `_users`):
//!
//! | Role | Gets |
//! |---|---|
//! | `regency_dbwriter` | admin of every regency database (only writer, D8); creating and dropping monthly databases needs CouchDB **server admin** (a CouchDB limitation) |
//! | `regency_backend` | reads settings, users, policies, catalog, state, jobs, outcomes |
//! | `regency_scheduler` | reads orders, policies, catalog |
//! | `regency_endpoint` | reads settings (deny list), state (pins) |
//! | `regency_sourcesync` | reads catalog |
//! | `regency_runner` | reads config |
//!
//! Every service role also reads `regency_config` (D21).

use serde_json::{Value, json};

use crate::couch::{Couch, CouchError};
use crate::model::{
    DB_CATALOG, DB_CONFIG, DB_ORDERS, DB_POLICIES, DB_SETTINGS, DB_STATE, DB_USERS, JOBS_PREFIX,
    OUTCOMES_PREFIX,
};

pub const ROLE_DBWRITER: &str = "regency_dbwriter";
pub const ROLE_BACKEND: &str = "regency_backend";
pub const ROLE_SCHEDULER: &str = "regency_scheduler";
pub const ROLE_ENDPOINT: &str = "regency_endpoint";
pub const ROLE_SOURCESYNC: &str = "regency_sourcesync";
pub const ROLE_RUNNER: &str = "regency_runner";

/// Reader roles of a database (D8 table).
pub fn reader_roles(db: &str) -> Vec<&'static str> {
    match db {
        DB_SETTINGS => vec![ROLE_BACKEND, ROLE_ENDPOINT],
        DB_ORDERS => vec![ROLE_SCHEDULER, ROLE_BACKEND],
        DB_USERS => vec![ROLE_BACKEND],
        DB_POLICIES => vec![ROLE_BACKEND, ROLE_SCHEDULER],
        DB_CATALOG => vec![ROLE_SCHEDULER, ROLE_BACKEND, ROLE_SOURCESYNC],
        DB_STATE => vec![ROLE_BACKEND, ROLE_ENDPOINT],
        DB_CONFIG => vec![
            ROLE_BACKEND,
            ROLE_SCHEDULER,
            ROLE_ENDPOINT,
            ROLE_SOURCESYNC,
            ROLE_RUNNER,
        ],
        db if db.starts_with(JOBS_PREFIX) || db.starts_with(OUTCOMES_PREFIX) => vec![ROLE_BACKEND],
        _ => vec![],
    }
}

/// `_security` of a database. Members can read; in CouchDB they could also
/// write documents, so readers' accounts must only be used by the services
/// that read (a `validate_doc_update` in [`design`] refuses non-dbwriter
/// writes as well).
pub fn security(db: &str) -> Value {
    json!({
        "admins": { "names": [], "roles": [ROLE_DBWRITER] },
        "members": { "names": [], "roles": reader_roles(db) },
    })
}

const ONLY_DBWRITER: &str = r#"function (newDoc, oldDoc, userCtx) {
  if (userCtx.roles.indexOf('_admin') === -1 && userCtx.roles.indexOf('regency_dbwriter') === -1) {
    throw({ forbidden: 'only regency dbwriter writes here' });
  }
}"#;

/// Design document (`_design/regency`) of a database.
pub fn design(db: &str) -> Value {
    let mut views = serde_json::Map::new();
    if db.starts_with(JOBS_PREFIX) {
        views.insert(
            "by_host".into(),
            json!({ "map": "function (doc) { if (doc.type === 'job') emit([doc.host_id, doc.created_at], null); }" }),
        );
        views.insert(
            "by_origin".into(),
            json!({ "map": "function (doc) { if (doc.type === 'job') emit([doc.origin, doc.created_at], null); }" }),
        );
        views.insert(
            "by_state".into(),
            json!({ "map": "function (doc) { if (doc.type === 'job') emit([doc.state, doc.created_at], null); }" }),
        );
    } else if db.starts_with(OUTCOMES_PREFIX) {
        views.insert(
            "by_host".into(),
            json!({ "map": "function (doc) { if (doc.type === 'outcome') emit([doc.host_id, doc.finished_at], null); }" }),
        );
        views.insert(
            "by_origin".into(),
            json!({ "map": "function (doc) { if (doc.type === 'outcome') emit([doc.origin, doc.finished_at], null); }" }),
        );
    } else if db == DB_STATE {
        views.insert(
            "by_month".into(),
            json!({ "map": "function (doc) { if (doc.type === 'host') { if (doc.last_assess) emit(doc.last_assess.month, null); if (doc.last_reach && (!doc.last_assess || doc.last_reach.month !== doc.last_assess.month)) emit(doc.last_reach.month, null); } }" }),
        );
    } else if db == DB_CATALOG {
        views.insert(
            "by_source".into(),
            json!({ "map": "function (doc) { if (doc.source_id) emit(doc.source_id, doc.revision); }" }),
        );
    }
    json!({
        "language": "javascript",
        "views": views,
        "validate_doc_update": ONLY_DBWRITER,
    })
}

/// Create (if needed) a database with its design document and `_security`.
/// Idempotent; returns whether the database was created.
pub async fn ensure(couch: &Couch, db: &str) -> Result<bool, CouchError> {
    let created = couch.create_db(db).await?;
    couch.put_security(db, &security(db)).await?;
    couch.put_design(db, "regency", design(db)).await?;
    Ok(created)
}

/// The databases that are never rotated.
pub const STATIC: [&str; 7] = [
    DB_CONFIG,
    DB_SETTINGS,
    DB_ORDERS,
    DB_USERS,
    DB_POLICIES,
    DB_CATALOG,
    DB_STATE,
];
