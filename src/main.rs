//! pc_sink binary: runs the BLE acquisition loop until Ctrl-C.
//!
//! Opens a per-session SQLite store, then drives
//! [`pc_sink::ble::run_acquisition`], which scans for HyfindTags, time-syncs and
//! drains each one, and persists decoded samples keyed by tag id. A `Ctrl-C`
//! triggers a clean shutdown via a [`CancellationToken`].

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use pc_sink::ble::{AcquireConfig, run_acquisition};
use pc_sink::store::SessionStore;

/// Default session database file created in the working directory.
const SESSION_DB_PATH: &str = "pc_sink_session.sqlite";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let store = SessionStore::open(Path::new(SESSION_DB_PATH))
        .with_context(|| format!("opening session store at {SESSION_DB_PATH}"))?;
    let store = Arc::new(Mutex::new(store));

    // Cancellation: Ctrl-C cancels the token the loop selects on.
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            log::info!("shutdown requested; stopping acquisition");
            shutdown.cancel();
        }
    });

    run_acquisition(AcquireConfig::default(), store, cancel)
        .await
        .context("running BLE acquisition loop")?;
    Ok(())
}
