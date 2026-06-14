//! pc_sink: a BLE central that discovers, drains, stores, and (later) plots data
//! from many HyfindTag sensor devices.
//!
//! [`models`] decodes the raw 100-byte BLE uplink packet into typed samples in
//! engineering units, [`command`] encodes the downlink time-sync command
//! written on every connect, and [`store`] persists decoded samples to a
//! per-session SQLite database. [`acquire`] drives the live BLE loop that ties
//! them together: scan → connect → time-sync → drain → disconnect, repeated
//! across many tags.

pub mod acquire;
pub mod command;
pub mod models;
pub mod store;
