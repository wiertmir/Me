use std::path::PathBuf;

use common::logging;
use tasks_service::{Config, app, build_state};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = match &args[..] {
        [] => "config.toml",
        [path] => path,
        _ => anyhow::bail!("usage: tasks-service [CONFIG]"),
    };
    let cfg = Config::load(&PathBuf::from(path))?;
    logging::init(&cfg.log, "tasks-service");
    let listen = cfg.listen;
    let state = build_state(cfg)?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(listen = %listener.local_addr()?, "tasks-service started");
    axum::serve(listener, app(state)).await?;
    Ok(())
}
