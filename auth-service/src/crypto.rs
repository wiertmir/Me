use std::sync::LazyLock;

use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use common::{ApiError, ApiResult};
use sha2::{Digest, Sha256};

pub const MAX_PASSWORD_BYTES: usize = 1024;

pub fn hash_password(pw: &str) -> String {
    Argon2::default()
        .hash_password(pw.as_bytes())
        .expect("argon2 hashing with default params")
        .to_string()
}

pub fn verify_password(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .is_ok_and(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
}

/// Verifies against a throwaway hash so unknown users cost the same time as wrong passwords.
pub fn verify_dummy(pw: &str) {
    static DUMMY: LazyLock<String> = LazyLock::new(|| hash_password("dummy password for timing"));
    verify_password(pw, &DUMMY);
}

fn validation(msg: &str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "validation", msg)
}

/// Upper bound only; used before hashing anything a client sent.
pub fn check_password_size(pw: &str) -> ApiResult<()> {
    if pw.len() > MAX_PASSWORD_BYTES {
        return Err(validation("password is too long"));
    }
    Ok(())
}

pub fn validate_password(pw: &str) -> ApiResult<()> {
    check_password_size(pw)?;
    if pw.chars().count() < 12 {
        return Err(validation("password must be at least 12 characters"));
    }
    Ok(())
}

/// 32 random bytes, base64url without padding.
pub fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

/// No `0 O 1 l I`; 32 symbols, so a masked random byte is unbiased.
const TEMP_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// 16-character one-time password from the OS CSPRNG (80 bits).
pub fn temporary_password() -> String {
    use rand::TryRng as _;
    let mut buf = [0u8; 16];
    rand::rngs::SysRng
        .try_fill_bytes(&mut buf)
        .expect("OS random source unavailable");
    buf.iter()
        .map(|b| TEMP_ALPHABET[(b & 31) as usize] as char)
        .collect()
}

/// 16 lower-case letters from the OS CSPRNG (about 75 bits). Rejection sampling (bytes >= 208 are
/// discarded) keeps the 26 letters free of modulo bias.
pub fn app_password() -> String {
    use rand::TryRng as _;
    let mut out = String::with_capacity(16);
    while out.len() < 16 {
        let mut buf = [0u8; 32];
        rand::rngs::SysRng
            .try_fill_bytes(&mut buf)
            .expect("OS random source unavailable");
        for b in buf.into_iter().filter(|b| *b < 208).take(16 - out.len()) {
            out.push((b'a' + b % 26) as char);
        }
    }
    out
}

/// `abcdefghijklmnop` becomes `abcd-efgh-ijkl-mnop`.
pub fn format_app_password(raw: &str) -> String {
    raw.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("-")
}

/// Drops dashes and whitespace and lower-cases, so users may type the password any way they like.
pub fn normalize_app_password(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_roundtrip_and_token_shape() {
        let h = hash_password("correct horse battery");
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery", &h));
        assert!(!verify_password("wrong", &h));
        assert!(!verify_password("x", "not a hash"));
        assert_eq!(random_token().len(), 43);
        let t = temporary_password();
        assert_eq!(t.len(), 16);
        assert!(t.bytes().all(|b| TEMP_ALPHABET.contains(&b)));
        assert!(!"0O1lI".chars().any(|c| TEMP_ALPHABET.contains(&(c as u8))));
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(validate_password("short").is_err());
        assert!(validate_password(&"a".repeat(1025)).is_err());
        assert!(validate_password("twelve chars").is_ok());
    }
}
