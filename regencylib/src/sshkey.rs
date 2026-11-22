//! The runner's SSH identity (D25): an ed25519 key in OpenSSH format,
//! generated at startup when missing, whose public key the runner advertises
//! in its instance status so that hosts can authorize it.

use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::SigningKey;
use rand::RngCore;

const KEY_TYPE: &str = "ssh-ed25519";

fn put_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn public_blob(public: &[u8; 32]) -> Vec<u8> {
    let mut blob = Vec::new();
    put_string(&mut blob, KEY_TYPE.as_bytes());
    put_string(&mut blob, public);
    blob
}

/// A new ed25519 key: `(private key file content, public key line)`, both in
/// OpenSSH format (unencrypted private key, as `ssh-keygen -N ''` writes).
pub fn generate(comment: &str) -> (String, String) {
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let signing = SigningKey::from_bytes(&seed);
    let public = signing.verifying_key().to_bytes();
    let mut check = [0u8; 4];
    rand::rng().fill_bytes(&mut check);

    let mut secret = Vec::new();
    secret.extend_from_slice(&check);
    secret.extend_from_slice(&check);
    put_string(&mut secret, KEY_TYPE.as_bytes());
    put_string(&mut secret, &public);
    let mut pair = seed.to_vec();
    pair.extend_from_slice(&public);
    put_string(&mut secret, &pair);
    put_string(&mut secret, comment.as_bytes());
    let mut pad = 1u8;
    while secret.len() % 8 != 0 {
        secret.push(pad);
        pad += 1;
    }

    let mut body = b"openssh-key-v1\0".to_vec();
    put_string(&mut body, b"none");
    put_string(&mut body, b"none");
    put_string(&mut body, b"");
    body.extend_from_slice(&1u32.to_be_bytes());
    put_string(&mut body, &public_blob(&public));
    put_string(&mut body, &secret);

    let encoded = STANDARD.encode(&body);
    let mut private = String::from("-----BEGIN OPENSSH PRIVATE KEY-----\n");
    for chunk in encoded.as_bytes().chunks(70) {
        private.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        private.push('\n');
    }
    private.push_str("-----END OPENSSH PRIVATE KEY-----\n");
    let line = format!(
        "{KEY_TYPE} {} {comment}",
        STANDARD.encode(public_blob(&public))
    );
    (private, line)
}

fn public_path(key: &Path) -> PathBuf {
    let mut name = key.as_os_str().to_owned();
    name.push(".pub");
    PathBuf::from(name)
}

/// What [`ensure`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Ensured {
    /// A new key was written.
    Generated(String),
    /// The key existed; its public key comes from `<key>.pub`.
    Existing(String),
    /// The key existed without `<key>.pub`: nothing to advertise.
    NoPublicKey,
}

/// Generate the key at `path` (mode 0600, and `<path>.pub`) unless it
/// exists; return its public key line.
pub fn ensure(path: &Path, comment: &str) -> std::io::Result<Ensured> {
    let public = public_path(path);
    if path.exists() {
        return match std::fs::read_to_string(&public) {
            Ok(line) => Ok(Ensured::Existing(line.trim().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ensured::NoPublicKey),
            Err(e) => Err(e),
        };
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let (private, line) = generate(comment);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(private.as_bytes())?;
    std::fs::write(&public, format!("{line}\n"))?;
    Ok(Ensured::Generated(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("regency-sshkey-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn generated_once_and_kept() {
        let dir = temp_dir("once");
        let key = dir.join("secrets/runner-ssh-key");
        let Ensured::Generated(line) = ensure(&key, "regency-runner-default").unwrap() else {
            panic!("expected a new key");
        };
        assert!(line.starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI"));
        assert!(line.ends_with(" regency-runner-default"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(ensure(&key, "other").unwrap(), Ensured::Existing(line));
        std::fs::remove_file(public_path(&key)).unwrap();
        assert_eq!(ensure(&key, "other").unwrap(), Ensured::NoPublicKey);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OpenSSH itself reads the key and derives the same public key.
    #[test]
    fn openssh_reads_it() {
        let Ok(output) = std::process::Command::new("ssh-keygen").arg("-?").output() else {
            eprintln!("ssh-keygen not installed: skipped");
            return;
        };
        drop(output);
        let dir = temp_dir("openssh");
        let key = dir.join("id");
        let Ensured::Generated(line) = ensure(&key, "test").unwrap() else {
            panic!("expected a new key");
        };
        let derived = std::process::Command::new("ssh-keygen")
            .arg("-y")
            .arg("-f")
            .arg(&key)
            .output()
            .unwrap();
        assert!(derived.status.success(), "{derived:?}");
        let derived = String::from_utf8(derived.stdout).unwrap();
        let fields = |l: &str| l.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        assert_eq!(fields(&derived), fields(&line));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
