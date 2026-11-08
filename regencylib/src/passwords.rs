//! Password and token hashing (D12): argon2id, PHC strings. Hashing happens
//! in the component that receives the secret (backend, regencyctl), never
//! after RabbitMQ.

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

pub fn hash(secret: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

pub fn verify(secret: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(secret.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let hash = super::hash("s3cret").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(super::verify("s3cret", &hash));
        assert!(!super::verify("nope", &hash));
    }
}
