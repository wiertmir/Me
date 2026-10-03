//! ES256 signing key, JWT issue/verify (via `jsonwebtoken`, algorithm pinned to ES256), JWKS.
use std::{io::Write, path::Path};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use chrono::Utc;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use p256::{
    ecdsa::SigningKey,
    elliptic_curve::Generate as _,
    pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{Config, users::User};

pub const ACCESS_TOKEN_SECS: i64 = 15 * 60;
const ID_TOKEN_SECS: i64 = 15 * 60;

pub struct Signer {
    enc: EncodingKey,
    dec: DecodingKey,
    kid: String,
    jwks: Value,
}

/// What a verified access token tells us.
#[derive(Debug, Deserialize)]
pub struct AccessClaims {
    pub sub: String,
    pub scope: String,
}

impl Signer {
    /// Loads the PKCS#8 PEM key, or generates one (file mode 0600) when absent.
    pub fn load_or_create(path: &Path) -> anyhow::Result<Signer> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let pem = match std::fs::read_to_string(path) {
            Ok(pem) => pem,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = SigningKey::generate_from_rng(&mut rand::rng());
                let pem = key
                    .to_pkcs8_pem(LineEnding::LF)
                    .map_err(|e| anyhow::anyhow!("encoding key: {e}"))?;
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
                opts.open(path)?.write_all(pem.as_bytes())?;
                pem.to_string()
            }
            Err(e) => return Err(e.into()),
        };
        let key = SigningKey::from_pkcs8_pem(&pem)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let point = key.verifying_key().to_sec1_point(false);
        let kid: String = Sha256::digest(point.as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect();
        let (x, y) = (
            B64.encode(point.x().expect("uncompressed point")),
            B64.encode(point.y().expect("uncompressed point")),
        );
        let jwks = json!({"keys": [{"kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": kid, "x": x, "y": y}]});
        Ok(Signer {
            enc: EncodingKey::from_ec_pem(pem.as_bytes())?,
            dec: DecodingKey::from_ec_components(&x, &y)?,
            kid,
            jwks,
        })
    }

    pub fn jwks(&self) -> Value {
        self.jwks.clone()
    }

    /// Signs arbitrary claims as a compact ES256 JWT.
    pub fn sign(&self, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, claims, &self.enc).expect("ES256 signing with a valid key")
    }

    pub fn access_token(&self, cfg: &Config, user: &User, scope: &str, client_id: &str) -> String {
        let now = Utc::now().timestamp();
        self.sign(&json!({
            "iss": cfg.issuer, "sub": user.id, "aud": cfg.audience, "exp": now + ACCESS_TOKEN_SECS, "iat": now,
            "scope": scope, "preferred_username": user.username, "client_id": client_id,
        }))
    }

    pub fn id_token(
        &self,
        cfg: &Config,
        user: &User,
        client_id: &str,
        nonce: Option<&str>,
    ) -> String {
        let now = Utc::now().timestamp();
        let mut claims = json!({
            "iss": cfg.issuer, "sub": user.id, "aud": client_id, "exp": now + ID_TOKEN_SECS, "iat": now,
            "preferred_username": user.username, "name": display_name(user),
            "email": user.email, "email_verified": user.email_verified,
        });
        if let Some(n) = nonce {
            claims["nonce"] = n.into();
        }
        self.sign(&claims)
    }

    /// Verifies signature (ES256 only, our kid), `iss`, `aud` and `exp` of one of our access tokens.
    pub fn verify_access(&self, cfg: &Config, token: &str) -> Option<AccessClaims> {
        let header = jsonwebtoken::decode_header(token).ok()?;
        if header.kid.as_deref() != Some(&self.kid) {
            return None;
        }
        let mut v = Validation::new(Algorithm::ES256);
        v.leeway = 0; // explicit: no clock skew tolerance, tokens live 15 minutes and issuer = verifier
        v.set_issuer(&[&cfg.issuer]);
        v.set_audience(&[&cfg.audience]);
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        jsonwebtoken::decode::<AccessClaims>(token, &self.dec, &v)
            .ok()
            .map(|d| d.claims)
    }
}

/// Display name, falling back to the username when none is set.
pub fn display_name(user: &User) -> &str {
    if user.display_name.is_empty() {
        &user.username
    } else {
        &user.display_name
    }
}
