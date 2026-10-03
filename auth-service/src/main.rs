use std::path::PathBuf;

use auth_service::{Config, app, build_state, logging};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path: PathBuf = std::env::args().nth(1).unwrap_or_else(|| "config.toml".into()).into();
    let cfg = Config::load(&path)?;
    logging::init(&cfg.log);
    let listen = cfg.listen;
    let (state, seed_password) = build_state(cfg)?;
    if let Some(password) = seed_password {
        // The one-time seed password is the only secret ever logged.
        tracing::warn!(password = %password, "seeded admin user; change the password at first sign-in");
    }
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(listen = %listener.local_addr()?, "auth-service started");
    axum::serve(listener, app(state)).await?;
    Ok(())
}
