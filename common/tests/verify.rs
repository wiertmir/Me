use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Json, Router, extract::FromRef, extract::State, http::StatusCode, routing::get};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use common::{ApiError, AuthUser, TokenVerifier};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use p256::{
    ecdsa::SigningKey,
    elliptic_curve::Generate as _,
    pkcs8::{EncodePrivateKey, LineEnding},
};
use serde_json::{Value, json};

const AUD: &str = "me-api";

struct TestKey {
    kid: String,
    enc: EncodingKey,
    jwk: Value,
}

impl TestKey {
    fn new(kid: &str) -> Self {
        let key = SigningKey::generate_from_rng(&mut rand::rng());
        let pem = key.to_pkcs8_pem(LineEnding::LF).unwrap();
        let point = key.verifying_key().to_sec1_point(false);
        let jwk = json!({"kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": kid,
            "x": B64.encode(point.x().unwrap()), "y": B64.encode(point.y().unwrap())});
        Self {
            kid: kid.into(),
            enc: EncodingKey::from_ec_pem(pem.as_bytes()).unwrap(),
            jwk,
        }
    }

    fn sign_claims(&self, claims: &Value) -> String {
        let mut h = Header::new(Algorithm::ES256);
        h.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&h, claims, &self.enc).unwrap()
    }

    fn token(&self, issuer: &str, aud: &str, exp_in: i64) -> String {
        self.sign_claims(&claims(issuer, aud, exp_in))
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn claims(issuer: &str, aud: &str, exp_in: i64) -> Value {
    json!({"iss": issuer, "sub": "6f1c2d3e-0000-4000-8000-000000000001", "aud": aud,
        "exp": now() + exp_in, "iat": now(), "scope": "openid calendar.read",
        "preferred_username": "alice", "client_id": "c"})
}

struct Jwks {
    issuer: String,
    keys: Arc<Mutex<Vec<Value>>>,
    hits: Arc<AtomicUsize>,
}

type JwksState = (Arc<Mutex<Vec<Value>>>, Arc<AtomicUsize>);

async fn serve_jwks(keys: Vec<Value>) -> Jwks {
    let keys = Arc::new(Mutex::new(keys));
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/.well-known/jwks.json",
            get(|State((k, h)): State<JwksState>| async move {
                h.fetch_add(1, Ordering::SeqCst);
                Json(json!({"keys": k.lock().unwrap().clone()}))
            }),
        )
        .with_state((keys.clone(), hits.clone()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Jwks { issuer, keys, hits }
}

async fn setup() -> (TestKey, Jwks, TokenVerifier) {
    let key = TestKey::new("k1");
    let srv = serve_jwks(vec![key.jwk.clone()]).await;
    let v = TokenVerifier::new(srv.issuer.clone(), AUD);
    (key, srv, v)
}

async fn assert_unauthorized(v: &TokenVerifier, token: &str) {
    let e = v.verify(token).await.unwrap_err();
    assert_eq!(
        (e.status, e.code),
        (StatusCode::UNAUTHORIZED, "unauthorized")
    );
}

#[tokio::test]
async fn accepts_valid_token() {
    let (key, srv, v) = setup().await;
    let c = v.verify(&key.token(&srv.issuer, AUD, 300)).await.unwrap();
    assert_eq!(c.sub.to_string(), "6f1c2d3e-0000-4000-8000-000000000001");
    assert_eq!(c.preferred_username, "alice");
    assert!(c.has_scope("calendar.read") && c.has_scope("openid"));
    assert!(!c.has_scope("calendar") && !c.has_scope("calendar.read openid"));
}

#[tokio::test]
async fn rejects_expired_token() {
    let (key, srv, v) = setup().await;
    assert_unauthorized(&v, &key.token(&srv.issuer, AUD, -120)).await;
    // within the 30 s leeway it is still accepted
    v.verify(&key.token(&srv.issuer, AUD, -10)).await.unwrap();
}

#[tokio::test]
async fn rejects_wrong_audience_issuer_and_id_token() {
    let (key, srv, v) = setup().await;
    assert_unauthorized(&v, &key.token(&srv.issuer, "other", 300)).await;
    assert_unauthorized(&v, &key.token("http://evil", AUD, 300)).await;
    // ID token shape: aud is a client id
    assert_unauthorized(&v, &key.token(&srv.issuer, "test-client", 300)).await;
}

#[tokio::test]
async fn rejects_token_without_scope() {
    let (key, srv, v) = setup().await;
    let mut c = claims(&srv.issuer, AUD, 300);
    c.as_object_mut().unwrap().remove("scope");
    assert_unauthorized(&v, &key.sign_claims(&c)).await;
}

#[tokio::test]
async fn rejects_token_signed_by_other_key() {
    let (_key, srv, v) = setup().await;
    let other = TestKey::new("k1"); // same kid, different key
    assert_unauthorized(&v, &other.token(&srv.issuer, AUD, 300)).await;
}

#[tokio::test]
async fn rejects_alg_none_and_hs256() {
    let (key, srv, v) = setup().await;
    let body = B64.encode(claims(&srv.issuer, AUD, 300).to_string());
    let none_hdr = B64.encode(json!({"alg": "none", "typ": "JWT", "kid": "k1"}).to_string());
    assert_unauthorized(&v, &format!("{none_hdr}.{body}.")).await;
    let mut h = Header::new(Algorithm::HS256);
    h.kid = Some(key.kid.clone());
    let hs = jsonwebtoken::encode(
        &h,
        &claims(&srv.issuer, AUD, 300),
        &EncodingKey::from_secret(b"secret"),
    )
    .unwrap();
    assert_unauthorized(&v, &hs).await;
    assert_unauthorized(&v, "not-a-jwt").await;
}

#[tokio::test]
async fn rejects_token_without_kid() {
    let (key, srv, v) = setup().await;
    let t = jsonwebtoken::encode(
        &Header::new(Algorithm::ES256),
        &claims(&srv.issuer, AUD, 300),
        &key.enc,
    )
    .unwrap();
    assert_unauthorized(&v, &t).await;
}

#[tokio::test]
async fn ignores_unsuitable_jwks_entries() {
    let key = TestKey::new("k1");
    let mut jwk = key.jwk.clone();
    jwk["alg"] = "RS256".into();
    let srv = serve_jwks(vec![jwk]).await;
    let v = TokenVerifier::new(srv.issuer.clone(), AUD);
    assert_unauthorized(&v, &key.token(&srv.issuer, AUD, 300)).await;
}

#[tokio::test]
async fn unknown_kid_refetches_once_per_interval() {
    let (key, srv, v) = setup().await;
    v.verify(&key.token(&srv.issuer, AUD, 300)).await.unwrap();
    assert_eq!(srv.hits.load(Ordering::SeqCst), 1);
    let stranger = TestKey::new("zzz");
    assert_unauthorized(&v, &stranger.token(&srv.issuer, AUD, 300)).await;
    assert_eq!(srv.hits.load(Ordering::SeqCst), 2);
    let stranger2 = TestKey::new("yyy");
    assert_unauthorized(&v, &stranger2.token(&srv.issuer, AUD, 300)).await;
    assert_unauthorized(&v, &stranger.token(&srv.issuer, AUD, 300)).await;
    assert_eq!(srv.hits.load(Ordering::SeqCst), 2);
    // the known key still works from cache
    v.verify(&key.token(&srv.issuer, AUD, 300)).await.unwrap();
    assert_eq!(srv.hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn concurrent_first_verifications_fetch_once() {
    let (key, srv, v) = setup().await;
    let v = Arc::new(v);
    let t = key.token(&srv.issuer, AUD, 300);
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let (v, t) = (v.clone(), t.clone());
            tokio::spawn(async move { v.verify(&t).await.map(|_| ()) })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(srv.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn key_rotation_is_picked_up_after_refetch() {
    let (key, srv, _) = setup().await;
    let v = TokenVerifier::new(srv.issuer.clone(), AUD).with_refetch_interval(Duration::ZERO);
    v.verify(&key.token(&srv.issuer, AUD, 300)).await.unwrap();
    let new_key = TestKey::new("k2");
    *srv.keys.lock().unwrap() = vec![new_key.jwk.clone()];
    v.verify(&new_key.token(&srv.issuer, AUD, 300))
        .await
        .unwrap();
    assert_unauthorized(&v, &key.token(&srv.issuer, AUD, 300)).await;
}

#[tokio::test]
async fn jwks_down_with_empty_cache_is_503() {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let v = TokenVerifier::new(issuer.clone(), AUD);
    let key = TestKey::new("k1");
    let e = v.verify(&key.token(&issuer, AUD, 300)).await.unwrap_err();
    assert_eq!(
        (e.status, e.code),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    );
}

#[derive(Clone)]
struct AppState {
    verifier: Arc<TokenVerifier>,
}
impl FromRef<AppState> for Arc<TokenVerifier> {
    fn from_ref(s: &AppState) -> Self {
        s.verifier.clone()
    }
}

#[tokio::test]
async fn extractor_reads_bearer_header() {
    let (key, srv, v) = setup().await;
    async fn me(AuthUser(c): AuthUser) -> Result<String, ApiError> {
        Ok(c.preferred_username)
    }
    let app = Router::new().route("/me", get(me)).with_state(AppState {
        verifier: Arc::new(v),
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/me", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let http = reqwest::Client::new();
    let r = http.get(&base).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["code"], "unauthorized");
    let t = key.token(&srv.issuer, AUD, 300);
    for bad in [
        format!("Basic {t}"),
        "Bearer".into(),
        "Bearer garbage".to_string(),
    ] {
        let r = http
            .get(&base)
            .header("Authorization", bad)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
    }
    let r = http
        .get(&base)
        .header("Authorization", format!("bearer {t}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "alice");
}
