//! The `regency.v1.Regency` service (D10, D12, D17).

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::Stream;
use regencylib::amqp::PublisherLink;
use regencylib::api::regency_server::Regency;
use regencylib::assignment::Effective;
use regencylib::config::RabbitMqConfig;
use regencylib::control::AbortPublisher;
use regencylib::messages::{
    AbortRequest, Command, CommandBody, JobState, LifecycleEvent, RunCommand,
};
use regencylib::model::{
    AccountKind, CatalogBody, CatalogItem, DB_ORDERS, DB_SETTINGS, JOBS_PREFIX, JobDoc,
    OUTCOME_ATTACHMENT, OUTCOMES_PREFIX, Order, OutcomeDoc, PolicyDoc, SETTINGS_DOC_ID,
    SettingsDoc, TokenHash, UserDoc, assignment_doc_id, job_doc_id, jobs_db, month_offset,
    outcome_doc_id, outcomes_db,
};
use regencylib::policy::{Authorizer, DecisionCache, Resource, TEMPLATES, job_action};
use regencylib::proto as api;
use regencylib::selector::{HostSelector, resolve};
use regencylib::tokens::{KeyRing, issue_enrollment};
use regencylib::topology::{EX_COMMANDS, EX_LIFECYCLE};
use regencylib::validate::reject_inline_secrets;
use regent_sdk::secrets::SecretProvidersPool;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tonic::{Request, Response, Status};
use tracing::{Instrument, Span, info, info_span, warn};

use crate::auth::{Authenticator, Caller, hash_secret};
use crate::convert::*;
use crate::view::{HostChange, View};

type EventStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

pub struct Api {
    pub view: Arc<View>,
    pub auth: Arc<Authenticator>,
    pub publisher: PublisherLink,
    pub publish_wait: Duration,
    pub rabbitmq: RabbitMqConfig,
    pub secrets: SecretProvidersPool,
    pub aborts: tokio::sync::Mutex<Option<AbortPublisher>>,
    pub enrollment_keys: Option<KeyRing>,
    pub rpcs: prometheus::IntCounterVec,
    /// Cancelled when backend stops: live views end then, or the graceful
    /// shutdown would wait for every open browser forever.
    pub shutdown: tokio_util::sync::CancellationToken,
}

/// Run a live view's feeding task until it ends or backend stops; ending it
/// drops the sender, which ends the client's stream.
async fn until_shutdown(
    shutdown: tokio_util::sync::CancellationToken,
    task: impl std::future::Future<Output = ()>,
) {
    tokio::select! {
        _ = task => {}
        _ = shutdown.cancelled() => {}
    }
}

fn rpc_span(rpc: &'static str) -> Span {
    info_span!("rpc", rpc, principal = tracing::field::Empty)
}

fn require(value: &str, field: &str) -> Result<(), Status> {
    if value.trim().is_empty() {
        Err(Status::invalid_argument(format!("{field} is required")))
    } else {
        Ok(())
    }
}

fn internal(e: impl std::fmt::Display) -> Status {
    warn!(error = %e, "internal error");
    Status::internal(e.to_string())
}

impl Api {
    async fn caller<T>(&self, request: &Request<T>) -> Result<Caller, Status> {
        let caller = self.auth.authenticate(request.metadata()).await?;
        Span::current().record("principal", caller.principal.display().as_str());
        Ok(caller)
    }

    fn allow(&self, caller: &Caller, action: &str, resource: &Resource) -> Result<(), Status> {
        if caller.bootstrap
            || self
                .view
                .authorizer()
                .is_authorized(&caller.principal, action, resource)
        {
            Ok(())
        } else {
            warn!(action, resource = ?resource, "denied");
            Err(Status::permission_denied(format!(
                "not allowed to {action} {resource:?}"
            )))
        }
    }

    fn host_resource(&self, host_id: &str) -> Resource {
        Resource::host(host_id, &self.view.hostdefs().0)
    }

    async fn publish_command(
        &self,
        caller: &Caller,
        command_id: &str,
        body: CommandBody,
    ) -> Result<Response<api::Accepted>, Status> {
        require(command_id, "command_id")?;
        let command = Command::new(command_id, caller.principal.display(), body);
        let publisher = self
            .publisher
            .wait(self.publish_wait)
            .await
            .ok_or_else(|| {
                Status::unavailable("RabbitMQ unavailable, retry with the same command_id")
            })?;
        publisher
            .publish_json(
                EX_COMMANDS,
                command.routing_key(),
                &command.to_bytes().map_err(internal)?,
            )
            .await
            .map_err(|e| Status::unavailable(format!("command not accepted, retry: {e}")))?;
        info!(command_id, kind = command.routing_key(), "command accepted");
        let mut accepted = api::Accepted::new();
        accepted.set_command_id(command_id);
        Ok(Response::new(accepted))
    }

    fn done<T>(&self, rpc: &str, started: Instant, result: Result<T, Status>) -> Result<T, Status> {
        let duration_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(_) => {
                self.rpcs.with_label_values(&[rpc, "ok"]).inc();
                tracing::debug!(duration_ms, "rpc ok");
            }
            Err(status) => {
                self.rpcs
                    .with_label_values(&[rpc, &format!("{:?}", status.code())])
                    .inc();
                info!(duration_ms, code = ?status.code(), error = status.message(), "rpc failed");
            }
        }
        result
    }

    /// Retained months (`YYYYMM`) whose database with `prefix` exists, newest
    /// first. Derived from `retention_months` (D8): `_all_dbs` is reserved to
    /// CouchDB server admins, which backend is not.
    async fn retained_months(&self, prefix: &str) -> Result<Vec<String>, Status> {
        let retention = self
            .view
            .couch
            .get::<SettingsDoc>(DB_SETTINGS, SETTINGS_DOC_ID)
            .await
            .map_err(internal)?
            .and_then(|s| s.doc.retention_months())
            .unwrap_or(regencylib::model::DEFAULT_RETENTION_MONTHS)
            .clamp(1, 120);
        let now = chrono::Utc::now();
        let mut months = Vec::new();
        // + 1: a month is dropped only once older than the retention (D8)
        for offset in 0..=(retention + 1) {
            let month = month_offset(now, -(offset as i32));
            if self
                .view
                .couch
                .db_exists(&format!("{prefix}{month}"))
                .await
                .map_err(internal)?
            {
                months.push(month);
            }
        }
        Ok(months)
    }

    async fn outcome_months(&self) -> Result<Vec<String>, Status> {
        self.retained_months(OUTCOMES_PREFIX).await
    }

    async fn find_job(&self, job_id: &str) -> Result<Option<JobDoc>, Status> {
        for month in self.retained_months(JOBS_PREFIX).await? {
            if let Some(stored) = self
                .view
                .couch
                .get::<JobDoc>(&jobs_db(&month), &job_doc_id(job_id))
                .await
                .map_err(internal)?
            {
                return Ok(Some(stored.doc));
            }
        }
        Ok(None)
    }

    fn catalog_resource(item: &CatalogItem) -> Resource {
        match &item.body {
            CatalogBody::Source(d) => Resource::Source(d.source_id.clone()),
            CatalogBody::Inventory(d) => Resource::Inventory(format!("{}/{}", d.source_id, d.name)),
            CatalogBody::Hostdef(d) => Resource::Host {
                id: d.host_id.clone(),
                inventories: d
                    .inventories
                    .iter()
                    .map(|i| format!("{}/{}", d.source_id, i))
                    .collect(),
                sources: vec![d.source_id.clone()],
            },
            CatalogBody::ExpectedState(d) => {
                Resource::ExpectedState(format!("{}/{}", d.source_id, d.name))
            }
            // D23: authorized as a change of its source
            CatalogBody::Assignment(d) => Resource::Source(d.source_id.clone()),
        }
    }

    async fn upsert_user(
        &self,
        caller: &Caller,
        command_id: &str,
        user: UserDoc,
    ) -> Result<Response<api::Accepted>, Status> {
        self.publish_command(caller, command_id, CommandBody::UserUpsert(user))
            .await
    }
}

/// Does a job match a WatchJobs filter (host ids: `None` = any)?
enum JobFilter {
    Job(String),
    Origin(String),
    Hosts(Option<HashSet<String>>),
}

impl JobFilter {
    fn matches(&self, job: &JobDoc) -> bool {
        match self {
            JobFilter::Job(id) => job.job_id == *id,
            JobFilter::Origin(origin) => job.origin == *origin,
            JobFilter::Hosts(None) => true,
            JobFilter::Hosts(Some(ids)) => ids.contains(&job.host_id),
        }
    }
}

#[tonic::async_trait]
impl Regency for Api {
    type WatchHostsStream = EventStream<api::HostEvent>;
    type WatchJobsStream = EventStream<api::JobEvent>;

    async fn login(
        &self,
        request: Request<api::LoginRequest>,
    ) -> Result<Response<api::LoginResponse>, Status> {
        let started = Instant::now();
        async {
            let message = request.get_ref();
            let (token, expires_at) = self
                .auth
                .login(
                    &message.username().to_string(),
                    &message.password().to_string(),
                )
                .await?;
            let mut response = api::LoginResponse::new();
            response.set_token(token);
            response.set_expires_at(expires_at);
            Ok(Response::new(response))
        }
        .instrument(rpc_span("Login"))
        .await
        .pipe(|r| self.done("Login", started, r))
    }

    async fn who_am_i(
        &self,
        request: Request<api::WhoAmIRequest>,
    ) -> Result<Response<api::WhoAmIResponse>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let mut response = api::WhoAmIResponse::new();
            response.set_principal(caller.principal.display());
            for group in &caller.principal.groups {
                response.groups_mut().push(group.as_str());
            }
            response.set_bootstrap_admin(caller.bootstrap);
            Ok(Response::new(response))
        }
        .instrument(rpc_span("WhoAmI"))
        .await
        .pipe(|r| self.done("WhoAmI", started, r))
    }

    async fn watch_hosts(
        &self,
        request: Request<api::WatchHostsRequest>,
    ) -> Result<Response<Self::WatchHostsStream>, Status> {
        let span = rpc_span("WatchHosts");
        let caller = self.caller(&request).instrument(span.clone()).await?;
        let selector = selector_from(request.get_ref().selector());
        let (tx, rx) = mpsc::channel::<Result<api::HostEvent, Status>>(512);
        let view = self.view.clone();
        let api = ApiRef { view: view.clone() };
        tokio::spawn(until_shutdown(
            self.shutdown.clone(),
            async move {
                info!(selector = ?selector, "live view of hosts opened");
                let mut changes = view.host_changes.subscribe();
                let mut authorizer = view.authorizer();
                let mut cache = DecisionCache::default();
                let mut version = view.catalog_version();
                let mut selected = view.selected(&selector);
                let mut refresh = tokio::time::interval(Duration::from_secs(30));
                refresh.tick().await;
                let summary = |doc: &regencylib::model::HostDoc| {
                    host_summary(doc, &view.host_meta(&doc.host_id))
                };
                // labels, inventories and assignments come from the catalog:
                // resend the snapshot when it changes (D23)
                let mut catalog_check = tokio::time::interval(Duration::from_secs(2));
                catalog_check.tick().await;
                let mut snapshot = true;
                loop {
                    if snapshot {
                        for doc in view.hosts() {
                            if selected.as_ref().is_none_or(|s| s.contains(&doc.host_id))
                                && api.readable(&caller, &mut cache, &authorizer, &doc.host_id)
                            {
                                let mut event = api::HostEvent::new();
                                event.set_upsert(summary(&doc));
                                if tx.send(Ok(event)).await.is_err() {
                                    return;
                                }
                            }
                        }
                        let mut done = api::HostEvent::new();
                        done.set_snapshot_done(true);
                        if tx.send(Ok(done)).await.is_err() {
                            return;
                        }
                        snapshot = false;
                    }
                    let change = tokio::select! {
                        _ = tx.closed() => return,
                        _ = refresh.tick() => {
                            // policies or catalog may have changed: start over
                            authorizer = view.authorizer();
                            cache = DecisionCache::default();
                            continue;
                        }
                        _ = catalog_check.tick() => {
                            if view.catalog_version() != version {
                                version = view.catalog_version();
                                selected = view.selected(&selector);
                                snapshot = true;
                            }
                            continue;
                        }
                        change = changes.recv() => change,
                    };
                    if view.catalog_version() != version {
                        version = view.catalog_version();
                        selected = view.selected(&selector);
                    }
                    let event = match change {
                        Ok(HostChange::Upsert(doc)) => {
                            if !selected.as_ref().is_none_or(|s| s.contains(&doc.host_id))
                                || !api.readable(&caller, &mut cache, &authorizer, &doc.host_id)
                            {
                                continue;
                            }
                            let mut event = api::HostEvent::new();
                            event.set_upsert(summary(&doc));
                            event
                        }
                        Ok(HostChange::Removed(host_id)) => {
                            let mut event = api::HostEvent::new();
                            event.set_removed(host_id);
                            event
                        }
                        Err(broadcast::error::RecvError::Lagged(missed)) => {
                            warn!(missed, "slow live-view client, resending the snapshot");
                            snapshot = true;
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    };
                    if tx.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            }
            .instrument(span),
        ));
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }

    async fn watch_jobs(
        &self,
        request: Request<api::WatchJobsRequest>,
    ) -> Result<Response<Self::WatchJobsStream>, Status> {
        use api::watch_jobs_request::FilterOneof;
        let span = rpc_span("WatchJobs");
        let caller = self.caller(&request).instrument(span.clone()).await?;
        let message = request.get_ref();
        let filter = match message.filter() {
            FilterOneof::JobId(id) => JobFilter::Job(id.to_string()),
            FilterOneof::Origin(origin) => JobFilter::Origin(origin.to_string()),
            FilterOneof::Hosts(selector) => {
                JobFilter::Hosts(self.view.selected(&selector_from(selector)))
            }
            _ => JobFilter::Hosts(None),
        };
        let hours = if message.since_hours() == 0 {
            24
        } else {
            message.since_hours().min(24 * 62)
        };
        let since = chrono::Utc::now() - chrono::Duration::hours(i64::from(hours));
        let (tx, rx) = mpsc::channel::<Result<api::JobEvent, Status>>(512);
        let view = self.view.clone();
        let api = ApiRef { view: view.clone() };
        tokio::spawn(until_shutdown(
            self.shutdown.clone(),
            async move {
                let mut changes = view.job_changes.subscribe();
                let mut authorizer = view.authorizer();
                let mut cache = DecisionCache::default();
                // snapshot from the current and previous month
                let now = chrono::Utc::now();
                for month in [month_offset(now, 0), month_offset(now, -1)] {
                    let db = jobs_db(&month);
                    let docs: Vec<JobDoc> = match &filter {
                        JobFilter::Job(id) => view
                            .couch
                            .get::<JobDoc>(&db, &job_doc_id(id))
                            .await
                            .ok()
                            .flatten()
                            .map(|s| vec![s.doc])
                            .unwrap_or_default(),
                        JobFilter::Origin(origin) => view
                            .couch
                            .view::<Value, Value>(
                                &db,
                                "regency",
                                "by_origin",
                                &[
                                    ("startkey", json!([origin, since])),
                                    ("endkey", json!([origin, {}])),
                                    ("include_docs", json!(true)),
                                    ("limit", json!(5000)),
                                ],
                            )
                            .await
                            .unwrap_or_default()
                            .into_iter()
                            .filter_map(|row| row.doc.and_then(|d| serde_json::from_value(d).ok()))
                            .collect(),
                        JobFilter::Hosts(_) => view
                            .couch
                            .find::<JobDoc>(
                                &db,
                                json!({"type": "job", "created_at": {"$gte": since}}),
                                5000,
                                None,
                                None,
                            )
                            .await
                            .map(|(docs, _)| docs.into_iter().map(|s| s.doc).collect())
                            .unwrap_or_default(),
                    };
                    for doc in docs {
                        if doc.created_at < since
                            || !filter.matches(&doc)
                            || !api.readable(&caller, &mut cache, &authorizer, &doc.host_id)
                        {
                            continue;
                        }
                        let mut event = api::JobEvent::new();
                        event.set_upsert(job_summary(&doc));
                        if tx.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                }
                let mut done = api::JobEvent::new();
                done.set_snapshot_done(true);
                if tx.send(Ok(done)).await.is_err() {
                    return;
                }
                let mut refresh = tokio::time::interval(Duration::from_secs(30));
                refresh.tick().await;
                loop {
                    let change = tokio::select! {
                        _ = tx.closed() => return,
                        _ = refresh.tick() => {
                            authorizer = view.authorizer();
                            cache = DecisionCache::default();
                            continue;
                        }
                        change = changes.recv() => change,
                    };
                    let doc = match change {
                        Ok(doc) => doc,
                        Err(broadcast::error::RecvError::Lagged(missed)) => {
                            warn!(missed, "slow live-view client, some job updates skipped");
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    };
                    if !filter.matches(&doc)
                        || !api.readable(&caller, &mut cache, &authorizer, &doc.host_id)
                    {
                        continue;
                    }
                    let mut event = api::JobEvent::new();
                    event.set_upsert(job_summary(&doc));
                    if tx.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            }
            .instrument(span),
        ));
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }

    async fn list_outcomes(
        &self,
        request: Request<api::ListOutcomesRequest>,
    ) -> Result<Response<api::ListOutcomesResponse>, Status> {
        use api::list_outcomes_request::FilterOneof;
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let message = request.get_ref();
            let (design_view, key) = match message.filter() {
                FilterOneof::HostId(host) => {
                    self.allow(&caller, "read", &self.host_resource(&host.to_string()))?;
                    ("by_host", host.to_string())
                }
                FilterOneof::Origin(origin) => ("by_origin", origin.to_string()),
                _ => return Err(Status::invalid_argument("host_id or origin is required")),
            };
            let page_size = message.page_size().clamp(1, 500) as usize;
            // page token: "<month>:<offset>"
            let token = message.page_token().to_string();
            let (start_month, mut offset) = match token.split_once(':') {
                Some((m, o)) => (Some(m.to_string()), o.parse::<usize>().unwrap_or(0)),
                None => (None, 0),
            };
            let authorizer = self.view.authorizer();
            let mut cache = DecisionCache::default();
            let mut response = api::ListOutcomesResponse::new();
            let mut count = 0;
            let months = self.outcome_months().await?;
            let months = months
                .iter()
                .skip_while(|m| start_month.as_ref().is_some_and(|s| *m != s));
            'months: for month in months {
                loop {
                    let rows = self
                        .view
                        .couch
                        .view::<Value, Value>(
                            &outcomes_db(month),
                            "regency",
                            design_view,
                            &[
                                ("startkey", json!([key, {}])),
                                ("endkey", json!([key])),
                                ("descending", json!(true)),
                                ("include_docs", json!(true)),
                                ("skip", json!(offset)),
                                ("limit", json!(page_size)),
                            ],
                        )
                        .await
                        .map_err(internal)?;
                    let fetched = rows.len();
                    for row in rows {
                        offset += 1;
                        let Some(doc) = row
                            .doc
                            .and_then(|d| serde_json::from_value::<OutcomeDoc>(d).ok())
                        else {
                            continue;
                        };
                        if !caller.bootstrap
                            && !cache.allowed(
                                &authorizer,
                                &caller.principal,
                                &self.host_resource(&doc.host_id),
                            )
                        {
                            continue;
                        }
                        response.outcomes_mut().push(outcome_summary(&doc, month));
                        count += 1;
                        if count >= page_size {
                            response.set_next_page_token(format!("{month}:{offset}"));
                            break 'months;
                        }
                    }
                    if fetched < page_size {
                        break;
                    }
                }
                offset = 0;
            }
            Ok(Response::new(response))
        }
        .instrument(rpc_span("ListOutcomes"))
        .await
        .pipe(|r| self.done("ListOutcomes", started, r))
    }

    async fn get_outcome(
        &self,
        request: Request<api::GetOutcomeRequest>,
    ) -> Result<Response<api::GetOutcomeResponse>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let job_id = request.get_ref().job_id().to_string();
            require(&job_id, "job_id")?;
            let month = request.get_ref().month().to_string();
            let months = if month.is_empty() {
                self.outcome_months().await?
            } else {
                vec![month]
            };
            for month in months {
                let db = outcomes_db(&month);
                let Some(stored) = self
                    .view
                    .couch
                    .get::<OutcomeDoc>(&db, &outcome_doc_id(&job_id))
                    .await
                    .map_err(internal)?
                else {
                    continue;
                };
                self.allow(&caller, "read", &self.host_resource(&stored.doc.host_id))?;
                let bytes = self
                    .view
                    .couch
                    .get_attachment(&db, &outcome_doc_id(&job_id), OUTCOME_ATTACHMENT)
                    .await
                    .map_err(internal)?
                    .unwrap_or_default();
                let mut response = api::GetOutcomeResponse::new();
                response.set_summary(outcome_summary(&stored.doc, &month));
                response
                    .set_result_json(serde_json::to_string(&stored.doc.result).unwrap_or_default());
                response.set_result_proto(bytes);
                response.set_publisher(stored.doc.publisher.as_str());
                return Ok(Response::new(response));
            }
            Err(Status::not_found("no such outcome within retention"))
        }
        .instrument(rpc_span("GetOutcome"))
        .await
        .pipe(|r| self.done("GetOutcome", started, r))
    }

    async fn list_orders(
        &self,
        request: Request<api::ListOrdersRequest>,
    ) -> Result<Response<api::ListOrdersResponse>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let orders = self
                .view
                .couch
                .all_docs_prefix::<Order>(DB_ORDERS, "order:")
                .await
                .map_err(internal)?;
            let authorizer = self.view.authorizer();
            let mut response = api::ListOrdersResponse::new();
            for stored in orders {
                if caller.bootstrap
                    || authorizer.is_authorized(
                        &caller.principal,
                        "read",
                        &Resource::Order(stored.doc.order_id.clone()),
                    )
                {
                    response.orders_mut().push(order_to(
                        &stored.doc,
                        stored.rev.as_deref().unwrap_or_default(),
                    ));
                }
            }
            Ok(Response::new(response))
        }
        .instrument(rpc_span("ListOrders"))
        .await
        .pipe(|r| self.done("ListOrders", started, r))
    }

    async fn list_catalog(
        &self,
        request: Request<api::ListCatalogRequest>,
    ) -> Result<Response<api::ListCatalogResponse>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let source = request.get_ref().source_id().to_string();
            let kind = request.get_ref().r#type().to_string();
            let authorizer = self.view.authorizer();
            let mut items = self.view.catalog_items();
            items.sort_by(|a, b| a.0.cmp(&b.0));
            let mut response = api::ListCatalogResponse::new();
            for (doc_id, item) in items {
                if (!source.is_empty() && item.source_id() != source)
                    || (!kind.is_empty() && !doc_id.starts_with(&format!("{kind}:")))
                {
                    continue;
                }
                if !caller.bootstrap
                    && !authorizer.is_authorized(
                        &caller.principal,
                        "read",
                        &Self::catalog_resource(&item),
                    )
                {
                    continue;
                }
                let mut entry = api::CatalogEntry::new();
                entry.set_doc_id(doc_id);
                entry.set_item_json(serde_json::to_string(&item).map_err(internal)?);
                response.items_mut().push(entry);
            }
            Ok(Response::new(response))
        }
        .instrument(rpc_span("ListCatalog"))
        .await
        .pipe(|r| self.done("ListCatalog", started, r))
    }

    async fn get_effective_state(
        &self,
        request: Request<api::GetEffectiveStateRequest>,
    ) -> Result<Response<api::GetEffectiveStateResponse>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let host_id = request.get_ref().host_id().to_string();
            self.allow(&caller, "read", &self.host_resource(&host_id))?;
            let effective = self.view.host_meta(&host_id).effective;
            let mut response = api::GetEffectiveStateResponse::new();
            for layer in effective.layers() {
                let mut message = api::AssignmentLayer::new();
                message.set_assignment(layer.assignment.as_str());
                message.set_priority(layer.priority);
                message.set_expected_state(layer.expected_state.as_str());
                response.layers_mut().push(message);
            }
            match &effective {
                Effective::Ready {
                    state,
                    origins,
                    revision,
                    ..
                } => {
                    response.set_state_json(serde_json::to_string(state).map_err(internal)?);
                    response.set_revision(revision.as_str());
                    for origin in origins {
                        response.origins_mut().push(origin.as_str());
                    }
                }
                Effective::Invalid { reason, .. } => response.set_error(reason.as_str()),
                Effective::Unassigned => {}
            }
            Ok(Response::new(response))
        }
        .instrument(rpc_span("GetEffectiveState"))
        .await
        .pipe(|r| self.done("GetEffectiveState", started, r))
    }

    async fn get_attribute_schema(
        &self,
        request: Request<api::GetAttributeSchemaRequest>,
    ) -> Result<Response<api::GetAttributeSchemaResponse>, Status> {
        let started = Instant::now();
        async {
            self.caller(&request).await?;
            // regent-sdk's own schema (feature `schema`): the builder's forms
            // follow the SDK the runners use (D23 one-shot builder)
            static SCHEMA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
            let schema = SCHEMA.get_or_init(|| {
                serde_json::to_string(&regent_sdk::schema::attribute()).unwrap_or_default()
            });
            let mut response = api::GetAttributeSchemaResponse::new();
            response.set_schema_json(schema.as_str());
            response.set_sdk_version(regent_sdk::VERSION);
            Ok(Response::new(response))
        }
        .instrument(rpc_span("GetAttributeSchema"))
        .await
        .pipe(|r| self.done("GetAttributeSchema", started, r))
    }

    async fn convert_attributes(
        &self,
        request: Request<api::ConvertAttributesRequest>,
    ) -> Result<Response<api::ConvertAttributesResponse>, Status> {
        let started = Instant::now();
        async {
            self.caller(&request).await?;
            let message = request.get_ref();
            let input = match message.input() {
                api::convert_attributes_request::InputOneof::Yaml(yaml) => {
                    AttributesInput::Yaml(yaml.to_str().unwrap_or_default())
                }
                api::convert_attributes_request::InputOneof::Json(json) => {
                    AttributesInput::Json(json.to_str().unwrap_or_default())
                }
                _ => return Err(Status::invalid_argument("yaml or json is required")),
            };
            let mut response = api::ConvertAttributesResponse::new();
            match convert_attributes(input) {
                Ok((yaml, json)) => {
                    // D9: shown at once, refused at run time
                    let value: serde_json::Value = serde_json::from_str(&json).map_err(internal)?;
                    if let Err(reason) = reject_inline_secrets(&value) {
                        response.set_error(reason.as_str());
                    }
                    response.set_yaml(yaml.as_str());
                    response.set_json(json.as_str());
                }
                Err(error) => response.set_error(error.as_str()),
            }
            Ok(Response::new(response))
        }
        .instrument(rpc_span("ConvertAttributes"))
        .await
        .pipe(|r| self.done("ConvertAttributes", started, r))
    }

    async fn get_settings(
        &self,
        request: Request<api::GetSettingsRequest>,
    ) -> Result<Response<api::GetSettingsResponse>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            self.allow(&caller, "read", &Resource::Settings)?;
            let settings = self
                .view
                .couch
                .get::<SettingsDoc>(DB_SETTINGS, SETTINGS_DOC_ID)
                .await
                .map_err(internal)?
                .map(|s| s.doc)
                .unwrap_or_default();
            let mut response = api::GetSettingsResponse::new();
            response.set_settings_json(serde_json::to_string(&settings.values).map_err(internal)?);
            Ok(Response::new(response))
        }
        .instrument(rpc_span("GetSettings"))
        .await
        .pipe(|r| self.done("GetSettings", started, r))
    }

    async fn list_policies(
        &self,
        request: Request<api::ListPoliciesRequest>,
    ) -> Result<Response<api::ListPoliciesResponse>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let authorizer = self.view.authorizer();
            let mut response = api::ListPoliciesResponse::new();
            let builtins = TEMPLATES.iter().map(|(id, text)| {
                (
                    PolicyDoc {
                        policy_id: id.to_string(),
                        text: text.to_string(),
                        description: "shipped role template".into(),
                    },
                    true,
                )
            });
            let stored = self.view.policies().into_iter().map(|p| (p, false));
            for (policy, builtin) in builtins.chain(stored) {
                if caller.bootstrap
                    || authorizer.is_authorized(
                        &caller.principal,
                        "read",
                        &Resource::Policy(policy.policy_id.clone()),
                    )
                {
                    let mut message = api::Policy::new();
                    message.set_policy_id(policy.policy_id);
                    message.set_text(policy.text);
                    message.set_description(policy.description);
                    message.set_builtin(builtin);
                    response.policies_mut().push(message);
                }
            }
            Ok(Response::new(response))
        }
        .instrument(rpc_span("ListPolicies"))
        .await
        .pipe(|r| self.done("ListPolicies", started, r))
    }

    async fn list_users(
        &self,
        request: Request<api::ListUsersRequest>,
    ) -> Result<Response<api::ListUsersResponse>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            self.allow(&caller, "read", &Resource::Settings)?;
            let mut users = self.view.users();
            users.sort_by(|a, b| a.name.cmp(&b.name));
            let mut response = api::ListUsersResponse::new();
            for user in users {
                let mut message = api::User::new();
                message.set_name(user.name);
                message.set_service_account(user.account == AccountKind::ServiceAccount);
                for group in user.groups {
                    message.groups_mut().push(group);
                }
                message.set_disabled(user.disabled);
                message.set_bootstrap(user.bootstrap);
                for token in user.tokens {
                    message.token_ids_mut().push(token.token_id);
                }
                response.users_mut().push(message);
            }
            Ok(Response::new(response))
        }
        .instrument(rpc_span("ListUsers"))
        .await
        .pipe(|r| self.done("ListUsers", started, r))
    }

    async fn run_now(
        &self,
        request: Request<api::RunNowRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let message = request.get_ref();
            let job = job_from(message.job())?;
            let selector: HostSelector = selector_from(message.selector());
            let pool = message.pool().to_string();
            if selector.is_empty() {
                return Err(Status::invalid_argument("the host selector is empty"));
            }
            // D23: a one-shot raw state, or each host's effective state
            let yaml = message.one_shot_yaml().to_string();
            let one_shot = if yaml.trim().is_empty() {
                None
            } else {
                let state = one_shot_from_yaml(&yaml).map_err(|e| {
                    Status::invalid_argument(format!("not an attribute (YAML): {e}"))
                })?;
                if state.attributes.is_empty() {
                    return Err(Status::invalid_argument("no attribute to run"));
                }
                Some(state)
            };
            // D16: Cedar checks the resolved hosts
            let (hostdefs, inventories) = self.view.hostdefs();
            let resolution = resolve(&selector, &hostdefs, &inventories, None);
            if resolution.targets.is_empty() {
                return Err(Status::failed_precondition(
                    "the selector matches no runnable host",
                ));
            }
            if one_shot.is_none() {
                let assignments = self.view.assignments();
                let ready = resolution.targets.iter().any(|t| {
                    matches!(
                        assignments.effective(&t.host_id, &t.inventories, &t.labels),
                        Effective::Ready { .. }
                    )
                });
                if !ready {
                    return Err(Status::failed_precondition(
                        "no selected host has a valid assignment (D23): nothing to run",
                    ));
                }
            }
            let action = job_action(job);
            let authorizer = self.view.authorizer();
            let resources = if caller.bootstrap {
                Default::default()
            } else {
                Resource::hosts(&hostdefs)
            };
            let denied: Vec<&str> = resolution
                .targets
                .iter()
                .filter(|t| {
                    !caller.bootstrap
                        && !authorizer.is_authorized(
                            &caller.principal,
                            action,
                            &resources
                                .get(&t.host_id)
                                .cloned()
                                .unwrap_or_else(|| Resource::host_of(&t.host_id, [])),
                        )
                })
                .map(|t| t.host_id.as_str())
                .collect();
            if !denied.is_empty() {
                return Err(Status::permission_denied(format!(
                    "not allowed to {action} {denied:?}"
                )));
            }
            let body = CommandBody::Run(RunCommand {
                selector,
                one_shot,
                job,
                pool: (!pool.is_empty()).then_some(pool),
            });
            self.publish_command(&caller, &message.command_id().to_string(), body)
                .await
        }
        .instrument(rpc_span("RunNow"))
        .await
        .pipe(|r| self.done("RunNow", started, r))
    }

    async fn abort_job(
        &self,
        request: Request<api::AbortJobRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let command_id = request.get_ref().command_id().to_string();
            let job_id = request.get_ref().job_id().to_string();
            require(&command_id, "command_id")?;
            require(&job_id, "job_id")?;
            let job = self
                .find_job(&job_id)
                .await?
                .ok_or_else(|| Status::not_found("no such job"))?;
            self.allow(&caller, "abort", &self.host_resource(&job.host_id))?;
            // D7: append to regency.control, then ABORT_REQUIRED
            let mut guard = self.aborts.lock().await;
            if guard.is_none() {
                let environment = regencylib::control::environment(&self.rabbitmq, &self.secrets)
                    .await
                    .map_err(|e| Status::unavailable(e.to_string()))?;
                *guard = Some(
                    AbortPublisher::new(&environment)
                        .await
                        .map_err(|e| Status::unavailable(e.to_string()))?,
                );
            }
            let request = AbortRequest::new(&job_id, &caller.principal.display());
            if let Err(e) = guard.as_ref().expect("set above").publish(&request).await {
                *guard = None;
                return Err(Status::unavailable(format!(
                    "abort not recorded, retry: {e}"
                )));
            }
            drop(guard);
            let event = LifecycleEvent::new(
                &job_id,
                JobState::AbortRequired,
                job.created_at,
                &job.origin,
                &job.host_id,
                job.job,
            )
            .with_details(format!("by {}", caller.principal.display()));
            let publisher = self
                .publisher
                .wait(self.publish_wait)
                .await
                .ok_or_else(|| Status::unavailable("RabbitMQ unavailable"))?;
            publisher
                .publish_json(
                    EX_LIFECYCLE,
                    &event.routing_key(),
                    &event.to_bytes().map_err(internal)?,
                )
                .await
                .map_err(|e| Status::unavailable(e.to_string()))?;
            info!(job_id = %job_id, host_id = %job.host_id, "abort requested");
            let mut accepted = api::Accepted::new();
            accepted.set_command_id(command_id);
            Ok(Response::new(accepted))
        }
        .instrument(rpc_span("AbortJob"))
        .await
        .pipe(|r| self.done("AbortJob", started, r))
    }

    async fn upsert_order(
        &self,
        request: Request<api::UpsertOrderRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let message = request.get_ref();
            let order = order_from(
                message.order(),
                caller.principal.display(),
                caller.principal.groups.clone(),
            )?;
            order.validate().map_err(Status::invalid_argument)?;
            let assignment = assignment_doc_id(&order.assignment).unwrap_or_default();
            if self.view.catalog_item(&assignment).is_none() {
                return Err(Status::not_found(format!(
                    "unknown assignment {}",
                    order.assignment
                )));
            }
            self.allow(
                &caller,
                "order.upsert",
                &Resource::Order(order.order_id.clone()),
            )?;
            self.publish_command(
                &caller,
                &message.command_id().to_string(),
                CommandBody::OrderUpsert(order),
            )
            .await
        }
        .instrument(rpc_span("UpsertOrder"))
        .await
        .pipe(|r| self.done("UpsertOrder", started, r))
    }

    async fn delete_order(
        &self,
        request: Request<api::DeleteOrderRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let order_id = request.get_ref().order_id().to_string();
            require(&order_id, "order_id")?;
            self.allow(&caller, "order.delete", &Resource::Order(order_id.clone()))?;
            self.publish_command(
                &caller,
                &request.get_ref().command_id().to_string(),
                CommandBody::OrderDelete { order_id },
            )
            .await
        }
        .instrument(rpc_span("DeleteOrder"))
        .await
        .pipe(|r| self.done("DeleteOrder", started, r))
    }

    async fn set_settings(
        &self,
        request: Request<api::SetSettingsRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let message = request.get_ref();
            let key = message.key().to_string();
            require(&key, "key")?;
            let value: Value = serde_json::from_str(&message.value_json().to_string())
                .map_err(|e| Status::invalid_argument(format!("value_json: {e}")))?;
            if key == "retention_months" && value.as_i64().is_none_or(|m| m < 1) {
                return Err(Status::invalid_argument(
                    "retention_months must be an integer >= 1",
                ));
            }
            self.allow(&caller, "settings.set", &Resource::Settings)?;
            self.publish_command(
                &caller,
                &message.command_id().to_string(),
                CommandBody::SettingsSet { key, value },
            )
            .await
        }
        .instrument(rpc_span("SetSettings"))
        .await
        .pipe(|r| self.done("SetSettings", started, r))
    }

    async fn list_instances(
        &self,
        request: Request<api::ListInstancesRequest>,
    ) -> Result<Response<api::ListInstancesResponse>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            self.allow(&caller, "read", &Resource::Settings)?;
            let now = chrono::Utc::now();
            let mut instances = self.view.instances();
            instances.sort_by(|a, b| (&a.service, &a.instance).cmp(&(&b.service, &b.instance)));
            let mut response = api::ListInstancesResponse::new();
            for status in instances {
                let mut item = api::Instance::new();
                item.set_service(status.service.clone());
                item.set_instance(status.instance.clone());
                item.set_version(status.version.clone());
                item.set_sdk_version(status.sdk_version.clone());
                item.set_pool(status.pool.clone().unwrap_or_default());
                item.set_started_at(status.started_at.timestamp_millis());
                item.set_last_seen(status.at.timestamp_millis());
                item.set_ready(status.ready);
                item.set_gone(now - status.at > regencylib::central::INSTANCE_GONE_AFTER);
                item.set_security_internal(status.security_internal.into());
                item.set_security_external(status.security_external.into());
                item.set_config_error(status.config_error.clone().unwrap_or_default());
                item.set_restart_pending(status.restart_pending);
                item.set_status_json(serde_json::to_string(&status).map_err(internal)?);
                response.instances_mut().push(item);
            }
            Ok(Response::new(response))
        }
        .instrument(rpc_span("ListInstances"))
        .await
        .pipe(|r| self.done("ListInstances", started, r))
    }

    async fn get_service_config(
        &self,
        request: Request<api::GetServiceConfigRequest>,
    ) -> Result<Response<api::GetServiceConfigResponse>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            self.allow(&caller, "read", &Resource::Settings)?;
            let message = request.get_ref();
            let service = message.service().to_string();
            let pool = Some(message.pool().to_string()).filter(|p| !p.is_empty());
            let id = regencylib::central::config_doc_id(&service, pool.as_deref());
            let stored = self
                .view
                .couch
                .get::<regencylib::central::ServiceConfigDoc>(regencylib::model::DB_CONFIG, &id)
                .await
                .or_else(|e| if e.is_not_found() { Ok(None) } else { Err(e) })
                .map_err(internal)?;
            let mut response = api::GetServiceConfigResponse::new();
            match stored {
                Some(stored) => {
                    response.set_settings_json(
                        serde_json::to_string(&stored.doc.settings).map_err(internal)?,
                    );
                    response.set_revision(stored.rev.unwrap_or_default());
                    response.set_updated_by(stored.doc.updated_by);
                    response.set_updated_at(
                        stored
                            .doc
                            .updated_at
                            .map(|t| t.timestamp_millis())
                            .unwrap_or(0),
                    );
                }
                None => response.set_settings_json("{}".to_string()),
            }
            Ok(Response::new(response))
        }
        .instrument(rpc_span("GetServiceConfig"))
        .await
        .pipe(|r| self.done("GetServiceConfig", started, r))
    }

    async fn set_service_config(
        &self,
        request: Request<api::SetServiceConfigRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let message = request.get_ref();
            let service = message.service().to_string();
            if !regencylib::central::SERVICES.contains(&service.as_str()) {
                return Err(Status::invalid_argument(format!(
                    "unknown service {service}"
                )));
            }
            let pool = Some(message.pool().to_string()).filter(|p| !p.is_empty());
            if pool.is_some() && service != "runner" {
                return Err(Status::invalid_argument(
                    "only runner configurations have a pool",
                ));
            }
            let settings: serde_json::Map<String, Value> =
                serde_json::from_str(&message.settings_json().to_string())
                    .map_err(|e| Status::invalid_argument(format!("settings_json: {e}")))?;
            let table =
                regencylib::central::json_to_table(&settings).map_err(Status::invalid_argument)?;
            self.allow(&caller, "config.set", &Resource::Settings)?;
            // D21: checked against every live instance's local layers
            let now = chrono::Utc::now();
            let live: Vec<_> = self
                .view
                .instances()
                .into_iter()
                .filter(|i| {
                    i.service == service
                        && now - i.at <= regencylib::central::INSTANCE_GONE_AFTER
                        && (pool.is_none() || i.pool == pool)
                })
                .collect();
            regencylib::central::validate(&service, &table, &live)
                .map_err(Status::failed_precondition)?;
            self.publish_command(
                &caller,
                &message.command_id().to_string(),
                CommandBody::ConfigSet {
                    service,
                    pool,
                    settings,
                },
            )
            .await
        }
        .instrument(rpc_span("SetServiceConfig"))
        .await
        .pipe(|r| self.done("SetServiceConfig", started, r))
    }

    async fn delete_host(
        &self,
        request: Request<api::DeleteHostRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let host_id = request.get_ref().host_id().to_string();
            require(&host_id, "host_id")?;
            self.allow(&caller, "host.delete", &self.host_resource(&host_id))?;
            self.publish_command(
                &caller,
                &request.get_ref().command_id().to_string(),
                CommandBody::HostDelete { host_id },
            )
            .await
        }
        .instrument(rpc_span("DeleteHost"))
        .await
        .pipe(|r| self.done("DeleteHost", started, r))
    }

    async fn approve_certificate(
        &self,
        request: Request<api::ApproveCertificateRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let message = request.get_ref();
            let host_id = message.host_id().to_string();
            let fingerprint = message.fingerprint().to_string();
            require(&host_id, "host_id")?;
            require(&fingerprint, "fingerprint")?;
            self.allow(&caller, "host.approve_cert", &self.host_resource(&host_id))?;
            self.publish_command(
                &caller,
                &message.command_id().to_string(),
                CommandBody::HostApproveCert {
                    host_id,
                    fingerprint,
                },
            )
            .await
        }
        .instrument(rpc_span("ApproveCertificate"))
        .await
        .pipe(|r| self.done("ApproveCertificate", started, r))
    }

    async fn upsert_catalog_item(
        &self,
        request: Request<api::UpsertCatalogItemRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let message = request.get_ref();
            let item: CatalogItem = serde_json::from_str(&message.item_json().to_string())
                .map_err(|e| Status::invalid_argument(format!("item_json: {e}")))?;
            reject_inline_secrets(&item).map_err(Status::invalid_argument)?;
            self.allow(&caller, "catalog.upsert", &Self::catalog_resource(&item))?;
            self.publish_command(
                &caller,
                &message.command_id().to_string(),
                CommandBody::CatalogUpsert(item),
            )
            .await
        }
        .instrument(rpc_span("UpsertCatalogItem"))
        .await
        .pipe(|r| self.done("UpsertCatalogItem", started, r))
    }

    async fn delete_catalog_item(
        &self,
        request: Request<api::DeleteCatalogItemRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let message = request.get_ref();
            let doc_id = message.doc_id().to_string();
            let source_id = message.source_id().to_string();
            let item = self
                .view
                .catalog_item(&doc_id)
                .ok_or_else(|| Status::not_found("no such catalog item"))?;
            self.allow(&caller, "catalog.delete", &Self::catalog_resource(&item))?;
            self.publish_command(
                &caller,
                &message.command_id().to_string(),
                CommandBody::CatalogDelete { doc_id, source_id },
            )
            .await
        }
        .instrument(rpc_span("DeleteCatalogItem"))
        .await
        .pipe(|r| self.done("DeleteCatalogItem", started, r))
    }

    async fn refresh_source(
        &self,
        request: Request<api::RefreshSourceRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let source_id = request.get_ref().source_id().to_string();
            require(&source_id, "source_id")?;
            self.allow(
                &caller,
                "source.refresh",
                &Resource::Source(source_id.clone()),
            )?;
            self.publish_command(
                &caller,
                &request.get_ref().command_id().to_string(),
                CommandBody::SourceRefresh { source_id },
            )
            .await
        }
        .instrument(rpc_span("RefreshSource"))
        .await
        .pipe(|r| self.done("RefreshSource", started, r))
    }

    async fn upsert_user(
        &self,
        request: Request<api::UpsertUserRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            self.allow(&caller, "user.upsert", &Resource::Settings)?;
            let message = request.get_ref();
            let name = message.name().to_string();
            require(&name, "name")?;
            if name.contains([':', '.']) {
                return Err(Status::invalid_argument("names cannot contain ':' or '.'"));
            }
            let existing = self.view.user(&name);
            let password = message.password().to_string();
            // D12: hashed here, never sent in clear over RabbitMQ
            let password_hash = if password.is_empty() {
                existing.as_ref().and_then(|u| u.password_hash.clone())
            } else {
                if password.len() < 12 {
                    return Err(Status::invalid_argument(
                        "passwords need at least 12 characters",
                    ));
                }
                Some(
                    tokio::task::spawn_blocking(move || hash_secret(&password))
                        .await
                        .map_err(internal)?
                        .map_err(internal)?,
                )
            };
            let user = UserDoc {
                name: name.clone(),
                account: if message.service_account() {
                    AccountKind::ServiceAccount
                } else {
                    AccountKind::User
                },
                password_hash,
                tokens: existing
                    .as_ref()
                    .map(|u| u.tokens.clone())
                    .unwrap_or_default(),
                groups: message.groups().iter().map(|g| g.to_string()).collect(),
                disabled: message.disabled(),
                bootstrap: existing.is_some_and(|u| u.bootstrap),
            };
            self.upsert_user(&caller, &message.command_id().to_string(), user)
                .await
        }
        .instrument(rpc_span("UpsertUser"))
        .await
        .pipe(|r| self.done("UpsertUser", started, r))
    }

    async fn delete_user(
        &self,
        request: Request<api::DeleteUserRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            self.allow(&caller, "user.delete", &Resource::Settings)?;
            let name = request.get_ref().name().to_string();
            if self.view.user(&name).is_some_and(|u| u.bootstrap) {
                return Err(Status::failed_precondition(
                    "the bootstrap admin can be disabled, not deleted",
                ));
            }
            self.publish_command(
                &caller,
                &request.get_ref().command_id().to_string(),
                CommandBody::UserDelete { name },
            )
            .await
        }
        .instrument(rpc_span("DeleteUser"))
        .await
        .pipe(|r| self.done("DeleteUser", started, r))
    }

    async fn create_api_token(
        &self,
        request: Request<api::CreateApiTokenRequest>,
    ) -> Result<Response<api::CreateApiTokenResponse>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            self.allow(&caller, "user.upsert", &Resource::Settings)?;
            let account = request.get_ref().account().to_string();
            let mut user = self
                .view
                .user(&account)
                .ok_or_else(|| Status::not_found("no such account"))?;
            let token_id = hex::encode(rand::random::<[u8; 4]>());
            let secret = base64::Engine::encode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                rand::random::<[u8; 32]>(),
            );
            let hashed = {
                let secret = secret.clone();
                tokio::task::spawn_blocking(move || hash_secret(&secret))
                    .await
                    .map_err(internal)?
                    .map_err(internal)?
            };
            user.tokens.push(TokenHash {
                token_id: token_id.clone(),
                hash: hashed,
            });
            self.upsert_user(&caller, &request.get_ref().command_id().to_string(), user)
                .await?;
            let mut response = api::CreateApiTokenResponse::new();
            response.set_token(format!("rgt_{account}.{token_id}.{secret}"));
            response.set_token_id(token_id);
            Ok(Response::new(response))
        }
        .instrument(rpc_span("CreateApiToken"))
        .await
        .pipe(|r| self.done("CreateApiToken", started, r))
    }

    async fn upsert_policy(
        &self,
        request: Request<api::UpsertPolicyRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let message = request.get_ref();
            let policy = PolicyDoc {
                policy_id: message.policy_id().to_string(),
                text: message.text().to_string(),
                description: message.description().to_string(),
            };
            require(&policy.policy_id, "policy_id")?;
            if policy.policy_id.starts_with("builtin-") {
                return Err(Status::invalid_argument("builtin-* ids are reserved"));
            }
            // D12: validated against the schema before acceptance
            Authorizer::validate_policy(&policy.policy_id, &policy.text)
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            self.allow(
                &caller,
                "policy.upsert",
                &Resource::Policy(policy.policy_id.clone()),
            )?;
            self.publish_command(
                &caller,
                &message.command_id().to_string(),
                CommandBody::PolicyUpsert(policy),
            )
            .await
        }
        .instrument(rpc_span("UpsertPolicy"))
        .await
        .pipe(|r| self.done("UpsertPolicy", started, r))
    }

    async fn delete_policy(
        &self,
        request: Request<api::DeletePolicyRequest>,
    ) -> Result<Response<api::Accepted>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let policy_id = request.get_ref().policy_id().to_string();
            self.allow(
                &caller,
                "policy.delete",
                &Resource::Policy(policy_id.clone()),
            )?;
            self.publish_command(
                &caller,
                &request.get_ref().command_id().to_string(),
                CommandBody::PolicyDelete { policy_id },
            )
            .await
        }
        .instrument(rpc_span("DeletePolicy"))
        .await
        .pipe(|r| self.done("DeletePolicy", started, r))
    }

    async fn issue_enrollment_token(
        &self,
        request: Request<api::IssueEnrollmentTokenRequest>,
    ) -> Result<Response<api::IssueEnrollmentTokenResponse>, Status> {
        let started = Instant::now();
        async {
            let caller = self.caller(&request).await?;
            let keys = self.enrollment_keys.as_ref().ok_or_else(|| {
                Status::failed_precondition("backend.enrollment_key is not configured")
            })?;
            let host_id = request.get_ref().host_id().to_string();
            require(&host_id, "host_id")?;
            self.allow(&caller, "host.enroll", &self.host_resource(&host_id))?;
            let ttl = match request.get_ref().ttl_sec() {
                0 => 3600,
                ttl => i64::from(ttl.min(7 * 86_400)),
            };
            let token = issue_enrollment(keys, &host_id, ttl, &caller.principal.display())
                .map_err(internal)?;
            info!(host_id = %host_id, ttl_sec = ttl, "enrollment token issued");
            let mut response = api::IssueEnrollmentTokenResponse::new();
            response.set_token(token);
            response.set_expires_at(chrono::Utc::now().timestamp() + ttl);
            Ok(Response::new(response))
        }
        .instrument(rpc_span("IssueEnrollmentToken"))
        .await
        .pipe(|r| self.done("IssueEnrollmentToken", started, r))
    }
}

/// What streaming tasks need of the API (owned, `'static`).
struct ApiRef {
    view: Arc<View>,
}

impl ApiRef {
    fn readable(
        &self,
        caller: &Caller,
        cache: &mut DecisionCache,
        authorizer: &Authorizer,
        host_id: &str,
    ) -> bool {
        caller.bootstrap
            || cache.allowed(
                authorizer,
                &caller.principal,
                &Resource::host(host_id, &self.view.hostdefs().0),
            )
    }
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}

impl<T> Pipe for T {}
