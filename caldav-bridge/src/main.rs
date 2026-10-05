use std::path::PathBuf;

use caldav_bridge::{Config, app, build_state};
use common::logging;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = match &args[..] {
        [] => "config.toml",
        [path] => path,
        _ => anyhow::bail!("usage: caldav-bridge [CONFIG]"),
    };
    let cfg = Config::load(&PathBuf::from(path))?;
    logging::init(&cfg.log, "caldav-bridge");
    let listen = cfg.listen;
    let state = build_state(cfg)?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(listen = %listener.local_addr()?, "caldav-bridge started");
    axum::serve(listener, app(state)).await?;
    Ok(())
}
