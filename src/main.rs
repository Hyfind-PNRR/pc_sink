#![warn(clippy::all, rust_2018_idioms)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// hide console window on Windows in release
//! `pc_sink` binary: runs the BLE acquisition loop until Ctrl-C.
//!
//! Opens a per-session `SQLite` store, then drives
//! [`pc_sink::ble::run_acquisition`], which scans for `HyfindTags`, time-syncs and
//! drains each one, and persists decoded samples keyed by tag id. A `Ctrl-C`
//! triggers a clean shutdown via a [`CancellationToken`].

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use pc_sink::ble::{AcquireConfig, DRAIN_EVENT_CHANNEL_CAPACITY, DrainEvent, run_acquisition};
use pc_sink::store::SessionStore;

/// Default session database file created in the working directory.
const SESSION_DB_PATH: &str = "pc_sink_session.sqlite";

mod app;

#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(target_arch = "wasm32"))]
fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Build the runtime manually — eframe must own the main thread, so tokio
    // runs in the background and we DON'T use #[tokio::main].
    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;

    let store = SessionStore::open(Path::new(SESSION_DB_PATH))
        .with_context(|| format!("opening session store at {SESSION_DB_PATH}"))?;
    let store = Arc::new(Mutex::new(store));

    let cancel = CancellationToken::new();

    // Subscribe BEFORE the loop starts so no drain is missed.
    let drain_events = broadcast::Sender::<DrainEvent>::new(DRAIN_EVENT_CHANNEL_CAPACITY);
    let ui_events = drain_events.subscribe(); // handed to the App
    let log_events = drain_events.subscribe(); // background logger

    // Ctrl-C task.
    {
        let shutdown = cancel.clone();
        runtime.spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                log::info!("shutdown requested; stopping acquisition");
                shutdown.cancel();
            }
        });
    }

    // Background drain-event logger.
    runtime.spawn(async move {
        let mut events = log_events;
        loop {
            match events.recv().await {
                Ok(event) => log::debug!(
                    "drained tag {}: {} samples",
                    event.tag_id,
                    event.samples_stored
                ),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    log::warn!("drain-event subscriber lagged; skipped {skipped} events");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Spawn acquisition onto the runtime — runs concurrently with the UI.
    let acq_handle = runtime.spawn(run_acquisition(
        AcquireConfig::default(),
        store,
        cancel.clone(),
        drain_events,
    ));

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 300.0])
            .with_min_inner_size([300.0, 220.0]),
        ..Default::default()
    };

    // eframe owns the main thread and blocks here until the window closes.
    // The App holds `ui_events` and (per shape B) a read-only DB path.
    let gui_result = eframe::run_native(
        "Pc_sink",
        native_options,
        Box::new(move |_cc| Ok(Box::new(app::App::new(ui_events)))),
    );

    // Window closed → tell acquisition to stop, then wait for it to finish.
    cancel.cancel();
    runtime
        .block_on(acq_handle)
        .context("joining acquisition task")?
        .context("running BLE acquisition loop")?;

    gui_result.map_err(|e| anyhow::anyhow!("eframe failed: {e}"))?;
    Ok(())
}
