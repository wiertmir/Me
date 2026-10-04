//! Access-token verification for resource servers: ES256 only, keys from the issuer's JWKS.
use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    extract::{FromRef, FromRequestParts},
    http::{StatusCode, header::AUTHORIZATION, request::Parts},
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::{sync::Mutex, time::Instant};
use uuid::Uuid;

use crate::ApiError;

/// Clock skew tolerated between auth-service and a resource server when checking `exp`.
const LEEWAY_SECS: u64 = 30;
const REFETCH_INTERVAL: Duration = Duration::from_secs(60);
const JWKS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Deserialize)]
pub struct Claims {
    pub sub: Uuid,
    pub scope: String,
    pub preferred_username: String,
    pub exp: i64,
}

impl Claims {
    /// True when `scope` (space-separated list) contains exactly this token.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scope.split(' ').any(|s| s == scope)
    }
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    crv: Option<String>,
    kid: Option<String>,
    alg: Option<String>,
    #[serde(rename = "use")]
    use_: Option<String>,
    x: Option<String>,
    y: Option<String>,
}

#[derive(Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

#[derive(Default)]
struct Cache {
    /// `None` until a fetch has succeeded.
    keys: Option<HashMap<String, DecodingKey>>,
    last_refetch: Option<Instant>,
}

pub struct TokenVerifier {
    issuer: String,
    audience: String,
    jwks_url: String,
    http: reqwest::Client,
    refetch_interval: Duration,
    // Held across the JWKS fetch on purpose: concurrent callers wait and then reuse its result.
    cache: Mutex<Cache>,
}

fn unauthorized() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "invalid or missing access token",
    )
}

impl TokenVerifier {
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>) -> Self {
        let issuer = issuer.into();
        Self {
            jwks_url: format!("{}/.well-known/jwks.json", issuer.trim_end_matches('/')),
            issuer,
            audience: audience.into(),
            http: reqwest::Client::builder()
                .timeout(JWKS_TIMEOUT)
                .build()
                .expect("static reqwest client config"),
            refetch_interval: REFETCH_INTERVAL,
            cache: Mutex::new(Cache::default()),
        }
    }

    /// Overrides the minimum time between JWKS refetches (default 60 s).
    pub fn with_refetch_interval(mut self, interval: Duration) -> Self {
        self.refetch_interval = interval;
        self
    }

    /// Overrides the keys URL (default `<issuer>/.well-known/jwks.json`), e.g. for an in-cluster address.
    pub fn with_jwks_url(mut self, url: impl Into<String>) -> Self {
        self.jwks_url = url.into();
        self
    }

    pub async fn verify(&self, token: &str) -> Result<Claims, ApiError> {
        let fail = |reason: &str| {
            tracing::debug!(reason, "access token rejected");
            unauthorized()
        };
        let header = jsonwebtoken::decode_header(token).map_err(|_| fail("malformed header"))?;
        let kid = header.kid.ok_or_else(|| fail("no kid"))?;
        let key = self
            .key_for(&kid)
            .await?
            .ok_or_else(|| fail("unknown kid"))?;

        let mut v = Validation::new(Algorithm::ES256); // only ES256 is accepted
        v.leeway = LEEWAY_SECS;
        v.set_issuer(&[&self.issuer]);
        v.set_audience(&[&self.audience]);
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        jsonwebtoken::decode::<Claims>(token, &key, &v)
            .map(|d| d.claims)
            .map_err(|e| {
                tracing::debug!(reason = %e, "access token rejected");
                unauthorized()
            })
    }

    async fn key_for(&self, kid: &str) -> Result<Option<DecodingKey>, ApiError> {
        let mut c = self.cache.lock().await;
        if let Some(k) = c.keys.as_ref().and_then(|k| k.get(kid)) {
            return Ok(Some(k.clone()));
        }
        let throttled = c
            .last_refetch
            .is_some_and(|t| t.elapsed() < self.refetch_interval);
        if throttled {
            return match c.keys {
                Some(_) => Ok(None),
                None => Err(unavailable()),
            };
        }
        // The very first successful load does not count as a refetch.
        let had_keys = c.keys.is_some();
        match self.fetch().await {
            Ok(keys) => {
                let found = keys.get(kid).cloned();
                c.keys = Some(keys);
                c.last_refetch = had_keys.then(Instant::now);
                Ok(found)
            }
            Err(e) => {
                tracing::warn!(error = %e, url = %self.jwks_url, "JWKS fetch failed");
                c.last_refetch = Some(Instant::now());
                if had_keys {
                    Ok(None)
                } else {
                    Err(unavailable())
                }
            }
        }
    }

    async fn fetch(&self) -> Result<HashMap<String, DecodingKey>, reqwest::Error> {
        let set: JwkSet = self
            .http
            .get(&self.jwks_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(set
            .keys
            .into_iter()
            .filter(|k| {
                k.kty == "EC"
                    && k.crv.as_deref() == Some("P-256")
                    && k.alg.as_deref().is_none_or(|a| a == "ES256")
                    && k.use_.as_deref().is_none_or(|u| u == "sig")
            })
            .filter_map(|k| {
                let key = DecodingKey::from_ec_components(k.x.as_deref()?, k.y.as_deref()?).ok()?;
                Some((k.kid?, key))
            })
            .collect())
    }
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "token verification is temporarily unavailable",
    )
}

/// Extractor: a verified `Authorization: Bearer <access token>`.
pub struct AuthUser(pub Claims);

impl<S> FromRequestParts<S> for AuthUser
where
    Arc<TokenVerifier>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, t)| t.trim())
            .filter(|t| !t.is_empty())
            .ok_or_else(unauthorized)?;
        Arc::<TokenVerifier>::from_ref(state)
            .verify(token)
            .await
            .map(AuthUser)
    }
}
