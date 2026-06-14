//! pc_sink: a BLE central that discovers, drains, stores, and (later) plots data
//! from many HyfindTag sensor devices.
//!
//! This phase implements the pure data path. [`models`] decodes the raw 100-byte
//! BLE uplink packet into typed samples in engineering units, [`command`]
//! encodes the downlink time-sync command written on every connect, and
//! [`store`] persists decoded samples to a per-session SQLite database.

pub mod command;
pub mod models;
pub mod store;
