//! The `regent.v1.Reporting` service (D5, D6, D13).

use std::sync::Arc;
use std::time::{Duration, Instant};

use protobuf::Serialize as _;
use regencylib::amqp::PublisherLink;
use regencylib::messages::{
    Command, CommandBody, ORIGIN_REGOPS, encode_outcome, outcome_envelope, outcome_routing_key,
};
use regencylib::tokens::{KeyRing, verify_enrollment};
use regencylib::topology::{EX_COMMANDS, EX_OUTCOMES};
use regent_sdk::proto::grpc::reporting::reporting_server::Reporting;
use regent_sdk::proto::{
    EnrollRequest, EnrollResponse, RenewCertificateRequest, RenewCertificateResponse,
    ReportOutcomeRequest, ReportOutcomeResponse,
};
use regent_sdk::task::RegentTaskResult;
use tonic::{Request, Response, Status};
use tracing::{Instrument, info, info_span, warn};

use crate::ca::BuiltInCa;
use crate::identity::{Identity, Trust, Verifiers};
use crate::trust::{Pin, TrustState};

pub struct ReportingService {
    pub verifiers: Verifiers,
    pub trust: Arc<TrustState>,
    pub ca: Option<Arc<BuiltInCa>>,
    pub enrollment_keys: Option<KeyRing>,
    pub trust_on_first_use: bool,
    pub publisher: PublisherLink,
    pub publish_wait: Duration,
    pub instance: String,
    pub outcomes: prometheus::IntCounterVec,
}

impl ReportingService {
    async fn authenticate<T>(&self, request: &Request<T>) -> Result<Identity, Status> {
        let chain = request
            .peer_certs()
            .ok_or_else(|| Status::unauthenticated("a client certificate is required"))?;
        let identity = self
            .verifiers
            .identify(&chain)
            .map_err(Status::unauthenticated)?;
        if let Some(reason) = self.trust.revoked(&identity) {
            return Err(Status::permission_denied(reason));
        }
        if identity.trust == Trust::SelfSigned {
            match self.trust.pin(&identity) {
                Pin::Pinned => {}
                Pin::Mismatch => {
                    warn!(host_id = %identity.host_id, fingerprint = %identity.fingerprint, "certificate differs from the pinned one");
                    return Err(Status::permission_denied(
                        "this certificate differs from the one pinned for this host",
                    ));
                }
                Pin::Unpinned { announce } => {
                    if announce {
                        self.announce_pending(&identity).await?;
                    }
                    if self.trust_on_first_use {
                        self.trust.pin_locally(&identity);
                    } else {
                        return Err(Status::permission_denied(
                            "self-signed certificate awaiting approval in regency",
                        ));
                    }
                }
            }
        }
        Ok(identity)
    }

    /// `host.cert_pending` command (D13), deduplicated by its id.
    async fn announce_pending(&self, identity: &Identity) -> Result<(), Status> {
        let command = Command::new(
            format!("cert:{}:{}", identity.host_id, identity.fingerprint),
            "service:endpoint",
            CommandBody::HostCertPending {
                host_id: identity.host_id.clone(),
                fingerprint: identity.fingerprint.clone(),
                auto_approve: self.trust_on_first_use,
            },
        );
        let result = async {
            let publisher = self
                .publisher
                .wait(self.publish_wait)
                .await
                .ok_or("RabbitMQ unavailable")?;
            let bytes = command.to_bytes().map_err(|e| e.to_string())?;
            publisher
                .publish_json(EX_COMMANDS, command.routing_key(), &bytes)
                .await
                .map_err(|e| e.to_string())
        }
        .await;
        match result {
            Ok(()) => {
                info!(host_id = %identity.host_id, fingerprint = %identity.fingerprint, auto_approve = self.trust_on_first_use, "new self-signed certificate announced");
                Ok(())
            }
            Err(e) => {
                self.trust.unannounce(identity);
                Err(Status::unavailable(format!(
                    "cannot record the certificate: {e}"
                )))
            }
        }
    }

    async fn report(
        &self,
        request: Request<ReportOutcomeRequest>,
    ) -> Result<Response<ReportOutcomeResponse>, Status> {
        let identity = self.authenticate(&request).await?;
        tracing::Span::current().record("host_id", identity.host_id.as_str());
        let message = request.into_inner();
        if !message.has_result() {
            return Err(Status::invalid_argument("missing result"));
        }
        // re-serializing keeps the fields this version does not know (D6)
        let result_bytes = message
            .result()
            .to_owned()
            .serialize()
            .map_err(|e| Status::invalid_argument(format!("cannot serialize result: {e:?}")))?;
        let result = RegentTaskResult::from_proto_bytes(&result_bytes)
            .map_err(|e| Status::invalid_argument(format!("invalid result: {e}")))?;
        tracing::Span::current().record("job_id", result.idempotency_key());
        if result.host_id() != identity.host_id {
            // D5: check, never rewrite
            self.outcomes.with_label_values(&["host_mismatch"]).inc();
            warn!(
                reported_host_id = result.host_id(),
                "outcome for another host refused"
            );
            return Err(Status::invalid_argument(format!(
                "outcome is for host {} but the certificate identifies {}",
                result.host_id(),
                identity.host_id
            )));
        }
        let reporter_bytes = if message.has_reporter() {
            message
                .reporter()
                .to_owned()
                .serialize()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let envelope = outcome_envelope(
            result_bytes,
            reporter_bytes,
            result.started_at(),
            ORIGIN_REGOPS,
            &format!("endpoint/{}", self.instance),
        );
        let bytes = encode_outcome(&envelope).map_err(|e| Status::internal(e.to_string()))?;
        let publisher = self
            .publisher
            .wait(self.publish_wait)
            .await
            .ok_or_else(|| Status::unavailable("RabbitMQ unavailable, retry later"))?;
        publisher
            .publish(
                EX_OUTCOMES,
                outcome_routing_key(result.job()),
                &bytes,
                lapin_properties(),
            )
            .await
            .map_err(|e| {
                self.outcomes.with_label_values(&["unavailable"]).inc();
                warn!(error = %e, "outcome publish failed");
                Status::unavailable(format!("cannot store the outcome now: {e}"))
            })?;
        self.outcomes.with_label_values(&["accepted"]).inc();
        info!(job = outcome_routing_key(result.job()), "outcome accepted");
        Ok(Response::new(ReportOutcomeResponse::new()))
    }

    fn ca(&self) -> Result<(&BuiltInCa, &KeyRing), Status> {
        match (&self.ca, &self.enrollment_keys) {
            (Some(ca), Some(keys)) => Ok((ca, keys)),
            _ => Err(Status::failed_precondition(
                "the built-in CA is not enabled on this endpoint",
            )),
        }
    }
}

fn lapin_properties() -> lapin::BasicProperties {
    lapin::BasicProperties::default().with_content_type("application/x-protobuf".into())
}

fn log_result<T>(rpc: &str, started: Instant, result: &Result<T, Status>) {
    let duration_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(_) => tracing::debug!(rpc, duration_ms, "rpc ok"),
        Err(status) => {
            warn!(rpc, duration_ms, code = ?status.code(), error = status.message(), "rpc refused")
        }
    }
}

#[tonic::async_trait]
impl Reporting for ReportingService {
    async fn report_outcome(
        &self,
        request: Request<ReportOutcomeRequest>,
    ) -> Result<Response<ReportOutcomeResponse>, Status> {
        let started = Instant::now();
        let span = info_span!("report_outcome", rpc = "ReportOutcome", peer = ?request.remote_addr(), host_id = tracing::field::Empty, job_id = tracing::field::Empty);
        async {
            let result = self.report(request).await;
            log_result("ReportOutcome", started, &result);
            result
        }
        .instrument(span)
        .await
    }

    async fn enroll(
        &self,
        request: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let started = Instant::now();
        let span = info_span!("enroll", rpc = "Enroll", peer = ?request.remote_addr(), host_id = tracing::field::Empty);
        async {
            let result = async {
                let (ca, keys) = self.ca()?;
                let message = request.into_inner();
                let host_id = verify_enrollment(keys, &message.enrollment_token().to_string())
                    .map_err(|e| {
                        Status::unauthenticated(format!("enrollment token refused: {e}"))
                    })?;
                tracing::Span::current().record("host_id", host_id.as_str());
                let revoked = self.trust.revoked(&Identity {
                    host_id: host_id.clone(),
                    trust: Trust::BuiltInCa,
                    fingerprint: String::new(),
                    serial: String::new(),
                });
                if let Some(reason) = revoked {
                    return Err(Status::permission_denied(reason));
                }
                let chain = ca
                    .sign(message.csr_der(), &host_id)
                    .map_err(Status::invalid_argument)?;
                info!("host enrolled");
                let mut response = EnrollResponse::new();
                response.set_certificate_chain_pem(chain);
                Ok(Response::new(response))
            }
            .await;
            log_result("Enroll", started, &result);
            result
        }
        .instrument(span)
        .await
    }

    async fn renew_certificate(
        &self,
        request: Request<RenewCertificateRequest>,
    ) -> Result<Response<RenewCertificateResponse>, Status> {
        let started = Instant::now();
        let span = info_span!("renew_certificate", rpc = "RenewCertificate", peer = ?request.remote_addr(), host_id = tracing::field::Empty);
        async {
            let result = async {
                let (ca, _) = self.ca()?;
                let identity = self.authenticate(&request).await?;
                tracing::Span::current().record("host_id", identity.host_id.as_str());
                if identity.trust != Trust::BuiltInCa {
                    return Err(Status::permission_denied(
                        "only certificates of the built-in CA are renewed here",
                    ));
                }
                let chain = ca
                    .sign(request.get_ref().csr_der(), &identity.host_id)
                    .map_err(Status::invalid_argument)?;
                info!("certificate renewed");
                let mut response = RenewCertificateResponse::new();
                response.set_certificate_chain_pem(chain);
                Ok(Response::new(response))
            }
            .await;
            log_result("RenewCertificate", started, &result);
            result
        }
        .instrument(span)
        .await
    }
}
