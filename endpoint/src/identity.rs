//! Client certificate → host identity (D5, D13).
//!
//! The TLS layer accepts any client certificate whose key the client proves
//! to hold; trust is decided here, per request, so that the three modes can
//! coexist on one port:
//! - chains to the external CA bundle → `ExternalCa`;
//! - chains to the built-in CA → `BuiltInCa` (may renew);
//! - otherwise, if pinning is enabled → `SelfSigned` (checked against pins).

use std::sync::Arc;

use rustls::RootCertStore;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer, X509Certificate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    ExternalCa,
    BuiltInCa,
    SelfSigned,
    /// Security level 0 (D19): accepted without any verification.
    Unverified,
}

#[derive(Debug, Clone)]
pub struct Identity {
    pub host_id: String,
    pub trust: Trust,
    /// SHA-256 of the DER certificate, `AB:CD:…` (same format as regops).
    pub fingerprint: String,
    /// Serial number, lowercase hex.
    pub serial: String,
}

/// TLS-level verifier: proof of possession only.
#[derive(Debug)]
pub struct AnyClientCert {
    algorithms: WebPkiSupportedAlgorithms,
}

impl AnyClientCert {
    pub fn new(provider: &CryptoProvider) -> Self {
        Self {
            algorithms: provider.signature_verification_algorithms,
        }
    }
}

impl ClientCertVerifier for AnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }
    fn client_auth_mandatory(&self) -> bool {
        // Enroll is called without a certificate
        false
    }
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Request-level chain verification.
pub struct Verifiers {
    external: Option<Arc<dyn ClientCertVerifier>>,
    builtin: Option<Arc<dyn ClientCertVerifier>>,
    pinning: bool,
    /// Level 0: accept any certificate.
    accept_any: bool,
}

fn verifier(
    certs: Vec<CertificateDer<'static>>,
    provider: Arc<CryptoProvider>,
) -> anyhow::Result<Arc<dyn ClientCertVerifier>> {
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots.add(cert)?;
    }
    Ok(WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider).build()?)
}

impl Verifiers {
    pub fn new(
        external_bundle: Option<Vec<CertificateDer<'static>>>,
        builtin_ca: Option<CertificateDer<'static>>,
        pinning: bool,
        accept_any: bool,
        provider: Arc<CryptoProvider>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            external: external_bundle
                .map(|certs| verifier(certs, provider.clone()))
                .transpose()?,
            builtin: builtin_ca
                .map(|ca| verifier(vec![ca], provider))
                .transpose()?,
            pinning,
            accept_any,
        })
    }

    /// Identity of a peer chain (end entity first); `Err` = not trusted.
    pub fn identify(&self, chain: &[CertificateDer<'static>]) -> Result<Identity, String> {
        let end = chain.first().ok_or("no client certificate")?;
        let intermediates = &chain[1..];
        let now = UnixTime::now();
        let trust = if self
            .builtin
            .as_ref()
            .is_some_and(|v| v.verify_client_cert(end, intermediates, now).is_ok())
        {
            Trust::BuiltInCa
        } else if self
            .external
            .as_ref()
            .is_some_and(|v| v.verify_client_cert(end, intermediates, now).is_ok())
        {
            Trust::ExternalCa
        } else if self.pinning {
            Trust::SelfSigned
        } else if self.accept_any {
            Trust::Unverified
        } else {
            return Err("client certificate is not issued by a trusted CA".to_string());
        };
        let (host_id, serial) = names(end)?;
        if trust == Trust::SelfSigned {
            check_validity(end)?;
        }
        Ok(Identity {
            host_id,
            trust,
            fingerprint: fingerprint(end),
            serial,
        })
    }
}

/// `AB:CD:…` SHA-256 of a DER certificate.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// (host id, serial): the first DNS SAN, else the CN.
fn names(der: &[u8]) -> Result<(String, String), String> {
    let (_, cert) = X509Certificate::from_der(der)
        .map_err(|e| format!("unreadable client certificate: {e}"))?;
    let serial = cert
        .raw_serial_as_string()
        .replace(':', "")
        .to_ascii_lowercase();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for name in &san.value.general_names {
            if let GeneralName::DNSName(dns) = name {
                return Ok((dns.to_string(), serial));
            }
        }
    }
    cert.subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .map(|cn| (cn.to_string(), serial))
        .ok_or_else(|| "client certificate names no host (no DNS SAN, no CN)".to_string())
}

fn check_validity(der: &[u8]) -> Result<(), String> {
    let (_, cert) = X509Certificate::from_der(der).map_err(|e| e.to_string())?;
    if cert.validity().is_valid() {
        Ok(())
    } else {
        Err("client certificate is expired or not yet valid".to_string())
    }
}

/// Load a PEM file of certificates.
pub fn load_certs(path: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let raw = std::fs::read(path).map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut raw.as_slice()).collect::<Result<_, _>>()?;
    anyhow::ensure!(!certs.is_empty(), "no certificate in {path}");
    Ok(certs)
}
