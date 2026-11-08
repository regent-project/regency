//! Authentication (D12): session tokens (issued by Login, signed with the
//! shared key), API tokens of local accounts (`rgt_…`, argon2id hashes in
//! CouchDB), and OIDC tokens of configured providers (JWKS).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header};
use regencylib::model::AccountKind;
use regencylib::policy::{Principal, PrincipalKind};
use regencylib::security::Level;
use regencylib::tokens::{KeyRing, SESSION_AUDIENCE};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tonic::Status;
use tracing::{info, warn};

use crate::config::OidcProvider;
use crate::view::View;

pub use regencylib::passwords::{hash as hash_secret, verify as verify_secret};

/// The authenticated caller.
#[derive(Debug, Clone)]
pub struct Caller {
    pub principal: Principal,
    pub bootstrap: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionClaims {
    sub: String,
    aud: String,
    exp: i64,
    iat: i64,
}

struct OidcState {
    config: OidcProvider,
    keys: Mutex<Option<(JwkSet, Instant)>>,
}

pub struct Authenticator {
    view: Arc<View>,
    sessions: KeyRing,
    session_ttl: i64,
    bootstrap_admin: String,
    oidc: Vec<OidcState>,
    http: reqwest::Client,
    /// sha256(api token) → (account, verified at): skips argon2 on every call
    verified_tokens: Mutex<HashMap<[u8; 32], (String, Instant)>>,
    /// D19 external level.
    level: Level,
}

const TOKEN_CACHE: Duration = Duration::from_secs(300);
const JWKS_TTL: Duration = Duration::from_secs(3600);

impl Authenticator {
    pub fn new(
        view: Arc<View>,
        sessions: KeyRing,
        session_ttl: i64,
        bootstrap_admin: String,
        oidc: Vec<OidcProvider>,
        level: Level,
    ) -> Self {
        Self {
            view,
            sessions,
            session_ttl,
            bootstrap_admin,
            oidc: oidc
                .into_iter()
                .map(|config| OidcState {
                    config,
                    keys: Mutex::new(None),
                })
                .collect(),
            http: reqwest::Client::new(),
            verified_tokens: Mutex::new(HashMap::new()),
            level,
        }
    }

    fn local_caller(&self, name: &str) -> Result<Caller, Status> {
        let user = self
            .view
            .user(name)
            .ok_or_else(|| Status::unauthenticated("unknown account"))?;
        if user.disabled {
            return Err(Status::permission_denied("account disabled"));
        }
        let kind = match user.account {
            AccountKind::User => PrincipalKind::User,
            AccountKind::ServiceAccount => PrincipalKind::ServiceAccount,
        };
        Ok(Caller {
            bootstrap: user.bootstrap
                && kind == PrincipalKind::User
                && name == self.bootstrap_admin,
            principal: Principal {
                kind,
                name: name.to_string(),
                groups: user.groups.into_iter().collect(),
            },
        })
    }

    /// Password login → session token.
    pub async fn login(&self, username: &str, password: &str) -> Result<(String, i64), Status> {
        // D19 level 3: passwords only as break-glass for the bootstrap admin
        if self.level >= Level::Hardened && username != self.bootstrap_admin {
            return Err(Status::permission_denied(
                "password login is disabled at security level 3: use OIDC or an API token",
            ));
        }
        let user = self.view.user(username);
        let hash = user
            .as_ref()
            .filter(|u| u.account == AccountKind::User && !u.disabled)
            .and_then(|u| u.password_hash.clone());
        let password = password.to_string();
        // always pay the hashing cost (no user enumeration by timing)
        let ok = tokio::task::spawn_blocking(move || match hash {
            Some(hash) => verify_secret(&password, &hash),
            None => {
                let _ = hash_secret(&password);
                false
            }
        })
        .await
        .unwrap_or(false);
        if !ok {
            warn!(user = username, "login refused");
            return Err(Status::unauthenticated("invalid username or password"));
        }
        let now = chrono::Utc::now().timestamp();
        let exp = now + self.session_ttl;
        let token = self
            .sessions
            .sign(&SessionClaims {
                sub: username.to_string(),
                aud: SESSION_AUDIENCE.to_string(),
                exp,
                iat: now,
            })
            .map_err(|e| Status::internal(e.to_string()))?;
        info!(user = username, "login");
        Ok((token, exp))
    }

    /// Authenticate the `authorization: Bearer …` metadata.
    pub async fn authenticate(
        &self,
        metadata: &tonic::metadata::MetadataMap,
    ) -> Result<Caller, Status> {
        let header = metadata.get("authorization").and_then(|v| v.to_str().ok());
        // D19 level 0: callers without credentials act as the bootstrap admin
        let Some(header) = header else {
            if self.level == Level::Open {
                return Ok(Caller {
                    principal: Principal::user(&self.bootstrap_admin, &[]),
                    bootstrap: true,
                });
            }
            return Err(Status::unauthenticated("missing authorization metadata"));
        };
        let token = header
            .strip_prefix("Bearer ")
            .or_else(|| header.strip_prefix("bearer "))
            .ok_or_else(|| Status::unauthenticated("expected a Bearer token"))?
            .trim();
        if let Some(rest) = token.strip_prefix("rgt_") {
            return self.api_token(token, rest).await;
        }
        if let Ok(claims) = self
            .sessions
            .verify::<SessionClaims>(token, SESSION_AUDIENCE)
        {
            return self.local_caller(&claims.sub);
        }
        self.oidc_token(token).await
    }

    /// `rgt_<account>.<token id>.<secret>`
    async fn api_token(&self, token: &str, rest: &str) -> Result<Caller, Status> {
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if let Some((account, at)) = self
            .verified_tokens
            .lock()
            .expect("tokens")
            .get(&digest)
            .cloned()
            && at.elapsed() < TOKEN_CACHE
        {
            return self.local_caller(&account);
        }
        let mut parts = rest.rsplitn(3, '.');
        let (Some(secret), Some(token_id), Some(account)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(Status::unauthenticated("malformed API token"));
        };
        let user = self
            .view
            .user(account)
            .ok_or_else(|| Status::unauthenticated("invalid API token"))?;
        let hash = user
            .tokens
            .iter()
            .find(|t| t.token_id == token_id)
            .map(|t| t.hash.clone())
            .ok_or_else(|| Status::unauthenticated("invalid API token"))?;
        let secret = secret.to_string();
        let ok = tokio::task::spawn_blocking(move || verify_secret(&secret, &hash))
            .await
            .unwrap_or(false);
        if !ok {
            return Err(Status::unauthenticated("invalid API token"));
        }
        self.verified_tokens
            .lock()
            .expect("tokens")
            .insert(digest, (account.to_string(), Instant::now()));
        self.local_caller(account)
    }

    async fn jwks(&self, provider: &OidcState, refresh: bool) -> Result<JwkSet, Status> {
        if !refresh
            && let Some((keys, at)) = provider.keys.lock().expect("jwks").as_ref()
            && at.elapsed() < JWKS_TTL
        {
            return Ok(keys.clone());
        }
        let fetch = async {
            let discovery: serde_json::Value = self
                .http
                .get(format!(
                    "{}/.well-known/openid-configuration",
                    provider.config.issuer.trim_end_matches('/')
                ))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let uri = discovery["jwks_uri"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            self.http
                .get(uri)
                .send()
                .await?
                .error_for_status()?
                .json::<JwkSet>()
                .await
        };
        let keys = fetch.await.map_err(|e| {
            warn!(issuer = %provider.config.issuer, error = %e, "cannot fetch OIDC keys");
            Status::unavailable("identity provider unreachable")
        })?;
        *provider.keys.lock().expect("jwks") = Some((keys.clone(), Instant::now()));
        Ok(keys)
    }

    async fn oidc_token(&self, token: &str) -> Result<Caller, Status> {
        let invalid = || Status::unauthenticated("invalid token");
        let header = decode_header(token).map_err(|_| invalid())?;
        // issuer from the (not yet verified) payload, to pick the provider
        let payload = token.split('.').nth(1).ok_or_else(invalid)?;
        let claims: serde_json::Value =
            base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload)
                .ok()
                .and_then(|raw| serde_json::from_slice(&raw).ok())
                .ok_or_else(invalid)?;
        let issuer = claims["iss"].as_str().ok_or_else(invalid)?;
        let provider = self
            .oidc
            .iter()
            .find(|p| p.config.issuer.trim_end_matches('/') == issuer.trim_end_matches('/'))
            .ok_or_else(invalid)?;
        let kid = header.kid.clone().ok_or_else(invalid)?;
        let mut keys = self.jwks(provider, false).await?;
        if keys.find(&kid).is_none() {
            keys = self.jwks(provider, true).await?;
        }
        let jwk = keys.find(&kid).ok_or_else(invalid)?;
        let key = DecodingKey::from_jwk(jwk).map_err(|_| invalid())?;
        let mut validation = Validation::new(header.alg);
        validation.set_audience(&[&provider.config.audience]);
        validation.set_issuer(&[issuer]);
        let data = decode::<serde_json::Value>(token, &key, &validation).map_err(|e| {
            warn!(issuer, error = %e, "OIDC token refused");
            invalid()
        })?;
        let name = data.claims[&provider.config.username_claim]
            .as_str()
            .or_else(|| data.claims["sub"].as_str())
            .ok_or_else(invalid)?
            .to_string();
        let groups = data.claims[&provider.config.groups_claim]
            .as_array()
            .map(|g| {
                g.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Caller {
            principal: Principal {
                kind: PrincipalKind::User,
                name,
                groups,
            },
            bootstrap: false,
        })
    }
}
