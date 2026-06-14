//! pc_sink binary: run the BLE acquisition loop until Ctrl-C, storing every
//! drained tag's samples into a per-session SQLite database.

use std::path::Path;

use anyhow::Context;
use pc_sink::acquire::{AcquireConfig, run_acquisition};
use pc_sink::store::SessionStore;

/// Filename of the session database opened in the current directory.
const SESSION_DB_FILE: &str = "session.sqlite";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let store = SessionStore::open(Path::new(SESSION_DB_FILE))
        .with_context(|| format!("opening session database {SESSION_DB_FILE}"))?;

    // Resolve when the user presses Ctrl-C; the loop then stops scanning cleanly.
    let shutdown = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            log::warn!("failed to listen for Ctrl-C, shutting down: {error}");
        }
    };

    run_acquisition(AcquireConfig::default(), store, shutdown)
        .await
        .context("running BLE acquisition loop")?;
    Ok(())
}
