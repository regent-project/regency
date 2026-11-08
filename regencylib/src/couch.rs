//! Thin CouchDB client on reqwest (D18): documents, `conflicts=true`,
//! attachments, `_bulk_docs`, views, `_find`, `_changes`, databases and
//! `_security`. Plus [`Couch::merge_upsert`], the D8 read-merge-write loop.

use std::time::Duration;

use base64::Engine;
use regent_sdk::secrets::SecretProvidersPool;
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::config::{CouchDbConfig, resolve_secret};
use crate::model::Stored;

#[derive(Debug, thiserror::Error)]
pub enum CouchError {
    #[error("CouchDB request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("CouchDB conflict on {0}")]
    Conflict(String),
    #[error("CouchDB {status} on {what}: {body}")]
    Status {
        status: u16,
        what: String,
        body: String,
    },
    #[error("CouchDB JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("CouchDB configuration: {0}")]
    Config(String),
}

impl CouchError {
    pub fn is_conflict(&self) -> bool {
        matches!(self, CouchError::Conflict(_))
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, CouchError::Status { status: 404, .. })
    }
}

/// CouchDB client (cheap to clone).
#[derive(Clone)]
pub struct Couch {
    http: reqwest::Client,
    base: String,
    auth: Option<(String, String)>,
}

/// A `_changes` batch.
#[derive(Debug, Deserialize)]
pub struct ChangesBatch {
    pub results: Vec<Change>,
    pub last_seq: Value,
}

#[derive(Debug, Deserialize)]
pub struct Change {
    pub id: String,
    pub seq: Value,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub doc: Option<Value>,
}

/// One row of a view.
#[derive(Debug, Deserialize)]
pub struct ViewRow<K, V> {
    pub id: Option<String>,
    pub key: K,
    pub value: V,
    #[serde(default)]
    pub doc: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ViewResult<K, V> {
    rows: Vec<ViewRow<K, V>>,
}

/// Result of one `_bulk_docs` entry.
#[derive(Debug, Deserialize)]
pub struct BulkResult {
    pub id: Option<String>,
    pub rev: Option<String>,
    pub error: Option<String>,
    pub reason: Option<String>,
}

fn enc(segment: &str) -> String {
    url::form_urlencoded::byte_serialize(segment.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

impl Couch {
    pub fn new(base: &str, auth: Option<(String, String)>) -> Self {
        Self::with_tls(base, auth, &crate::config::TlsMaterial::default(), false)
            .expect("HTTP client")
    }

    /// Client with an explicit trust anchor (only it is trusted), a client
    /// identity for mutual TLS, and optionally TLS 1.3 only (D19).
    pub fn with_tls(
        base: &str,
        auth: Option<(String, String)>,
        tls: &crate::config::TlsMaterial,
        tls13_only: bool,
    ) -> Result<Self, CouchError> {
        let config_error = |e: reqwest::Error| CouchError::Config(format!("TLS: {e}"));
        let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(10));
        if let Some(ca) = &tls.ca_pem {
            builder = builder
                .tls_certs_only(reqwest::Certificate::from_pem_bundle(ca).map_err(config_error)?);
        }
        if let (Some(cert), Some(key)) = (&tls.cert_pem, &tls.key_pem) {
            let mut pem = cert.clone();
            pem.push(b'\n');
            pem.extend_from_slice(key.as_bytes());
            builder = builder.identity(reqwest::Identity::from_pem(&pem).map_err(config_error)?);
        }
        if tls13_only {
            builder = builder.min_tls_version(reqwest::tls::Version::TLS_1_3);
        }
        Ok(Self {
            http: builder.build().map_err(config_error)?,
            base: base.trim_end_matches('/').to_string(),
            auth,
        })
    }

    /// Client from the `[couchdb]` section (password through its reference).
    pub async fn from_config(
        config: &CouchDbConfig,
        secrets: &SecretProvidersPool,
    ) -> Result<Self, CouchError> {
        let auth = match (&config.username, &config.password) {
            (Some(user), Some(reference)) => Some((
                user.clone(),
                resolve_secret(secrets, reference)
                    .await
                    .map_err(|e| CouchError::Config(e.to_string()))?,
            )),
            (Some(_), None) | (None, Some(_)) => {
                return Err(CouchError::Config(
                    "username and password go together".into(),
                ));
            }
            (None, None) => None,
        };
        let tls = config
            .tls(secrets)
            .await
            .map_err(|e| CouchError::Config(e.to_string()))?;
        // TLS 1.3 only when mutual TLS is configured (security level 3, D19)
        Self::with_tls(&config.url, auth, &tls, tls.has_identity())
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self.http.request(method, format!("{}/{}", self.base, path));
        match &self.auth {
            Some((user, password)) => builder.basic_auth(user, Some(password)),
            None => builder,
        }
    }

    async fn check(
        response: reqwest::Response,
        what: &str,
    ) -> Result<reqwest::Response, CouchError> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        if status == StatusCode::CONFLICT {
            return Err(CouchError::Conflict(what.to_string()));
        }
        Err(CouchError::Status {
            status: status.as_u16(),
            what: what.to_string(),
            body,
        })
    }

    /// `GET /` (reachability, version).
    pub async fn ping(&self) -> Result<Value, CouchError> {
        let response = self.request(Method::GET, "").send().await?;
        Ok(Self::check(response, "/").await?.json().await?)
    }

    // ------------------------------------------------------------ databases

    /// Create a database; `Ok(false)` if it exists (412).
    pub async fn create_db(&self, db: &str) -> Result<bool, CouchError> {
        let response = self.request(Method::PUT, &enc(db)).send().await?;
        if response.status() == StatusCode::PRECONDITION_FAILED {
            return Ok(false);
        }
        Self::check(response, db).await?;
        Ok(true)
    }

    /// Delete a database; `Ok(false)` if already gone (404).
    pub async fn delete_db(&self, db: &str) -> Result<bool, CouchError> {
        let response = self.request(Method::DELETE, &enc(db)).send().await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        Self::check(response, db).await?;
        Ok(true)
    }

    pub async fn db_exists(&self, db: &str) -> Result<bool, CouchError> {
        let response = self.request(Method::HEAD, &enc(db)).send().await?;
        Ok(response.status().is_success())
    }

    pub async fn all_dbs(&self) -> Result<Vec<String>, CouchError> {
        let response = self.request(Method::GET, "_all_dbs").send().await?;
        Ok(Self::check(response, "_all_dbs").await?.json().await?)
    }

    /// Set a database's `_security` object.
    pub async fn put_security(&self, db: &str, security: &Value) -> Result<(), CouchError> {
        let response = self
            .request(Method::PUT, &format!("{}/_security", enc(db)))
            .json(security)
            .send()
            .await?;
        Self::check(response, db).await.map(|_| ())
    }

    // ------------------------------------------------------------ documents

    /// Get a document (with `_conflicts`), `None` if missing or deleted.
    pub async fn get<T: DeserializeOwned>(
        &self,
        db: &str,
        id: &str,
    ) -> Result<Option<Stored<T>>, CouchError> {
        self.get_query(db, id, &[("conflicts", "true")]).await
    }

    /// Get a specific revision.
    pub async fn get_rev<T: DeserializeOwned>(
        &self,
        db: &str,
        id: &str,
        rev: &str,
    ) -> Result<Option<Stored<T>>, CouchError> {
        self.get_query(db, id, &[("rev", rev)]).await
    }

    async fn get_query<T: DeserializeOwned>(
        &self,
        db: &str,
        id: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<Stored<T>>, CouchError> {
        let response = self
            .request(Method::GET, &format!("{}/{}", enc(db), enc(id)))
            .query(query)
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let value: Value = Self::check(response, id).await?.json().await?;
        Ok(Some(serde_json::from_value(value)?))
    }

    /// Create or update (with `doc.rev`); returns the new rev. 409 →
    /// [`CouchError::Conflict`] (for a create: it already exists).
    pub async fn put<T: Serialize>(&self, db: &str, doc: &Stored<T>) -> Result<String, CouchError> {
        let response = self
            .request(Method::PUT, &format!("{}/{}", enc(db), enc(&doc.id)))
            .json(doc)
            .send()
            .await?;
        #[derive(Deserialize)]
        struct Ok_ {
            rev: String,
        }
        let ok: Ok_ = Self::check(response, &doc.id).await?.json().await?;
        Ok(ok.rev)
    }

    /// Create with an inline attachment; 409 → Conflict (already exists).
    pub async fn create_with_attachment<T: Serialize>(
        &self,
        db: &str,
        doc: &Stored<T>,
        name: &str,
        content_type: &str,
        data: &[u8],
    ) -> Result<String, CouchError> {
        let mut value = serde_json::to_value(doc)?;
        value["_attachments"] = json!({
            name: {
                "content_type": content_type,
                "data": base64::engine::general_purpose::STANDARD.encode(data),
            }
        });
        let response = self
            .request(Method::PUT, &format!("{}/{}", enc(db), enc(&doc.id)))
            .json(&value)
            .send()
            .await?;
        let body: Value = Self::check(response, &doc.id).await?.json().await?;
        Ok(body["rev"].as_str().unwrap_or_default().to_string())
    }

    /// Raw attachment bytes, `None` if missing.
    pub async fn get_attachment(
        &self,
        db: &str,
        id: &str,
        name: &str,
    ) -> Result<Option<Vec<u8>>, CouchError> {
        let response = self
            .request(
                Method::GET,
                &format!("{}/{}/{}", enc(db), enc(id), enc(name)),
            )
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(
            Self::check(response, id).await?.bytes().await?.to_vec(),
        ))
    }

    pub async fn delete(&self, db: &str, id: &str, rev: &str) -> Result<(), CouchError> {
        let response = self
            .request(Method::DELETE, &format!("{}/{}", enc(db), enc(id)))
            .query(&[("rev", rev)])
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        Self::check(response, id).await.map(|_| ())
    }

    pub async fn bulk_docs(
        &self,
        db: &str,
        docs: Vec<Value>,
    ) -> Result<Vec<BulkResult>, CouchError> {
        let response = self
            .request(Method::POST, &format!("{}/_bulk_docs", enc(db)))
            .json(&json!({ "docs": docs }))
            .send()
            .await?;
        Ok(Self::check(response, "_bulk_docs").await?.json().await?)
    }

    /// Every document whose id starts with `prefix`.
    pub async fn all_docs_prefix<T: DeserializeOwned>(
        &self,
        db: &str,
        prefix: &str,
    ) -> Result<Vec<Stored<T>>, CouchError> {
        let response = self
            .request(Method::GET, &format!("{}/_all_docs", enc(db)))
            .query(&[
                ("include_docs", "true".to_string()),
                ("startkey", serde_json::to_string(prefix)?),
                (
                    "endkey",
                    serde_json::to_string(&format!("{prefix}\u{fff0}"))?,
                ),
            ])
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let result: ViewResult<Value, Value> = Self::check(response, db).await?.json().await?;
        let mut docs = Vec::new();
        for row in result.rows {
            if let Some(doc) = row.doc {
                match serde_json::from_value(doc) {
                    Ok(doc) => docs.push(doc),
                    Err(e) => warn!(db, id = ?row.id, error = %e, "skipping unreadable document"),
                }
            }
        }
        Ok(docs)
    }

    /// Create or replace a design document (`_design/<name>`).
    pub async fn put_design(&self, db: &str, name: &str, design: Value) -> Result<(), CouchError> {
        let id = format!("_design/{name}");
        let current = self
            .request(Method::GET, &format!("{}/_design/{}", enc(db), enc(name)))
            .send()
            .await?;
        let mut design = design;
        if current.status().is_success() {
            let existing: Value = current.json().await?;
            let mut comparable = existing.clone();
            if let Some(map) = comparable.as_object_mut() {
                map.remove("_id");
                map.remove("_rev");
            }
            if comparable == design {
                return Ok(());
            }
            design["_rev"] = existing["_rev"].clone();
        }
        let response = self
            .request(Method::PUT, &format!("{}/_design/{}", enc(db), enc(name)))
            .json(&design)
            .send()
            .await?;
        Self::check(response, &id).await.map(|_| ())
    }

    /// Query a view with JSON-encoded parameters (`key`, `startkey`, `limit`…).
    pub async fn view<K: DeserializeOwned, V: DeserializeOwned>(
        &self,
        db: &str,
        design: &str,
        view: &str,
        params: &[(&str, Value)],
    ) -> Result<Vec<ViewRow<K, V>>, CouchError> {
        let query: Vec<(&str, String)> = params
            .iter()
            .map(|(k, v)| {
                let text = match (k, v) {
                    // booleans and numbers as-is, keys JSON-encoded
                    (_, Value::Bool(b)) => b.to_string(),
                    (_, Value::Number(n)) if !k.contains("key") => n.to_string(),
                    _ => v.to_string(),
                };
                (*k, text)
            })
            .collect();
        let response = self
            .request(
                Method::GET,
                &format!("{}/_design/{}/_view/{}", enc(db), enc(design), enc(view)),
            )
            .query(&query)
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let result: ViewResult<K, V> = Self::check(response, view).await?.json().await?;
        Ok(result.rows)
    }

    /// Mango `_find`; returns docs and the bookmark for the next page.
    pub async fn find<T: DeserializeOwned>(
        &self,
        db: &str,
        selector: Value,
        limit: usize,
        bookmark: Option<&str>,
        sort: Option<Value>,
    ) -> Result<(Vec<Stored<T>>, Option<String>), CouchError> {
        let mut body = json!({ "selector": selector, "limit": limit });
        if let Some(bookmark) = bookmark {
            body["bookmark"] = json!(bookmark);
        }
        if let Some(sort) = sort {
            body["sort"] = sort;
        }
        let response = self
            .request(Method::POST, &format!("{}/_find", enc(db)))
            .json(&body)
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok((Vec::new(), None));
        }
        #[derive(Deserialize)]
        struct Found {
            docs: Vec<Value>,
            bookmark: Option<String>,
        }
        let found: Found = Self::check(response, "_find").await?.json().await?;
        let docs = found
            .docs
            .into_iter()
            .filter_map(|doc| serde_json::from_value(doc).ok())
            .collect();
        Ok((docs, found.bookmark))
    }

    /// One `_changes` request (with docs): long-poll for at most `timeout`,
    /// or (`timeout` = zero) an immediate answer with what exists now.
    pub async fn changes(
        &self,
        db: &str,
        since: &Value,
        timeout: Duration,
    ) -> Result<ChangesBatch, CouchError> {
        let since = match since {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let response = self
            .request(Method::GET, &format!("{}/_changes", enc(db)))
            .query(&[
                (
                    "feed",
                    if timeout.is_zero() {
                        "normal"
                    } else {
                        "longpoll"
                    }
                    .to_string(),
                ),
                ("include_docs", "true".to_string()),
                ("conflicts", "true".to_string()),
                ("since", since),
                ("timeout", timeout.as_millis().to_string()),
            ])
            .timeout(timeout + Duration::from_secs(30))
            .send()
            .await?;
        Ok(Self::check(response, "_changes").await?.json().await?)
    }

    /// Current `update_seq` of a database (to follow `_changes` from now).
    pub async fn update_seq(&self, db: &str) -> Result<Value, CouchError> {
        let response = self.request(Method::GET, &enc(db)).send().await?;
        let info: Value = Self::check(response, db).await?.json().await?;
        Ok(info["update_seq"].clone())
    }

    /// D8 read-merge-write: merge `incoming` into the stored document with
    /// `merge` (commutative, idempotent), creating it if absent; retries on
    /// 409; resolves CouchDB `_conflicts` by merging every leaf and deleting
    /// the losers. Returns the merged document.
    pub async fn merge_upsert<T, F>(
        &self,
        db: &str,
        id: &str,
        incoming: &T,
        merge: F,
    ) -> Result<T, CouchError>
    where
        T: Serialize + DeserializeOwned + Clone + PartialEq,
        F: Fn(&T, &T) -> T,
    {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let result = self.merge_once(db, id, incoming, &merge).await;
            match result {
                Err(e) if e.is_conflict() && attempt < 20 => {
                    debug!(db, id, attempt, "conflict, retrying merge");
                    tokio::time::sleep(crate::amqp::jitter(Duration::from_millis(
                        20 * u64::from(attempt),
                    )))
                    .await;
                }
                other => return other,
            }
        }
    }

    async fn merge_once<T, F>(
        &self,
        db: &str,
        id: &str,
        incoming: &T,
        merge: &F,
    ) -> Result<T, CouchError>
    where
        T: Serialize + DeserializeOwned + Clone + PartialEq,
        F: Fn(&T, &T) -> T,
    {
        let Some(current) = self.get::<T>(db, id).await? else {
            self.put(db, &Stored::new(id, incoming.clone())).await?;
            return Ok(incoming.clone());
        };
        let mut merged = merge(&current.doc, incoming);
        let mut losers = Vec::new();
        for rev in &current.conflicts {
            if let Some(leaf) = self.get_rev::<T>(db, id, rev).await? {
                merged = merge(&merged, &leaf.doc);
            }
            losers.push(json!({ "_id": id, "_rev": rev, "_deleted": true }));
        }
        if merged == current.doc && losers.is_empty() {
            return Ok(merged);
        }
        let mut winner = serde_json::to_value(Stored {
            id: id.to_string(),
            rev: current.rev.clone(),
            conflicts: Vec::new(),
            deleted: false,
            doc: merged.clone(),
        })?;
        if winner.get("_rev").is_none() {
            winner["_rev"] = Value::Null;
        }
        let mut docs = vec![winner];
        docs.extend(losers);
        let results = self.bulk_docs(db, docs).await?;
        if let Some(first) = results.first()
            && first.error.is_some()
        {
            if first.error.as_deref() == Some("conflict") {
                return Err(CouchError::Conflict(id.to_string()));
            }
            return Err(CouchError::Status {
                status: 500,
                what: id.to_string(),
                body: format!("{:?}: {:?}", first.error, first.reason),
            });
        }
        Ok(merged)
    }
}

/// Where [`follow`] starts.
pub enum Since {
    /// From the beginning: every current document is replayed first (caches).
    Start,
    /// Only changes after now.
    Now,
    /// From a known sequence.
    Seq(Value),
}

/// Follow `db`'s `_changes` until `cancel`, calling `on_change` for every
/// change (with its doc), reconnecting with backoff on errors. `on_caught_up`
/// is called after each batch once the feed reached the current end (e.g.
/// readiness after the initial replay). A missing database is waited for.
pub async fn follow<F, R>(
    couch: &Couch,
    db: &str,
    since: Since,
    cancel: &tokio_util::sync::CancellationToken,
    mut on_change: F,
    mut on_caught_up: R,
) where
    F: FnMut(Change),
    R: FnMut(),
{
    let mut since = match since {
        Since::Start => Value::from("0"),
        Since::Seq(seq) => seq,
        Since::Now => loop {
            match couch.update_seq(db).await {
                Ok(seq) => break seq,
                Err(e) => {
                    let wait = if e.is_not_found() {
                        debug!(db, "database does not exist (yet), waiting");
                        Duration::from_secs(30)
                    } else {
                        warn!(db, error = %e, "cannot read update_seq, retrying");
                        Duration::from_secs(2)
                    };
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
            }
        },
    };
    let mut delay = Duration::from_millis(500);
    // the first request answers at once (catch-up), then long-poll
    let mut wait = Duration::ZERO;
    loop {
        let batch = tokio::select! {
            _ = cancel.cancelled() => return,
            batch = couch.changes(db, &since, wait) => batch,
        };
        match batch {
            Ok(batch) => {
                delay = Duration::from_millis(500);
                for change in batch.results {
                    on_change(change);
                }
                since = batch.last_seq;
                wait = Duration::from_secs(55);
                on_caught_up();
            }
            Err(e) => {
                if e.is_not_found() {
                    debug!(db, "database does not exist (yet), waiting");
                    delay = Duration::from_secs(30);
                } else {
                    warn!(db, error = %e, "_changes failed, retrying");
                }
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
}
