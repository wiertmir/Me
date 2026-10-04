//! The shared service secret: header check and start-up rules.
use std::net::SocketAddr;

use axum::http::HeaderMap;
use subtle::ConstantTimeEq;

/// The `service_secret` published in `config.example.toml`: fine on one's own machine, public knowledge
/// anywhere else.
pub const EXAMPLE_SERVICE_SECRET: &str = "dev-only-service-secret-change-me";

pub fn secret_ok(headers: &HeaderMap, expected: &str) -> bool {
    // An absent header must never match, even against an (invalid) empty secret.
    headers
        .get("x-service-secret")
        .is_some_and(|v| v.as_bytes().ct_eq(expected.as_bytes()).into())
}

/// Start-up rules for a service secret: an error when it is shorter than 16 characters, or the published
/// example value on a listener that other machines can reach. `Ok(true)` when the example is in use.
pub fn check(secret: &str, listen: SocketAddr, env_var: &str) -> anyhow::Result<bool> {
    anyhow::ensure!(
        secret.len() >= 16,
        "service_secret must be at least 16 characters"
    );
    let example = secret == EXAMPLE_SERVICE_SECRET;
    anyhow::ensure!(
        !example || listen.ip().is_loopback(),
        "service_secret is the example value from config.example.toml and listen ({listen}) is not a loopback \
         address; set a secret of your own ({env_var})"
    );
    Ok(example)
}
