//! Built-in CA (D13): signs short-lived client certificates for enrolled
//! hosts. The CA key is shared by every endpoint replica (secret reference).

use rcgen::{
    CertificateSigningRequestParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType, SerialNumber,
};
use rustls::pki_types::{CertificateDer, CertificateSigningRequestDer};

pub struct BuiltInCa {
    issuer: Issuer<'static, KeyPair>,
    ca_pem: String,
    ca_der: CertificateDer<'static>,
    lifetime_days: u32,
}

impl BuiltInCa {
    pub fn new(ca_pem: &str, key_pem: &str, lifetime_days: u32) -> anyhow::Result<Self> {
        let key = KeyPair::from_pem(key_pem)?;
        let issuer = Issuer::from_ca_cert_pem(ca_pem, key)?;
        let ca_der = rustls_pemfile::certs(&mut ca_pem.as_bytes())
            .next()
            .ok_or_else(|| anyhow::anyhow!("no certificate in the CA PEM"))??;
        Ok(Self {
            issuer,
            ca_pem: ca_pem.to_string(),
            ca_der,
            lifetime_days,
        })
    }

    pub fn ca_der(&self) -> CertificateDer<'static> {
        self.ca_der.clone()
    }

    /// Sign `csr_der` if its only name is `host_id`; returns cert + CA, PEM.
    pub fn sign(&self, csr_der: &[u8], host_id: &str) -> Result<String, String> {
        let der = CertificateSigningRequestDer::from(csr_der.to_vec());
        let mut csr = CertificateSigningRequestParams::from_der(&der)
            .map_err(|e| format!("invalid certificate signing request: {e}"))?;
        let names: Vec<String> = csr
            .params
            .subject_alt_names
            .iter()
            .map(|san| match san {
                SanType::DnsName(name) => name.to_string(),
                other => format!("{other:?}"),
            })
            .collect();
        if names != [host_id.to_string()] {
            return Err(format!(
                "the request must name exactly the host {host_id} as DNS SAN, it names {names:?}"
            ));
        }
        let now = time::OffsetDateTime::now_utc();
        let params = &mut csr.params;
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.not_before = now - time::Duration::minutes(5);
        params.not_after = now + time::Duration::days(i64::from(self.lifetime_days));
        params.serial_number = Some(SerialNumber::from_slice(&rand_serial()));
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, host_id);
        let cert = csr
            .signed_by(&self.issuer)
            .map_err(|e| format!("signing failed: {e}"))?;
        Ok(format!("{}{}", cert.pem(), self.ca_pem))
    }
}

fn rand_serial() -> [u8; 16] {
    let mut serial: [u8; 16] = rand::random();
    serial[0] &= 0x7f; // positive
    serial
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A throwaway CA (cert PEM, key PEM).
    pub fn test_ca() -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "regency test CA");
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    pub fn csr(host: &str) -> (Vec<u8>, String) {
        let key = KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec![host.to_string()]).unwrap();
        params.distinguished_name.push(DnType::CommonName, host);
        let csr = params.serialize_request(&key).unwrap();
        (csr.der().to_vec(), key.serialize_pem())
    }

    #[test]
    fn signs_only_the_bound_host() {
        let (ca_pem, ca_key) = test_ca();
        let ca = BuiltInCa::new(&ca_pem, &ca_key, 30).unwrap();
        let (der, _) = csr("web-01");
        let chain = ca.sign(&der, "web-01").unwrap();
        assert_eq!(chain.matches("BEGIN CERTIFICATE").count(), 2);
        assert!(ca.sign(&der, "web-02").unwrap_err().contains("web-02"));
    }
}
