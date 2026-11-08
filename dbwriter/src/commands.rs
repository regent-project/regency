//! Commands applied by dbwriter, the only CouchDB writer (D10). backend has
//! already authorized them (Cedar, D12); here only the structural rules are
//! enforced: D9 (no inline secrets), D11 (items owned by their source), D12
//! (the bootstrap admin cannot be deleted), schema validity.

use chrono::Utc;
use regencylib::couch::{Couch, CouchError};
use regencylib::messages::{Command, CommandBody};
use regencylib::model::{
    CatalogBody, CatalogItem, DB_CATALOG, DB_CONFIG, DB_ORDERS, DB_POLICIES, DB_SETTINGS, DB_STATE,
    DB_USERS, HostDoc, JOBS_PREFIX, OUTCOMES_PREFIX, SETTINGS_DOC_ID, SettingsDoc, SourceKind,
    Stored, UserDoc, host_doc_id, order_doc_id, policy_doc_id, user_doc_id,
};
use regencylib::policy::Authorizer;
use regencylib::validate::reject_inline_secrets;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tracing::info;

/// Issuer name of sourcesync (D11: the only writer of Git items).
pub const SOURCESYNC: &str = "service:sourcesync";

#[derive(Debug)]
pub enum CommandError {
    /// Valid message, refused by a rule: logged, acked, never retried.
    Refused(String),
    Couch(CouchError),
}

impl From<CouchError> for CommandError {
    fn from(e: CouchError) -> Self {
        CommandError::Couch(e)
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::Refused(reason) => write!(f, "refused: {reason}"),
            CommandError::Couch(e) => write!(f, "{e}"),
        }
    }
}

fn refuse<T>(reason: impl Into<String>) -> Result<T, CommandError> {
    Err(CommandError::Refused(reason.into()))
}

/// Write `doc` unless the stored one is identical (so re-applying a command
/// does not create a revision; for orders a new `_rev` would mean new task
/// keys, D3).
async fn put_if_changed<T: Serialize + DeserializeOwned>(
    couch: &Couch,
    db: &str,
    id: &str,
    doc: &T,
) -> Result<bool, CouchError> {
    let current = couch.get::<Value>(db, id).await?;
    let wanted = serde_json::to_value(doc)?;
    let rev = match current {
        Some(stored) => {
            if stored.doc == wanted {
                return Ok(false);
            }
            stored.rev
        }
        None => None,
    };
    let mut stored = Stored::new(id, wanted);
    stored.rev = rev;
    couch.put(db, &stored).await?;
    Ok(true)
}

/// Read-modify-write of one document (retried on conflict by the caller).
async fn modify<T, F>(
    couch: &Couch,
    db: &str,
    id: &str,
    default: T,
    change: F,
) -> Result<(), CouchError>
where
    T: Serialize + DeserializeOwned + PartialEq + Clone,
    F: FnOnce(&mut T),
{
    let current = couch.get::<T>(db, id).await?;
    let (rev, before) = match current {
        Some(stored) => (stored.rev, stored.doc),
        None => (None, default),
    };
    let mut after = before.clone();
    change(&mut after);
    if after == before && rev.is_some() {
        return Ok(());
    }
    let mut stored = Stored::new(id, after);
    stored.rev = rev;
    couch.put(db, &stored).await.map(|_| ())
}

async fn delete_doc(couch: &Couch, db: &str, id: &str) -> Result<bool, CouchError> {
    match couch.get::<Value>(db, id).await? {
        Some(stored) => {
            for rev in &stored.conflicts {
                couch.delete(db, id, rev).await?;
            }
            couch
                .delete(db, id, stored.rev.as_deref().unwrap_or_default())
                .await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

async fn source_kind(couch: &Couch, source_id: &str) -> Result<Option<SourceKind>, CouchError> {
    Ok(couch
        .get::<CatalogItem>(DB_CATALOG, &format!("source:{source_id}"))
        .await?
        .and_then(|stored| match stored.doc.body {
            CatalogBody::Source(source) => Some(source.kind),
            _ => None,
        }))
}

/// D11 ownership of an item of `source_id` written by `issuer`.
async fn check_owner(
    couch: &Couch,
    source_id: &str,
    issuer: &str,
    is_source_doc: bool,
) -> Result<(), CommandError> {
    let from_sourcesync = issuer == SOURCESYNC;
    if is_source_doc {
        if from_sourcesync {
            return refuse("sources are defined by administrators, not by sourcesync");
        }
        return Ok(());
    }
    match source_kind(couch, source_id).await? {
        None => refuse(format!("unknown source {source_id}")),
        Some(SourceKind::Git) if !from_sourcesync => refuse(format!(
            "source {source_id} is a Git source: its items are edited in Git only (D11)"
        )),
        Some(SourceKind::Api | SourceKind::Frontend) if from_sourcesync => {
            refuse(format!("source {source_id} is not a Git source"))
        }
        Some(_) => Ok(()),
    }
}

pub async fn apply(couch: &Couch, command: &Command) -> Result<(), CommandError> {
    let issuer = command.issued_by.as_str();
    match &command.body {
        CommandBody::OrderUpsert(order) => {
            order.validate().or_else(refuse)?;
            if put_if_changed(couch, DB_ORDERS, &order_doc_id(&order.order_id), order).await? {
                info!(order_id = %order.order_id, "order written");
            }
        }
        CommandBody::OrderDelete { order_id } => {
            if delete_doc(couch, DB_ORDERS, &order_doc_id(order_id)).await? {
                info!(order_id = %order_id, "order deleted");
            }
        }
        CommandBody::SettingsSet { key, value } => {
            if key == "retention_months" && value.as_i64().is_none_or(|months| months < 1) {
                return refuse("retention_months must be an integer >= 1");
            }
            let (key, value) = (key.clone(), value.clone());
            modify(
                couch,
                DB_SETTINGS,
                SETTINGS_DOC_ID,
                SettingsDoc::with_defaults(),
                |settings: &mut SettingsDoc| {
                    settings.values.insert(key, value);
                },
            )
            .await?;
            info!(setting = %command_key(command), "setting changed");
        }
        CommandBody::HostDelete { host_id } => delete_host(couch, host_id).await?,
        CommandBody::HostCertPending {
            host_id,
            fingerprint,
            auto_approve,
        } => {
            let mut doc = HostDoc::empty(host_id, Utc::now());
            if *auto_approve {
                doc.certs.pinned = Some((fingerprint.clone(), Utc::now()));
            } else {
                doc.certs.pending.insert(fingerprint.clone());
            }
            couch
                .merge_upsert(DB_STATE, &host_doc_id(host_id), &doc, HostDoc::merge)
                .await?;
            info!(host_id = %host_id, fingerprint = %fingerprint, auto_approve, "client certificate recorded");
        }
        CommandBody::HostApproveCert {
            host_id,
            fingerprint,
        } => {
            let mut doc = HostDoc::empty(host_id, Utc::now());
            doc.certs.pinned = Some((fingerprint.clone(), Utc::now()));
            couch
                .merge_upsert(DB_STATE, &host_doc_id(host_id), &doc, HostDoc::merge)
                .await?;
            info!(host_id = %host_id, fingerprint = %fingerprint, "client certificate pinned");
        }
        CommandBody::CatalogUpsert(item) => {
            reject_inline_secrets(item).or_else(refuse)?;
            if let CatalogBody::Hostdef(def) = &item.body
                && def.builder.id != def.host_id
            {
                return refuse("hostdef: builder id and host_id differ");
            }
            if let CatalogBody::Assignment(def) = &item.body {
                def.validate().or_else(refuse)?;
            }
            let is_source = matches!(item.body, CatalogBody::Source(_));
            check_owner(couch, item.source_id(), issuer, is_source).await?;
            let id = item.doc_id();
            if put_if_changed(couch, DB_CATALOG, &id, item).await? {
                info!(doc_id = %id, source_id = item.source_id(), revision = %item.revision, "catalog item written");
            }
        }
        CommandBody::CatalogDelete { doc_id, source_id } => {
            let Some(stored) = couch.get::<CatalogItem>(DB_CATALOG, doc_id).await? else {
                return Ok(());
            };
            if stored.doc.source_id() != source_id {
                return refuse(format!("{doc_id} does not belong to source {source_id}"));
            }
            let is_source = matches!(stored.doc.body, CatalogBody::Source(_));
            check_owner(couch, source_id, issuer, is_source).await?;
            if is_source {
                // the source's items go with it (host existence stays, D8)
                let rows = couch
                    .view::<Value, Value>(
                        DB_CATALOG,
                        "regency",
                        "by_source",
                        &[("key", json!(source_id))],
                    )
                    .await?;
                for row in rows {
                    if let Some(id) = row.id
                        && id != *doc_id
                    {
                        delete_doc(couch, DB_CATALOG, &id).await?;
                    }
                }
            }
            delete_doc(couch, DB_CATALOG, doc_id).await?;
            info!(doc_id = %doc_id, source_id = %source_id, "catalog item deleted");
        }
        CommandBody::SourceRefresh { .. } | CommandBody::Run(_) => {
            // not routed here (D10); harmless if it ever is
        }
        CommandBody::UserUpsert(user) => {
            if user.name.is_empty() || user.name.contains(':') {
                return refuse("invalid user name");
            }
            let id = user_doc_id(&user.name);
            let existing = couch.get::<UserDoc>(DB_USERS, &id).await?;
            let mut user = user.clone();
            // bootstrap: only set at creation, never removed (D12)
            user.bootstrap = match &existing {
                Some(stored) => stored.doc.bootstrap,
                None => user.bootstrap && issuer.starts_with("service:"),
            };
            if put_if_changed(couch, DB_USERS, &id, &user).await? {
                info!(user = %user.name, disabled = user.disabled, "account written");
            }
        }
        CommandBody::UserDelete { name } => {
            if let Some(stored) = couch.get::<UserDoc>(DB_USERS, &user_doc_id(name)).await? {
                if stored.doc.bootstrap {
                    return refuse("the bootstrap admin can be disabled, not deleted");
                }
                delete_doc(couch, DB_USERS, &user_doc_id(name)).await?;
                info!(user = %name, "account deleted");
            }
        }
        CommandBody::PolicyUpsert(policy) => {
            Authorizer::validate_policy(&policy.policy_id, &policy.text)
                .map_err(|e| CommandError::Refused(e.to_string()))?;
            if put_if_changed(
                couch,
                DB_POLICIES,
                &policy_doc_id(&policy.policy_id),
                policy,
            )
            .await?
            {
                info!(policy_id = %policy.policy_id, "policy written");
            }
        }
        CommandBody::PolicyDelete { policy_id } => {
            if delete_doc(couch, DB_POLICIES, &policy_doc_id(policy_id)).await? {
                info!(policy_id = %policy_id, "policy deleted");
            }
        }
        CommandBody::ConfigSet {
            service,
            pool,
            settings,
        } => {
            // D21 structural rules (backend validated the content)
            if !regencylib::central::SERVICES.contains(&service.as_str()) {
                return refuse(format!("unknown service {service}"));
            }
            if pool.is_some() && service != "runner" {
                return refuse("only runner configurations have a pool");
            }
            let table = regencylib::central::json_to_table(settings).or_else(refuse)?;
            let local_only = regencylib::central::bootstrap_keys_in(&table);
            if !local_only.is_empty() {
                return refuse(format!(
                    "local-only keys cannot be set centrally: {}",
                    local_only.join(", ")
                ));
            }
            let id = regencylib::central::config_doc_id(service, pool.as_deref());
            if settings.is_empty() {
                if delete_doc(couch, DB_CONFIG, &id).await? {
                    info!(doc_id = %id, "central configuration removed");
                }
            } else {
                let doc = regencylib::central::ServiceConfigDoc {
                    kind: regencylib::central::CONFIG_DOC_TYPE.to_string(),
                    service: service.clone(),
                    pool: pool.clone(),
                    settings: settings.clone(),
                    updated_by: issuer.to_string(),
                    updated_at: Some(command.issued_at),
                };
                if put_if_changed(couch, DB_CONFIG, &id, &doc).await? {
                    info!(doc_id = %id, "central configuration written");
                }
            }
        }
    }
    Ok(())
}

fn command_key(command: &Command) -> String {
    match &command.body {
        CommandBody::SettingsSet { key, .. } => key.clone(),
        _ => String::new(),
    }
}

/// D8 manual host deletion: host doc (and its pin) + its jobs and outcomes in
/// every retained database.
async fn delete_host(couch: &Couch, host_id: &str) -> Result<(), CouchError> {
    let mut removed = 0usize;
    for db in couch.all_dbs().await? {
        if !(db.starts_with(JOBS_PREFIX) || db.starts_with(OUTCOMES_PREFIX)) {
            continue;
        }
        loop {
            let rows = couch
                .view::<Value, Value>(
                    &db,
                    "regency",
                    "by_host",
                    &[
                        ("startkey", json!([host_id])),
                        ("endkey", json!([host_id, {}])),
                        ("include_docs", json!(true)),
                        ("limit", json!(500)),
                    ],
                )
                .await?;
            if rows.is_empty() {
                break;
            }
            let deletions: Vec<Value> = rows
                .iter()
                .filter_map(|row| row.doc.as_ref())
                .map(|doc| json!({ "_id": doc["_id"], "_rev": doc["_rev"], "_deleted": true }))
                .collect();
            removed += deletions.len();
            couch.bulk_docs(&db, deletions).await?;
        }
    }
    delete_doc(couch, DB_STATE, &host_doc_id(host_id)).await?;
    info!(host_id, removed, "host deleted");
    Ok(())
}
