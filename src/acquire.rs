//! The continuous, fully-automatic BLE acquisition loop.
//!
//! pc_sink is a BLE **central** servicing a store-and-forward fleet of HyfindTag
//! sensors (CLAUDE.md §A.2). Each tag stays disconnected while it buffers
//! samples, periodically advertises, and — once a central connects and
//! time-syncs it — **indicates** every buffered packet until empty, then
//! disconnects. There are no persistent connections; this loop services many
//! short-lived connect/drain cycles opportunistically:
//!
//! ```text
//! scan ─▶ tag advertising? ─▶ connect ─▶ write time-sync ─▶ drain all packets ─▶ disconnect ─▶ (repeat)
//! ```
//!
//! Protocol specifics this module honours (CLAUDE.md §A.3 / §A.6):
//!
//! * Tags do **not** advertise the service UUID — they are matched by the
//!   advertised **name prefix** (`HyfindTag`), never by a service-UUID scan
//!   filter.
//! * The DATA characteristic is **indicate**, not notify; we subscribe to
//!   indications.
//! * The time-sync command is written to the COMMAND characteristic **before**
//!   draining, on every connect, carrying the current epoch milliseconds — the
//!   single place this module reads the wall clock.
//!
//! All BLE/clock I/O lives at this edge. Decoding ([`crate::models`]) and
//! storage ([`crate::store`]) stay pure/testable; the per-packet
//! decode-and-store step is factored into [`decode_and_store`] so it can be
//! exercised without any BLE hardware.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use btleplug::api::{
    Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Manager, Peripheral, PeripheralId};
use futures::StreamExt;
use log::{debug, info, warn};
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

use crate::command::DownlinkCommand;
use crate::models::{BlePacket, DecodeError};
use crate::store::{SessionStore, StoreError, TagId};

/// Advertised-name prefix every HyfindTag uses; the only match criterion.
pub const DEFAULT_NAME_PREFIX: &str = "HyfindTag";

/// Default cap on tags serviced concurrently.
///
/// The store-and-forward model holds only a few short-lived links at a time, so
/// this is a deliberate parameter — desktop OS BLE stacks cap concurrent
/// central links well below the fleet size (CLAUDE.md §A.2). It is never an
/// assumption that all advertising tags connect at once.
pub const DEFAULT_MAX_CONCURRENT_CONNECTIONS: usize = 4;

/// How long a connected tag may go without indicating before it is treated as
/// drained and disconnected.
const DRAIN_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// GATT service UUID (`00001522-…`). Tags do **not** advertise it; it is used
/// only to locate characteristics after connecting, never as a scan filter.
pub const SERVICE_UUID: Uuid = Uuid::from_u128(0x00001522_1212_efde_1523_785feabcd123);

/// DATA characteristic UUID (`00001523-…`), an **indicate** characteristic
/// carrying 100-byte [`BlePacket`]s.
pub const DATA_CHAR_UUID: Uuid = Uuid::from_u128(0x00001523_1212_efde_1523_785feabcd123);

/// COMMAND characteristic UUID (`00001525-…`); the 9-byte time-sync command is
/// written here on connect.
pub const COMMAND_CHAR_UUID: Uuid = Uuid::from_u128(0x00001525_1212_efde_1523_785feabcd123);

/// Configuration for the acquisition loop.
#[derive(Debug, Clone)]
pub struct AcquireConfig {
    /// Advertised-name prefix a peripheral must start with to be serviced.
    pub name_prefix: String,
    /// Maximum number of tags serviced concurrently (a parameter, not an
    /// assumption that every tag connects at once).
    pub max_concurrent: usize,
}

impl Default for AcquireConfig {
    fn default() -> Self {
        Self {
            name_prefix: DEFAULT_NAME_PREFIX.to_owned(),
            max_concurrent: DEFAULT_MAX_CONCURRENT_CONNECTIONS,
        }
    }
}

/// Errors raised while running the acquisition loop.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    /// An underlying BLE operation failed.
    #[error("ble error: {0}")]
    Ble(#[from] btleplug::Error),
    /// A session-store operation failed.
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    /// A drained payload could not be decoded.
    #[error("decode error: {0}")]
    Decode(#[from] DecodeError),
    /// No Bluetooth adapter was available on this host.
    #[error("no bluetooth adapter found")]
    NoAdapter,
    /// A required GATT characteristic was absent after service discovery.
    #[error("characteristic {0} not found on tag")]
    CharacteristicMissing(Uuid),
    /// The DATA characteristic does not support indications (contradicts §A.3).
    #[error("DATA characteristic {0} does not support indications")]
    DataNotIndicate(Uuid),
    /// The system clock is before the Unix epoch, so no time-sync can be sent.
    #[error("system clock is before the unix epoch")]
    ClockBeforeEpoch,
}

/// Decodes one raw DATA payload and inserts its samples under `tag_id`.
///
/// This is the pure-logic seam the loop calls for every indicated packet: it
/// performs no BLE or clock access, only [`BlePacket::from_bytes`] (Issue 1)
/// followed by [`SessionStore::insert_samples`] (Issue 3). It is unit-tested
/// without hardware.
///
/// Returns the number of samples stored.
///
/// # Errors
/// Returns [`AcquireError::Decode`] if `raw` is not a valid 100-byte packet, or
/// [`AcquireError::Store`] if the insert fails. In neither case is anything
/// partially stored (the batch insert is transactional).
fn decode_and_store(
    store: &SessionStore,
    tag_id: &TagId,
    raw: &[u8],
) -> Result<usize, AcquireError> {
    let packet = BlePacket::from_bytes(raw)?;
    let samples = packet.samples();
    store.insert_samples(tag_id, samples)?;
    Ok(samples.len())
}

/// Returns the current time as epoch milliseconds.
///
/// The one place in pc_sink that reads the wall clock, used to build the
/// time-sync command (§A.6).
///
/// # Errors
/// Returns [`AcquireError::ClockBeforeEpoch`] if the system clock predates the
/// Unix epoch or overflows an `i64`.
fn now_epoch_ms() -> Result<i64, AcquireError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AcquireError::ClockBeforeEpoch)?;
    i64::try_from(elapsed.as_millis()).map_err(|_| AcquireError::ClockBeforeEpoch)
}

/// Runs the scan → connect → time-sync → drain → disconnect loop, storing
/// decoded samples into `store`, until `shutdown` resolves.
///
/// Scanning matches peripherals by [`AcquireConfig::name_prefix`] (tags do not
/// advertise the service UUID). Each matching tag is serviced on its own task,
/// bounded by [`AcquireConfig::max_concurrent`]; a failure with one tag is
/// logged and never aborts the loop or other tags.
///
/// # Errors
/// Returns [`AcquireError`] only for failures that prevent the loop from running
/// at all (no adapter, scan could not start). Per-tag and per-packet failures
/// are logged and swallowed.
pub async fn run_acquisition(
    config: AcquireConfig,
    store: SessionStore,
    shutdown: impl Future<Output = ()>,
) -> Result<(), AcquireError> {
    let manager = Manager::new().await?;
    let adapter = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or(AcquireError::NoAdapter)?;

    info!(
        "starting acquisition: name prefix {:?}, max concurrent {}",
        config.name_prefix, config.max_concurrent
    );

    // An empty scan filter: we match by advertised NAME, never by service UUID
    // (tags do not advertise the service UUID — §A.3).
    adapter.start_scan(ScanFilter::default()).await?;
    let mut events = adapter.events().await?;

    let config = Arc::new(config);
    let store = Arc::new(Mutex::new(store));
    let permits = Arc::new(Semaphore::new(config.max_concurrent));
    // Tags currently being serviced, so a tag that keeps advertising while we
    // drain it is not picked up by a second task.
    let in_progress: Arc<Mutex<HashSet<PeripheralId>>> = Arc::new(Mutex::new(HashSet::new()));

    tokio::pin!(shutdown);
    loop {
        let event = tokio::select! {
            () = &mut shutdown => break,
            event = events.next() => match event {
                Some(event) => event,
                None => break, // adapter event stream closed
            },
        };

        let id = match event {
            btleplug::api::CentralEvent::DeviceDiscovered(id)
            | btleplug::api::CentralEvent::DeviceUpdated(id) => id,
            _ => continue,
        };

        // Bound concurrency: if no permit is free, skip this advertisement; the
        // tag will re-advertise and be picked up once a slot frees.
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            continue;
        };

        let peripheral = match adapter.peripheral(&id).await {
            Ok(peripheral) => peripheral,
            Err(error) => {
                warn!("could not resolve peripheral {id}: {error}");
                continue;
            }
        };

        let Some(name) = matching_name(&peripheral, &config.name_prefix).await else {
            continue; // not a HyfindTag (or name not yet received)
        };

        // Reserve this tag so concurrent advertisements don't double-service it.
        if !in_progress.lock().await.insert(id.clone()) {
            continue;
        }

        let store = Arc::clone(&store);
        let in_progress = Arc::clone(&in_progress);
        let task_id = id.clone();
        tokio::spawn(async move {
            // Hold the permit for the lifetime of the drain cycle.
            let _permit = permit;
            if let Err(error) = service_tag(&peripheral, &store, &name).await {
                warn!("tag {task_id} ({name}) errored mid-service: {error}");
            }
            // Best-effort disconnect; the tag also drops the link when drained.
            if let Err(error) = peripheral.disconnect().await {
                debug!("disconnect of {task_id} failed (likely already gone): {error}");
            }
            in_progress.lock().await.remove(&task_id);
        });
    }

    info!("acquisition loop shutting down");
    adapter.stop_scan().await?;
    Ok(())
}

/// Returns the peripheral's advertised name if it starts with `name_prefix`.
async fn matching_name(peripheral: &Peripheral, name_prefix: &str) -> Option<String> {
    let name = peripheral.properties().await.ok()??.local_name?;
    name.starts_with(name_prefix).then_some(name)
}

/// Services one tag end to end: connect → time-sync → subscribe → drain.
///
/// The tag's stable id is its BLE address; `name` is its human-readable label
/// (CLAUDE.md §A.2). The time-sync write happens **before** the drain loop.
async fn service_tag(
    peripheral: &Peripheral,
    store: &Mutex<SessionStore>,
    name: &str,
) -> Result<(), AcquireError> {
    let tag_id = TagId::new(peripheral.address().to_string());
    info!("servicing tag {tag_id} ({name})");

    peripheral.connect().await?;
    peripheral.discover_services().await?;

    let command_char = find_characteristic(peripheral, COMMAND_CHAR_UUID)?;
    let data_char = find_characteristic(peripheral, DATA_CHAR_UUID)?;
    // §A.3: DATA must be an *indicate* characteristic, not notify.
    if !data_char.properties.contains(CharPropFlags::INDICATE) {
        return Err(AcquireError::DataNotIndicate(DATA_CHAR_UUID));
    }

    // Record the label up front so the tag row exists even if the drain is empty.
    store.lock().await.upsert_tag(&tag_id, name)?;

    // Time-sync BEFORE draining (§A.6): the tag stamps every BlePacket.time in
    // real wall-clock ms from this. `now_epoch_ms` is the only clock read.
    let command = DownlinkCommand::SetTime {
        time_ms: now_epoch_ms()?,
    };
    peripheral
        .write(&command_char, &command.to_bytes(), WriteType::WithResponse)
        .await?;
    debug!("time-synced {tag_id}; subscribing to DATA indications");

    // Subscribe to indications (btleplug selects indicate vs notify from the
    // characteristic's properties, which we verified above).
    peripheral.subscribe(&data_char).await?;
    drain_indications(peripheral, store, &tag_id).await;
    peripheral.unsubscribe(&data_char).await?;
    Ok(())
}

/// Drains indicated packets until the tag goes idle or disconnects.
///
/// A malformed/short payload is logged and skipped — one bad packet never
/// aborts the drain (per-packet error isolation).
async fn drain_indications(peripheral: &Peripheral, store: &Mutex<SessionStore>, tag_id: &TagId) {
    let mut indications = match peripheral.notifications().await {
        Ok(indications) => indications,
        Err(error) => {
            warn!("could not open indication stream for {tag_id}: {error}");
            return;
        }
    };

    // The loop ends when the indication stream closes (tag disconnected) or the
    // idle timeout fires (tag drained) — both leave the `while let` pattern.
    let mut stored = 0usize;
    while let Ok(Some(value)) = tokio::time::timeout(DRAIN_IDLE_TIMEOUT, indications.next()).await {
        if value.uuid != DATA_CHAR_UUID {
            continue;
        }
        let guard = store.lock().await;
        match decode_and_store(&guard, tag_id, &value.value) {
            Ok(count) => stored += count,
            Err(error) => warn!("dropping bad packet from {tag_id}: {error}"),
        }
    }
    info!("drained {tag_id}: stored {stored} samples");
}

/// Finds a characteristic by UUID on a connected peripheral.
fn find_characteristic(
    peripheral: &Peripheral,
    uuid: Uuid,
) -> Result<Characteristic, AcquireError> {
    peripheral
        .characteristics()
        .into_iter()
        .find(|characteristic| characteristic.uuid == uuid)
        .ok_or(AcquireError::CharacteristicMissing(uuid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SAMPLES_PER_PACKET;

    /// A valid all-zero 100-byte packet: decodes to [`SAMPLES_PER_PACKET`]
    /// samples (every field zero is in range), enough to exercise the seam.
    fn known_payload() -> Vec<u8> {
        vec![0u8; crate::models::PACKET_SIZE]
    }

    #[test]
    fn decode_and_store_inserts_samples_under_tag_id() {
        let store = SessionStore::open_in_memory().expect("open in-memory db");
        let tag = TagId::new("AA:BB:CC:DD:EE:FF");

        let stored = decode_and_store(&store, &tag, &known_payload()).expect("decode and store");

        assert_eq!(stored, SAMPLES_PER_PACKET);
        assert_eq!(
            store.samples_for(&tag).expect("read back").len(),
            SAMPLES_PER_PACKET
        );
    }

    #[test]
    fn decode_and_store_rejects_short_payload_without_panicking() {
        let store = SessionStore::open_in_memory().expect("open in-memory db");
        let tag = TagId::new("AA:BB:CC:DD:EE:FF");

        let result = decode_and_store(&store, &tag, &[0u8; 50]);

        assert!(matches!(result, Err(AcquireError::Decode(_))));
        // Nothing was stored for the tag — the error path is clean.
        assert!(store.samples_for(&tag).expect("read back").is_empty());
    }

    #[test]
    fn default_config_uses_named_constants() {
        let config = AcquireConfig::default();
        assert_eq!(config.name_prefix, DEFAULT_NAME_PREFIX);
        assert_eq!(config.max_concurrent, DEFAULT_MAX_CONCURRENT_CONNECTIONS);
    }

    #[test]
    fn gatt_uuids_share_the_firmware_base() {
        // Base `…-1212-efde-1523-785feabcd123`; 16-bit slot differs per role.
        assert_eq!(
            SERVICE_UUID.to_string(),
            "00001522-1212-efde-1523-785feabcd123"
        );
        assert_eq!(
            DATA_CHAR_UUID.to_string(),
            "00001523-1212-efde-1523-785feabcd123"
        );
        assert_eq!(
            COMMAND_CHAR_UUID.to_string(),
            "00001525-1212-efde-1523-785feabcd123"
        );
    }
}
