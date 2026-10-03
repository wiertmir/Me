use std::path::PathBuf;

use auth_service::{Config, app, build_state, logging, reset_password};

const USAGE: &str =
    "usage: auth-service [CONFIG]\n       auth-service CONFIG reset-password USERNAME";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (path, reset_user) = match args[..] {
        [] => ("config.toml", None),
        [path] => (path, None),
        [path, "reset-password", username] => (path, Some(username)),
        _ => anyhow::bail!(USAGE),
    };
    let cfg = Config::load(&PathBuf::from(path))?;
    logging::init(&cfg.log);
    if let Some(username) = reset_user {
        // Recovery command: no server is started. The password goes to stdout, once, and nowhere else.
        let password = reset_password(&cfg, username)?;
        println!("one-time password for {username}: {password}");
        eprintln!("Sign in with it; you will be asked to choose a new password.");
        return Ok(());
    }
    let listen = cfg.listen;
    let (state, seed_password) = build_state(cfg)?;
    if let Some(password) = seed_password {
        // The one-time seed password is the only secret ever logged.
        tracing::warn!(one_time_password = %password, "seeded admin user; this one-time password must be changed at first sign-in");
    }
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(listen = %listener.local_addr()?, "auth-service started");
    axum::serve(listener, app(state)).await?;
    Ok(())
}
