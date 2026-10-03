//! ES256 signing key, JWT issue/verify, JWKS. Hand-rolled JWS (3 parts, fixed algorithm) rather than a
//! general JWT library, so no other algorithm can ever be accepted.
use std::{io::Write, path::Path};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use chrono::Utc;
use p256::{
    ecdsa::{
        Signature, SigningKey, VerifyingKey,
        signature::{Signer as _, Verifier as _},
    },
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
    key: SigningKey,
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
        let key = match std::fs::read_to_string(path) {
            Ok(pem) => SigningKey::from_pkcs8_pem(&pem)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?,
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
                key
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self::from_key(key))
    }

    fn from_key(key: SigningKey) -> Signer {
        let point = key.verifying_key().to_sec1_point(false);
        let kid: String = Sha256::digest(point.as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect();
        let (x, y) = (
            point.x().expect("uncompressed point"),
            point.y().expect("uncompressed point"),
        );
        let jwks = json!({"keys": [{
            "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": kid,
            "x": B64.encode(x), "y": B64.encode(y),
        }]});
        Signer { key, kid, jwks }
    }

    pub fn jwks(&self) -> Value {
        self.jwks.clone()
    }

    /// Signs arbitrary claims as a compact ES256 JWT.
    pub fn sign(&self, claims: &Value) -> String {
        let header = json!({"alg": "ES256", "typ": "JWT", "kid": self.kid});
        let input = format!(
            "{}.{}",
            B64.encode(header.to_string()),
            B64.encode(claims.to_string())
        );
        let sig: Signature = self.key.sign(input.as_bytes());
        format!("{input}.{}", B64.encode(sig.to_bytes()))
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
        let mut parts = token.split('.');
        let (h, p, s) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        let header: Value = serde_json::from_slice(&B64.decode(h).ok()?).ok()?;
        if header["alg"] != "ES256" || header["kid"] != self.kid.as_str() {
            return None;
        }
        let sig = Signature::from_slice(&B64.decode(s).ok()?).ok()?;
        let vk: &VerifyingKey = self.key.verifying_key();
        vk.verify(format!("{h}.{p}").as_bytes(), &sig).ok()?;
        #[derive(Deserialize)]
        struct Full {
            iss: String,
            aud: String,
            exp: i64,
            sub: String,
            scope: String,
        }
        let c: Full = serde_json::from_slice(&B64.decode(p).ok()?).ok()?;
        (c.iss == cfg.issuer && c.aud == cfg.audience && c.exp > Utc::now().timestamp()).then_some(
            AccessClaims {
                sub: c.sub,
                scope: c.scope,
            },
        )
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
