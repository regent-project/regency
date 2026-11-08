//! Signed tokens (HS256 JWT): enrollment tokens (D13, issued by backend,
//! checked by the endpoint) and backend session tokens (D12). Keys are shared
//! through secret references; key ids allow rotation.

use std::collections::BTreeMap;

use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, decode_header, encode,
};
use serde::{Deserialize, Serialize};

pub const ENROLL_AUDIENCE: &str = "regency-enroll";
pub const SESSION_AUDIENCE: &str = "regency-session";

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("invalid token: {0}")]
    Invalid(String),
    #[error("unknown key id {0:?}")]
    UnknownKey(Option<String>),
}

/// Keys by id; `current` signs, all verify.
#[derive(Clone)]
pub struct KeyRing {
    current: String,
    keys: BTreeMap<String, Vec<u8>>,
}

impl KeyRing {
    pub fn single(key: &[u8]) -> Self {
        Self {
            current: "k1".to_string(),
            keys: BTreeMap::from([("k1".to_string(), key.to_vec())]),
        }
    }

    pub fn new(current: &str, keys: BTreeMap<String, Vec<u8>>) -> Result<Self, TokenError> {
        if !keys.contains_key(current) {
            return Err(TokenError::UnknownKey(Some(current.to_string())));
        }
        Ok(Self {
            current: current.to_string(),
            keys,
        })
    }

    pub fn sign<C: Serialize>(&self, claims: &C) -> Result<String, TokenError> {
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(self.current.clone());
        encode(
            &header,
            claims,
            &EncodingKey::from_secret(&self.keys[&self.current]),
        )
        .map_err(|e| TokenError::Invalid(e.to_string()))
    }

    pub fn verify<C: for<'de> Deserialize<'de>>(
        &self,
        token: &str,
        audience: &str,
    ) -> Result<C, TokenError> {
        let header = decode_header(token).map_err(|e| TokenError::Invalid(e.to_string()))?;
        let key = header
            .kid
            .as_ref()
            .and_then(|kid| self.keys.get(kid))
            .ok_or_else(|| TokenError::UnknownKey(header.kid.clone()))?;
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_audience(&[audience]);
        validation.leeway = 30;
        decode::<C>(token, &DecodingKey::from_secret(key), &validation)
            .map(|data| data.claims)
            .map_err(|e| TokenError::Invalid(e.to_string()))
    }
}

/// Enrollment token claims: bound to one host id, short expiry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollClaims {
    pub sub: String,
    pub aud: String,
    pub exp: i64,
    pub iat: i64,
    /// Who issued it (audit).
    pub iss: String,
}

pub fn issue_enrollment(
    keys: &KeyRing,
    host_id: &str,
    ttl_sec: i64,
    issuer: &str,
) -> Result<String, TokenError> {
    let now = chrono::Utc::now().timestamp();
    keys.sign(&EnrollClaims {
        sub: host_id.to_string(),
        aud: ENROLL_AUDIENCE.to_string(),
        exp: now + ttl_sec,
        iat: now,
        iss: issuer.to_string(),
    })
}

/// The host id an enrollment token is bound to.
pub fn verify_enrollment(keys: &KeyRing, token: &str) -> Result<String, TokenError> {
    keys.verify::<EnrollClaims>(token, ENROLL_AUDIENCE)
        .map(|c| c.sub)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_tokens() {
        let keys = KeyRing::single(b"0123456789abcdef0123456789abcdef");
        let token = issue_enrollment(&keys, "web-01", 600, "User::\"admin\"").unwrap();
        assert_eq!(verify_enrollment(&keys, &token).unwrap(), "web-01");
        let other = KeyRing::single(b"another key another key another k");
        assert!(verify_enrollment(&other, &token).is_err());
        let expired = issue_enrollment(&keys, "web-01", -3600, "x").unwrap();
        assert!(verify_enrollment(&keys, &expired).is_err());
        // a session token is not an enrollment token
        let session = keys
            .sign(&EnrollClaims {
                sub: "a".into(),
                aud: SESSION_AUDIENCE.into(),
                exp: i64::MAX / 2,
                iat: 0,
                iss: "x".into(),
            })
            .unwrap();
        assert!(verify_enrollment(&keys, &session).is_err());
    }
}
