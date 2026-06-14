//! Continuous BLE acquisition loop over many short-lived HyfindTag connections.
//!
//! HyfindTags are not streamers: a tag buffers samples while disconnected, then
//! advertises, expects a time-sync on connect, **indicates** every buffered
//! packet until empty, and disconnects (CLAUDE.md A.2). This module runs that
//! cycle automatically and forever:
//!
//! ```text
//! scan ─▶ tag advertising? ─▶ connect ─▶ write time-sync ─▶ drain all packets ─▶ disconnect ─▶ (repeat)
//! ```
//!
//! ## Boundaries
//! - Tags are matched by the advertised **name prefix** [`TAG_NAME_PREFIX`];
//!   there is **no** filter on the advertised service UUID — the tag does not
//!   advertise it (CLAUDE.md A.3).
//! - The DATA characteristic is consumed via **indications** (not notify).
//! - The time-sync command is produced by [`crate::command::DownlinkCommand`]
//!   (Issue 2) and written to the COMMAND characteristic **before** draining;
//!   the current epoch ms read in [`current_epoch_ms`] is the one place this
//!   module touches the real clock (CLAUDE.md A.6).
//! - Decode reuses [`crate::models::BlePacket::from_bytes`] (Issue 1) and
//!   storage reuses [`crate::store::SessionStore`] (Issue 3); the pure
//!   decode→insert step lives in [`decode_and_store`] so it is unit-testable
//!   without any BLE transport.
//! - A failure servicing one tag is logged and never stops the loop or other
//!   tags.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use btleplug::api::{Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures::StreamExt;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::command::DownlinkCommand;
use crate::models::BlePacket;
use crate::store::{SessionStore, TagId};

/// Advertised name prefix that identifies a HyfindTag (CLAUDE.md A.3).
pub const TAG_NAME_PREFIX: &str = "HyfindTag";

/// Primary service UUID `00001522-1212-efde-1523-785feabcd123` (not advertised).
pub const UUID_SERVICE: Uuid = Uuid::from_u128(0x0000_1522_1212_efde_1523_785f_eabc_d123);

/// DATA characteristic `00001523-…` — sample packets arrive here via INDICATE.
pub const UUID_DATA: Uuid = Uuid::from_u128(0x0000_1523_1212_efde_1523_785f_eabc_d123);

/// COMMAND characteristic `00001525-…` — the time-sync command is written here.
pub const UUID_COMMAND: Uuid = Uuid::from_u128(0x0000_1525_1212_efde_1523_785f_eabc_d123);

/// Default cap on tags serviced concurrently.
///
/// The model is short-lived drain cycles, but desktop BLE stacks cap concurrent
/// central links well below the tag count, so this is a parameter rather than an
/// inline assumption that every tag connects at once (CLAUDE.md A.2).
pub const DEFAULT_MAX_CONCURRENT: usize = 4;

/// Default idle gap after which a tag is treated as drained.
///
/// A tag indicates its buffered packets back-to-back, then disconnects. If no
/// packet arrives within this window the drain is considered complete even if no
/// disconnect event was observed.
pub const DEFAULT_DRAIN_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Configuration for the acquisition loop.
#[derive(Debug, Clone)]
pub struct AcquireConfig {
    /// Advertised name prefix a peripheral must match to be serviced.
    pub name_prefix: String,
    /// Maximum number of tags connected/drained at the same instant.
    pub max_concurrent: usize,
    /// Idle gap with no indicated packet after which a tag is deemed drained.
    pub drain_idle_timeout: Duration,
}

impl Default for AcquireConfig {
    fn default() -> Self {
        Self {
            name_prefix: TAG_NAME_PREFIX.to_owned(),
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            drain_idle_timeout: DEFAULT_DRAIN_IDLE_TIMEOUT,
        }
    }
}

/// A handle to the session store shared across concurrent tag-service tasks.
///
/// The store wraps a single SQLite connection (not `Sync`), so concurrent drains
/// serialize their inserts behind this async mutex. Inserts are brief and never
/// hold the guard across an `.await`.
pub type SharedStore = Arc<Mutex<SessionStore>>;

/// Errors raised while running the acquisition loop.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    /// A btleplug / OS BLE operation failed.
    #[error("ble error: {0}")]
    Ble(#[from] btleplug::Error),
    /// No Bluetooth adapter was available on this host.
    #[error("no bluetooth adapter found")]
    NoAdapter,
    /// A required GATT characteristic was absent after service discovery.
    #[error("characteristic {0} not found on tag")]
    CharacteristicNotFound(Uuid),
    /// A drained packet could not be decoded (Issue 1).
    #[error("decode error: {0}")]
    Decode(#[from] crate::models::DecodeError),
    /// Persisting decoded samples failed (Issue 3).
    #[error("store error: {0}")]
    Store(#[from] crate::store::StoreError),
    /// The system clock could not be read as epoch milliseconds.
    #[error("system clock is not representable as epoch milliseconds")]
    Clock,
}

/// Returns the current wall-clock time as epoch milliseconds.
///
/// This is the **only** place the acquisition path reads the real clock
/// (CLAUDE.md A.6); the value is handed to Issue 2's encoder, never to a
/// hand-rolled byte frame.
///
/// # Errors
/// Returns [`AcquireError::Clock`] if the clock is before the Unix epoch or the
/// millisecond count overflows `i64`.
pub fn current_epoch_ms() -> Result<i64, AcquireError> {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AcquireError::Clock)?;
    i64::try_from(since_epoch.as_millis()).map_err(|_| AcquireError::Clock)
}

/// Decodes one raw 100-byte DATA payload and inserts its samples under `tag_id`.
///
/// This is the pure seam the live loop drives for every indicated packet: it
/// reuses [`BlePacket::from_bytes`] (Issue 1) and
/// [`SessionStore::insert_samples`] (Issue 3) with no BLE transport involved, so
/// the decode→store behaviour — including the malformed-payload error path — is
/// unit-testable on its own.
///
/// Returns the number of samples stored on success.
///
/// # Errors
/// Returns [`AcquireError::Decode`] if `raw` is not a valid packet (e.g. wrong
/// length), or [`AcquireError::Store`] if the insert fails. On a decode error
/// nothing is written.
pub fn decode_and_store(
    store: &SessionStore,
    tag_id: &TagId,
    raw: &[u8],
) -> Result<usize, AcquireError> {
    let packet = BlePacket::from_bytes(raw)?;
    let samples = packet.samples();
    store.insert_samples(tag_id, samples)?;
    Ok(samples.len())
}

/// Runs the scan → connect → time-sync → drain → disconnect loop until cancelled.
///
/// Continuously scans (no service-UUID filter), and for each discovered tag whose
/// advertised name starts with `config.name_prefix` spawns a bounded task that
/// connects, time-syncs, drains, and disconnects. Decoded samples land in `store`
/// keyed by the tag's BLE address. Per-tag failures are logged and isolated; the
/// loop returns `Ok(())` once `cancel` fires.
///
/// # Errors
/// Returns [`AcquireError`] only for failures that prevent the loop from running
/// at all (no adapter, scanning cannot start). Per-tag errors do not propagate.
pub async fn run_acquisition(
    config: AcquireConfig,
    store: SharedStore,
    cancel: CancellationToken,
) -> Result<(), AcquireError> {
    let manager = Manager::new().await?;
    let adapter = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or(AcquireError::NoAdapter)?;
    // Shared by reference everywhere and cloned into per-tag tasks; `Arc` avoids
    // depending on `Adapter: Clone` across platforms.
    let adapter = Arc::new(adapter);

    // No service-UUID filter: the tag does not advertise its service (A.3).
    adapter.start_scan(ScanFilter::default()).await?;
    log::info!(
        "scanning for tags with name prefix {:?} (max {} concurrent)",
        config.name_prefix,
        config.max_concurrent
    );

    let config = Arc::new(config);
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent));
    // Tags currently being serviced, so repeated advertisements don't spawn
    // duplicate connect/drain tasks for the same peripheral.
    let in_progress: Arc<Mutex<HashSet<PeripheralId>>> = Arc::new(Mutex::new(HashSet::new()));

    let mut events = adapter.events().await?;
    loop {
        let event = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            event = events.next() => event,
        };
        let Some(event) = event else { break };

        let id = match event {
            CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) => id,
            _ => continue,
        };

        maybe_service_tag(&adapter, id, &config, &store, &semaphore, &in_progress).await;
    }

    // Best-effort: stopping the scan should not mask a clean shutdown.
    if let Err(error) = adapter.stop_scan().await {
        log::warn!("failed to stop scan on shutdown: {error}");
    }
    Ok(())
}

/// Spawns a service task for `id` if it is a fresh HyfindTag and a slot is free.
///
/// Non-matching, already-in-progress, or over-capacity peripherals are ignored;
/// an ignored tag simply re-advertises and is reconsidered later.
async fn maybe_service_tag(
    adapter: &Arc<Adapter>,
    id: PeripheralId,
    config: &Arc<AcquireConfig>,
    store: &SharedStore,
    semaphore: &Arc<Semaphore>,
    in_progress: &Arc<Mutex<HashSet<PeripheralId>>>,
) {
    let Some(name) = advertised_name(adapter, &id).await else {
        return;
    };
    if !name.starts_with(config.name_prefix.as_str()) {
        return;
    }

    // Reserve a concurrency slot without blocking the scan loop; if none is free
    // the tag will be picked up on a later advertisement.
    let Ok(permit) = Arc::clone(semaphore).try_acquire_owned() else {
        return;
    };

    {
        let mut guard = in_progress.lock().await;
        if !guard.insert(id.clone()) {
            return; // Already being serviced.
        }
    }

    // Clones moved into the task: all are `Arc`s (cheap), each needed because
    // the task outlives this scope.
    let adapter = Arc::clone(adapter);
    let config = Arc::clone(config);
    let store = Arc::clone(store);
    let in_progress = Arc::clone(in_progress);
    tokio::spawn(async move {
        let _permit = permit; // Released (slot freed) when the task ends.
        if let Err(error) = service_tag(&adapter, &id, &name, &config, &store).await {
            log::warn!("tag {name} ({id:?}) service error: {error}");
        }
        in_progress.lock().await.remove(&id);
    });
}

/// Reads a peripheral's advertised local name, if any.
async fn advertised_name(adapter: &Adapter, id: &PeripheralId) -> Option<String> {
    let peripheral = adapter.peripheral(id).await.ok()?;
    peripheral
        .properties()
        .await
        .ok()?
        .and_then(|p| p.local_name)
}

/// Connects to one tag, drains it, and always disconnects afterwards.
async fn service_tag(
    adapter: &Adapter,
    id: &PeripheralId,
    name: &str,
    config: &AcquireConfig,
    store: &SharedStore,
) -> Result<(), AcquireError> {
    let peripheral = adapter.peripheral(id).await?;
    let tag_id = TagId::new(peripheral.address().to_string());

    peripheral.connect().await?;
    // Disconnect regardless of how the drain goes, so the link is freed for the
    // next tag even on error.
    let result = drain_tag(&peripheral, &tag_id, name, config, store).await;
    if let Err(error) = peripheral.disconnect().await {
        log::warn!("tag {name} disconnect error: {error}");
    }
    result
}

/// Time-syncs a connected tag, then stores every packet it indicates.
async fn drain_tag(
    peripheral: &Peripheral,
    tag_id: &TagId,
    name: &str,
    config: &AcquireConfig,
    store: &SharedStore,
) -> Result<(), AcquireError> {
    peripheral.discover_services().await?;
    let characteristics = peripheral.characteristics();
    let command = characteristics
        .iter()
        .find(|c| c.uuid == UUID_COMMAND)
        .ok_or(AcquireError::CharacteristicNotFound(UUID_COMMAND))?;
    let data = characteristics
        .iter()
        .find(|c| c.uuid == UUID_DATA)
        .ok_or(AcquireError::CharacteristicNotFound(UUID_DATA))?;

    // Record the human-readable label for this stable tag id.
    store.lock().await.upsert_tag(tag_id, name)?;

    // Write time-sync BEFORE draining (A.6), via Issue 2's encoder — not a
    // hand-rolled frame.
    let time_ms = current_epoch_ms()?;
    let command_bytes = DownlinkCommand::SetTime { time_ms }.to_bytes();
    peripheral
        .write(command, &command_bytes, WriteType::WithResponse)
        .await?;

    // DATA is INDICATE: `subscribe` enables indications for this characteristic.
    peripheral.subscribe(data).await?;
    let mut notifications = peripheral.notifications().await?;

    let mut stored = 0usize;
    loop {
        match tokio::time::timeout(config.drain_idle_timeout, notifications.next()).await {
            // A DATA packet: decode + store. A per-packet error is logged and
            // draining continues — one bad packet must not abort the tag.
            Ok(Some(notification)) if notification.uuid == UUID_DATA => {
                // Scope the store guard so it is released before matching.
                let outcome = {
                    let store = store.lock().await;
                    decode_and_store(&store, tag_id, &notification.value)
                };
                match outcome {
                    Ok(count) => stored += count,
                    Err(error) => {
                        log::warn!("tag {name}: dropping undecodable packet: {error}");
                    }
                }
            }
            // A notification on some other characteristic: ignore, keep draining.
            Ok(Some(_)) => {}
            // Stream ended (tag disconnected) — drain complete.
            Ok(None) => break,
            // Idle gap with no further packets — treat as drained.
            Err(_) => break,
        }
    }

    log::info!("tag {name}: drained {stored} samples");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SAMPLES_PER_PACKET;

    /// Builds a valid 100-byte packet mirroring the firmware `struct` layout
    /// `"<q" + "HHhh"*10 + "H" + "B"*10` (same shape models.rs tests use).
    fn valid_packet() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(crate::models::PACKET_SIZE);
        bytes.extend_from_slice(&1_000_000_i64.to_le_bytes()); // time
        for i in 0..SAMPLES_PER_PACKET as u16 {
            bytes.extend_from_slice(&(2567 + i).to_le_bytes()); // temp
            bytes.extend_from_slice(&(4500 + i).to_le_bytes()); // hum
            bytes.extend_from_slice(&100_i16.to_le_bytes()); // adc1
            bytes.extend_from_slice(&(-100_i16).to_le_bytes()); // adc2
        }
        bytes.extend_from_slice(&50_u16.to_le_bytes()); // dt
        bytes.extend_from_slice(&[0x88u8; SAMPLES_PER_PACKET]); // stimuli
        assert_eq!(bytes.len(), crate::models::PACKET_SIZE);
        bytes
    }

    #[test]
    fn decode_and_store_persists_samples_under_tag_id() {
        let store = SessionStore::open_in_memory().expect("open in-memory db");
        let tag = TagId::new("AA:BB:CC:DD:EE:FF");

        let stored = decode_and_store(&store, &tag, &valid_packet()).expect("decode+store");

        assert_eq!(stored, SAMPLES_PER_PACKET);
        let read_back = store.samples_for(&tag).expect("read back");
        assert_eq!(read_back.len(), SAMPLES_PER_PACKET);
        // First sample timestamp == packet `time` (i == 0).
        assert_eq!(read_back[0].timestamp_ms, 1_000_000);
        // Sample is attributed to the supplied tag id, and only that id.
        assert_eq!(store.tag_ids().expect("ids"), vec![tag]);
    }

    #[test]
    fn decode_and_store_rejects_short_payload_without_storing() {
        let store = SessionStore::open_in_memory().expect("open in-memory db");
        let tag = TagId::new("AA:BB:CC:DD:EE:FF");

        // 99 bytes: one short of a packet — the per-tag error path.
        let result = decode_and_store(&store, &tag, &[0u8; 99]);

        assert!(matches!(result, Err(AcquireError::Decode(_))));
        // Nothing was written for the tag.
        assert!(store.samples_for(&tag).expect("read back").is_empty());
    }

    #[test]
    fn malformed_packet_does_not_panic_and_leaves_store_usable() {
        let store = SessionStore::open_in_memory().expect("open in-memory db");
        let tag = TagId::new("11:22:33:44:55:66");

        // A bad packet is handled gracefully...
        assert!(decode_and_store(&store, &tag, &[]).is_err());
        // ...and a subsequent good packet still stores, proving the loop's
        // per-packet error isolation keeps the store usable.
        let stored = decode_and_store(&store, &tag, &valid_packet()).expect("good packet");
        assert_eq!(stored, SAMPLES_PER_PACKET);
        assert_eq!(
            store.samples_for(&tag).expect("read back").len(),
            SAMPLES_PER_PACKET
        );
    }

    #[test]
    fn current_epoch_ms_is_positive_and_after_2020() {
        // 2020-01-01T00:00:00Z in ms; the real clock must be well past it.
        const JAN_2020_MS: i64 = 1_577_836_800_000;
        let now = current_epoch_ms().expect("clock readable");
        assert!(now > JAN_2020_MS, "epoch ms {now} should be after 2020");
    }

    #[test]
    fn default_config_uses_named_constants() {
        let config = AcquireConfig::default();
        assert_eq!(config.name_prefix, TAG_NAME_PREFIX);
        assert_eq!(config.max_concurrent, DEFAULT_MAX_CONCURRENT);
        assert_eq!(config.drain_idle_timeout, DEFAULT_DRAIN_IDLE_TIMEOUT);
    }

    #[test]
    fn gatt_uuid_constants_match_the_contract() {
        assert_eq!(
            UUID_SERVICE.to_string(),
            "00001522-1212-efde-1523-785feabcd123"
        );
        assert_eq!(
            UUID_DATA.to_string(),
            "00001523-1212-efde-1523-785feabcd123"
        );
        assert_eq!(
            UUID_COMMAND.to_string(),
            "00001525-1212-efde-1523-785feabcd123"
        );
    }
}
