//! First-start preparation of the all-in-one image: random secrets (service
//! account passwords, session and enrollment keys, bootstrap admin password)
//! and the endpoint's TLS material (a built-in CA and a server certificate
//! signed by it). Existing files are never overwritten, so a volume on the
//! state directory keeps them across restarts.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use rand::RngCore;
use serde::Deserialize;
use tracing::{info, warn};

/// `[prepare]` section of `aio.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepare {
    /// Directory of the secret files (one secret per file, mode 0600).
    pub secrets_dir: Option<String>,
    /// Secrets generated at random when missing.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Secrets taken from an environment variable when it is set (else
    /// generated at random): name → variable.
    #[serde(default)]
    pub from_env: BTreeMap<String, String>,
    /// Secrets whose generated value is logged once (e.g. the bootstrap
    /// admin's initial password).
    #[serde(default)]
    pub announce: Vec<String>,
    pub tls: Option<TlsPrepare>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsPrepare {
    pub dir: String,
    /// DNS names of the endpoint's server certificate.
    pub names: Vec<String>,
    /// D25: also put this machine's hostname, FQDN and IP addresses in it,
    /// so that regops agents on other machines can reach it by any of them.
    #[serde(default)]
    pub local_names: bool,
    #[serde(default = "default_days")]
    pub days: u32,
}

fn default_days() -> u32 {
    825
}

fn write_private(path: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(content)
}

fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn run(prepare: &Prepare) -> Result<(), String> {
    if let Some(dir) = &prepare.secrets_dir {
        let names = prepare.secrets.iter().chain(prepare.from_env.keys());
        for name in names {
            let path = PathBuf::from(dir).join(name);
            if path.exists() {
                continue;
            }
            let from_env = prepare
                .from_env
                .get(name)
                .and_then(|var| std::env::var(var).ok())
                .filter(|v| !v.is_empty());
            let generated = from_env.is_none();
            let value = from_env.unwrap_or_else(random_secret);
            write_private(&path, value.as_bytes())
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            if generated && prepare.announce.contains(name) {
                warn!(
                    secret = %name,
                    file = %path.display(),
                    value = %value,
                    "generated at first start (shown once): change it after logging in"
                );
            } else {
                info!(secret = %name, file = %path.display(), from_env = !generated, "secret prepared");
            }
        }
    }
    if let Some(tls) = &prepare.tls {
        prepare_tls(tls)?;
    }
    Ok(())
}

fn prepare_tls(tls: &TlsPrepare) -> Result<(), String> {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
    let dir = PathBuf::from(&tls.dir);
    let (ca_cert, ca_key) = (dir.join("ca.pem"), dir.join("ca.key"));
    let (cert, key) = (dir.join("endpoint.pem"), dir.join("endpoint.key"));
    if ca_cert.exists() && cert.exists() {
        return Ok(());
    }
    let err = |e: rcgen::Error| e.to_string();
    let now = time::OffsetDateTime::now_utc();
    let ca_keypair = KeyPair::generate().map_err(err)?;
    let mut ca_params = CertificateParams::default();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "regency all-in-one CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca_params.not_before = now - time::Duration::hours(1);
    ca_params.not_after = now + time::Duration::days(i64::from(tls.days));
    let ca = ca_params.self_signed(&ca_keypair).map_err(err)?;

    let mut names = tls.names.clone();
    if tls.local_names {
        for name in local_names() {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    let server_keypair = KeyPair::generate().map_err(err)?;
    // names parsing as IP addresses become IP SANs
    let mut params = CertificateParams::new(names.clone()).map_err(err)?;
    params.distinguished_name.push(
        DnType::CommonName,
        names.first().cloned().unwrap_or_default(),
    );
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + time::Duration::days(i64::from(tls.days));
    let ca_key_pem = ca_keypair.serialize_pem();
    let issuer = rcgen::Issuer::from_ca_cert_pem(&ca.pem(), ca_keypair).map_err(err)?;
    let server = params.signed_by(&server_keypair, &issuer).map_err(err)?;

    for (path, content, private) in [
        (&ca_cert, ca.pem(), false),
        (&ca_key, ca_key_pem, true),
        (&cert, server.pem(), false),
        (&key, server_keypair.serialize_pem(), true),
    ] {
        let result = if private {
            write_private(path, content.as_bytes())
        } else {
            std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(path, content.as_bytes()))
        };
        result.map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    info!(dir = %dir.display(), names = ?names, "endpoint TLS material generated (built-in CA)");
    Ok(())
}

/// This machine's hostname, FQDN and IP addresses (best effort: what cannot
/// be found is left out).
fn local_names() -> Vec<String> {
    let mut names = Vec::new();
    let mut add = |name: &str| {
        let name = name.trim().trim_end_matches('.').to_lowercase();
        let valid = !name.is_empty()
            && name != "localhost"
            && !name.starts_with("127.")
            && name != "::1"
            && (name.parse::<std::net::IpAddr>().is_ok()
                || name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'));
        if valid && !names.contains(&name) {
            names.push(name);
        }
    };
    let command = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default()
    };
    if let Ok(hostname) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        add(&hostname);
    }
    add(&command("hostname", &[]));
    add(&command("hostname", &["--fqdn"]));
    // every address of the host (Linux), else the one of the default route
    let all = command("hostname", &["-I"]);
    if all.trim().is_empty() {
        if let Some(ip) = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|socket| {
                socket.connect("192.0.2.1:9")?;
                socket.local_addr()
            })
            .ok()
            .map(|address| address.ip())
            .filter(|ip| !ip.is_unspecified())
        {
            add(&ip.to_string());
        }
    } else {
        for ip in all.split_whitespace() {
            // skip link-local IPv6 (needs a zone, useless in a URL)
            if !ip.to_lowercase().starts_with("fe80") {
                add(ip);
            }
        }
    }
    names
}

/// `${NAME}` and `${NAME:-default}` from the supervisor's environment.
pub fn expand(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &rest[start + 2..start + end];
        let (name, default) = match inner.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (inner, None),
        };
        let value = std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| default.map(str::to_string))
            .unwrap_or_default();
        out.push_str(&value);
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion_with_defaults() {
        // SAFETY: test-local variable names
        unsafe {
            std::env::set_var("AIO_TEST_SET", "x");
            std::env::remove_var("AIO_TEST_UNSET");
        }
        assert_eq!(expand("a${AIO_TEST_SET}b"), "axb");
        assert_eq!(expand("${AIO_TEST_UNSET:-d}"), "d");
        assert_eq!(expand("${AIO_TEST_UNSET}"), "");
        assert_eq!(expand("plain"), "plain");
        assert_eq!(expand("${broken"), "${broken");
    }

    #[test]
    fn secrets_and_tls_are_generated_once() {
        let dir = std::env::temp_dir().join(format!("aio-prepare-{}", random_secret()));
        let prepare = Prepare {
            secrets_dir: Some(dir.join("secrets").display().to_string()),
            secrets: vec!["a".into()],
            from_env: BTreeMap::new(),
            announce: vec![],
            tls: Some(TlsPrepare {
                dir: dir.join("tls").display().to_string(),
                names: vec!["localhost".into(), "regency".into()],
                local_names: true,
                days: 30,
            }),
        };
        run(&prepare).unwrap();
        let first = std::fs::read_to_string(dir.join("secrets/a")).unwrap();
        assert_eq!(first.len(), 64);
        run(&prepare).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("secrets/a")).unwrap(),
            first
        );
        let pem = std::fs::read_to_string(dir.join("tls/endpoint.pem")).unwrap();
        assert!(pem.contains("BEGIN CERTIFICATE"));
        // the configured names come first, then this machine's
        let (_, der) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
        let cert = der.parse_x509().unwrap();
        let sans: Vec<String> = cert
            .subject_alternative_name()
            .unwrap()
            .unwrap()
            .value
            .general_names
            .iter()
            .map(|n| n.to_string())
            .collect();
        assert!(
            sans[0].contains("localhost") && sans[1].contains("regency"),
            "{sans:?}"
        );
        assert!(sans.len() >= 2 + local_names().len().min(1), "{sans:?}");
        assert!(dir.join("tls/ca.key").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
