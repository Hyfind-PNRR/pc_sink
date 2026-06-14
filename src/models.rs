//! Pure decoding of the 100-byte BLE uplink packet (`BlePacket`) into typed,
//! fully-decoded samples.
//!
//! This module performs **no** BLE, I/O, or clock access — it is a pure
//! transform from raw little-endian bytes to engineering units, and is the
//! foundation every later acquisition/storage stage builds on.
//!
//! Wire layout (packed, little-endian), mirroring firmware `ble_packet` and the
//! canonical Python cross-check `struct.Struct("<q" + ("HHhh"*10) + "H" + ("B"*10))`:
//!
//! | Offset | Size | Field        | Type    | Meaning                                |
//! |--------|------|--------------|---------|----------------------------------------|
//! | 0      | 8    | `time`       | i64 LE  | wall-clock ms of the **first** sample  |
//! | 8      | 80   | `dp[0..10]`  | 10 × {u16 temp, u16 hum, i16 adc1, i16 adc2} | samples |
//! | 88     | 2    | `dt`         | u16 LE  | ms between consecutive samples         |
//! | 90     | 10   | `stimuli[..]`| u8 × 10 | stimulus byte active for each sample   |

use serde::{Deserialize, Serialize};

/// Total size of a raw `BlePacket` payload in bytes.
pub const PACKET_SIZE: usize = 100;

/// Number of samples carried in a single packet.
pub const SAMPLES_PER_PACKET: usize = 10;

/// Sense resistor R9, in ohms, used by the current conversion (firmware §A.4).
pub const R9_OHMS: f64 = 3000.0;

/// Byte offset of the `time` field.
const TIME_OFFSET: usize = 0;
/// Byte offset of the first `data_packet`.
const DATA_OFFSET: usize = 8;
/// Size of one `data_packet` (`u16 temp, u16 hum, i16 adc1, i16 adc2`).
const DATA_PACKET_SIZE: usize = 8;
/// Byte offset of the `dt` field.
const DT_OFFSET: usize = 88;
/// Byte offset of the first `stimuli` byte.
const STIMULI_OFFSET: usize = 90;

/// One stimulus level: a 4-bit `VoltageStimuli` code and its millivolt value.
///
/// Mirrors firmware `VoltageStimuli` (`hytag_stimuli.h`). Exhaustive over the 16
/// possible nibble codes; every nibble value is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoltageStimuli {
    /// `0x0` — channel off (0 mV).
    Off,
    /// `0x1` — 131 mV.
    Mv131,
    /// `0x2` — 161 mV.
    Mv161,
    /// `0x3` — 277 mV.
    Mv277,
    /// `0x4` — 211 mV.
    Mv211,
    /// `0x5` — 322 mV.
    Mv322,
    /// `0x6` — 347 mV.
    Mv347,
    /// `0x7` — 448 mV.
    Mv448,
    /// `0x8` — 304 mV (firmware default on both channels, byte `0x88`).
    Mv304,
    /// `0x9` — 408 mV.
    Mv408,
    /// `0xA` — 431 mV.
    Mv431,
    /// `0xB` — 524 mV.
    Mv524,
    /// `0xC` — 470 mV.
    Mv470,
    /// `0xD` — 561 mV.
    Mv561,
    /// `0xE` — 582 mV.
    Mv582,
    /// `0xF` — 663 mV.
    Mv663,
}

impl VoltageStimuli {
    /// Maps a 4-bit code to its stimulus level.
    ///
    /// Only the low nibble of `code` is significant; any higher bits are
    /// ignored, so this is total and never panics.
    pub fn from_nibble(code: u8) -> Self {
        match code & 0x0F {
            0x0 => Self::Off,
            0x1 => Self::Mv131,
            0x2 => Self::Mv161,
            0x3 => Self::Mv277,
            0x4 => Self::Mv211,
            0x5 => Self::Mv322,
            0x6 => Self::Mv347,
            0x7 => Self::Mv448,
            0x8 => Self::Mv304,
            0x9 => Self::Mv408,
            0xA => Self::Mv431,
            0xB => Self::Mv524,
            0xC => Self::Mv470,
            0xD => Self::Mv561,
            0xE => Self::Mv582,
            // 0xF is the only remaining masked value.
            _ => Self::Mv663,
        }
    }

    /// Returns the stimulus value in millivolts.
    pub fn millivolts(self) -> u16 {
        match self {
            Self::Off => 0,
            Self::Mv131 => 131,
            Self::Mv161 => 161,
            Self::Mv277 => 277,
            Self::Mv211 => 211,
            Self::Mv322 => 322,
            Self::Mv347 => 347,
            Self::Mv448 => 448,
            Self::Mv304 => 304,
            Self::Mv408 => 408,
            Self::Mv431 => 431,
            Self::Mv524 => 524,
            Self::Mv470 => 470,
            Self::Mv561 => 561,
            Self::Mv582 => 582,
            Self::Mv663 => 663,
        }
    }
}

/// Computes per-channel current from a signed ADC reading and stimulus level.
///
/// Reproduces firmware/`tag-tester` exactly: `(adc_mv - stim_mv) / 3000`.
///
/// > Unit caveat (§A.4): the divisor `3000` yields **mA** (mV ÷ 3 kΩ), although
/// > `tag-tester` labels the output "µA". The formula is reproduced as specified;
/// > the unit-label discrepancy is left for the maintainer to resolve rather than
/// > silently "corrected" here.
pub fn parse_current(adc_mv: i16, stim_mv: u16) -> f64 {
    (f64::from(adc_mv) - f64::from(stim_mv)) / R9_OHMS
}

/// One fully-decoded sample in engineering units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Wall-clock timestamp in ms: `time + i * dt`.
    pub timestamp_ms: i64,
    /// Temperature in °C (`raw_temp / 100`).
    pub temperature_c: f64,
    /// Relative humidity in % (`raw_hum / 100`).
    pub humidity_pct: f64,
    /// Raw signed ADC channel 1 reading, in mV.
    pub adc1_mv: i16,
    /// Raw signed ADC channel 2 reading, in mV.
    pub adc2_mv: i16,
    /// Channel A stimulus in mV (low nibble of the stimulus byte).
    pub stim_a_mv: u16,
    /// Channel B stimulus in mV (high nibble of the stimulus byte).
    pub stim_b_mv: u16,
    /// Channel A current: `(adc1_mv - stim_a_mv) / 3000`.
    pub current_a: f64,
    /// Channel B current: `(adc2_mv - stim_b_mv) / 3000`.
    pub current_b: f64,
}

/// Error returned when a raw payload cannot be decoded into a [`BlePacket`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The payload was not exactly [`PACKET_SIZE`] bytes long.
    #[error("packet must be exactly {expected} bytes, got {actual}")]
    InvalidLength {
        /// Required length ([`PACKET_SIZE`]).
        expected: usize,
        /// Length actually supplied.
        actual: usize,
    },
}

/// A decoded 100-byte packet: the batch start time, sample interval, and 10
/// fully-decoded [`Sample`]s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlePacket {
    /// Wall-clock ms of the first sample in the batch.
    time_ms: i64,
    /// Milliseconds between consecutive samples.
    dt_ms: u16,
    /// The 10 decoded samples.
    samples: [Sample; SAMPLES_PER_PACKET],
}

impl BlePacket {
    /// Decodes a packet from exactly [`PACKET_SIZE`] little-endian bytes.
    ///
    /// # Errors
    /// Returns [`DecodeError::InvalidLength`] if `bytes.len() != PACKET_SIZE`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let bytes: [u8; PACKET_SIZE] =
            bytes.try_into().map_err(|_| DecodeError::InvalidLength {
                expected: PACKET_SIZE,
                actual: bytes.len(),
            })?;

        let time_ms = i64::from_le_bytes([
            bytes[TIME_OFFSET],
            bytes[TIME_OFFSET + 1],
            bytes[TIME_OFFSET + 2],
            bytes[TIME_OFFSET + 3],
            bytes[TIME_OFFSET + 4],
            bytes[TIME_OFFSET + 5],
            bytes[TIME_OFFSET + 6],
            bytes[TIME_OFFSET + 7],
        ]);
        let dt_ms = u16::from_le_bytes([bytes[DT_OFFSET], bytes[DT_OFFSET + 1]]);

        let samples = core::array::from_fn(|i| decode_sample(&bytes, i, time_ms, dt_ms));

        Ok(Self {
            time_ms,
            dt_ms,
            samples,
        })
    }

    /// Wall-clock ms of the first sample in the batch.
    pub fn time_ms(&self) -> i64 {
        self.time_ms
    }

    /// Milliseconds between consecutive samples.
    pub fn dt_ms(&self) -> u16 {
        self.dt_ms
    }

    /// The decoded samples (length [`SAMPLES_PER_PACKET`]).
    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }
}

/// Decodes the `i`-th sample from a validated, fixed-size packet buffer.
fn decode_sample(bytes: &[u8; PACKET_SIZE], i: usize, time_ms: i64, dt_ms: u16) -> Sample {
    let base = DATA_OFFSET + i * DATA_PACKET_SIZE;
    let raw_temp = u16::from_le_bytes([bytes[base], bytes[base + 1]]);
    let raw_hum = u16::from_le_bytes([bytes[base + 2], bytes[base + 3]]);
    let adc1_mv = i16::from_le_bytes([bytes[base + 4], bytes[base + 5]]);
    let adc2_mv = i16::from_le_bytes([bytes[base + 6], bytes[base + 7]]);

    let stim_byte = bytes[STIMULI_OFFSET + i];
    // Low nibble = channel A, high nibble = channel B (firmware §A.5).
    let stim_a_mv = VoltageStimuli::from_nibble(stim_byte & 0x0F).millivolts();
    let stim_b_mv = VoltageStimuli::from_nibble(stim_byte >> 4).millivolts();

    Sample {
        timestamp_ms: time_ms + (i as i64) * i64::from(dt_ms),
        temperature_c: f64::from(raw_temp) / 100.0,
        humidity_pct: f64::from(raw_hum) / 100.0,
        adc1_mv,
        adc2_mv,
        stim_a_mv,
        stim_b_mv,
        current_a: parse_current(adc1_mv, stim_a_mv),
        current_b: parse_current(adc2_mv, stim_b_mv),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a 100-byte packet mirroring the firmware Python `struct` layout
    /// `"<q" + "HHhh"*10 + "H" + "B"*10`.
    fn build_packet(
        time: i64,
        samples: &[(u16, u16, i16, i16); SAMPLES_PER_PACKET],
        dt: u16,
        stimuli: &[u8; SAMPLES_PER_PACKET],
    ) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(PACKET_SIZE);
        bytes.extend_from_slice(&time.to_le_bytes());
        for (temp, hum, adc1, adc2) in samples {
            bytes.extend_from_slice(&temp.to_le_bytes());
            bytes.extend_from_slice(&hum.to_le_bytes());
            bytes.extend_from_slice(&adc1.to_le_bytes());
            bytes.extend_from_slice(&adc2.to_le_bytes());
        }
        bytes.extend_from_slice(&dt.to_le_bytes());
        bytes.extend_from_slice(stimuli);
        assert_eq!(bytes.len(), PACKET_SIZE);
        bytes
    }

    #[test]
    fn decodes_known_packet() {
        let samples = [
            (2567, 4500, 100, -100),
            (2568, 4501, 101, -101),
            (2569, 4502, 102, -102),
            (2570, 4503, 103, -103),
            (2571, 4504, 104, -104),
            (2572, 4505, 105, -105),
            (2573, 4506, 106, -106),
            (2574, 4507, 107, -107),
            (2575, 4508, 108, -108),
            (2576, 4509, 109, -109),
        ];
        let stimuli = [0x88u8; SAMPLES_PER_PACKET];
        let raw = build_packet(1_000_000, &samples, 50, &stimuli);

        let packet = BlePacket::from_bytes(&raw).expect("valid packet");
        assert_eq!(packet.time_ms(), 1_000_000);
        assert_eq!(packet.dt_ms(), 50);
        assert_eq!(packet.samples().len(), SAMPLES_PER_PACKET);

        let first = &packet.samples()[0];
        assert_eq!(first.timestamp_ms, 1_000_000);
        assert!((first.temperature_c - 25.67).abs() < 1e-9);
        assert!((first.humidity_pct - 45.00).abs() < 1e-9);
        assert_eq!(first.adc1_mv, 100);
        assert_eq!(first.adc2_mv, -100);
        assert_eq!(first.stim_a_mv, 304);
        assert_eq!(first.stim_b_mv, 304);
    }

    #[test]
    fn rejects_wrong_length_input() {
        assert_eq!(
            BlePacket::from_bytes(&[0u8; 99]),
            Err(DecodeError::InvalidLength {
                expected: 100,
                actual: 99,
            })
        );
        assert_eq!(
            BlePacket::from_bytes(&[0u8; 101]),
            Err(DecodeError::InvalidLength {
                expected: 100,
                actual: 101,
            })
        );
        assert!(BlePacket::from_bytes(&[]).is_err());
    }

    #[test]
    fn scales_temperature_and_humidity_by_hundred() {
        let mut samples = [(0u16, 0u16, 0i16, 0i16); SAMPLES_PER_PACKET];
        samples[0] = (2567, 8012, 0, 0);
        let raw = build_packet(0, &samples, 1, &[0u8; SAMPLES_PER_PACKET]);

        let packet = BlePacket::from_bytes(&raw).expect("valid packet");
        let s = &packet.samples()[0];
        assert!((s.temperature_c - 25.67).abs() < 1e-9);
        assert!((s.humidity_pct - 80.12).abs() < 1e-9);
    }

    #[test]
    fn maps_all_sixteen_stimulus_codes_to_millivolts() {
        let expected: [(u8, u16); 16] = [
            (0x0, 0),
            (0x1, 131),
            (0x2, 161),
            (0x3, 277),
            (0x4, 211),
            (0x5, 322),
            (0x6, 347),
            (0x7, 448),
            (0x8, 304),
            (0x9, 408),
            (0xA, 431),
            (0xB, 524),
            (0xC, 470),
            (0xD, 561),
            (0xE, 582),
            (0xF, 663),
        ];
        for (code, mv) in expected {
            assert_eq!(VoltageStimuli::from_nibble(code).millivolts(), mv);
        }
    }

    #[test]
    fn splits_stimulus_byte_into_channel_nibbles() {
        // 0x88 -> both channels 304 mV.
        let raw = build_packet(
            0,
            &[(0, 0, 0, 0); SAMPLES_PER_PACKET],
            1,
            &[0x88u8; SAMPLES_PER_PACKET],
        );
        let packet = BlePacket::from_bytes(&raw).expect("valid packet");
        assert_eq!(packet.samples()[0].stim_a_mv, 304);
        assert_eq!(packet.samples()[0].stim_b_mv, 304);

        // 0x1F -> low nibble (ch A) = 663, high nibble (ch B) = 131.
        let mut stimuli = [0u8; SAMPLES_PER_PACKET];
        stimuli[0] = 0x1F;
        let raw = build_packet(0, &[(0, 0, 0, 0); SAMPLES_PER_PACKET], 1, &stimuli);
        let packet = BlePacket::from_bytes(&raw).expect("valid packet");
        assert_eq!(packet.samples()[0].stim_a_mv, 663);
        assert_eq!(packet.samples()[0].stim_b_mv, 131);
    }

    #[test]
    fn parse_current_uses_exact_3000_divisor() {
        // Positive: (1000 - 304) / 3000.
        assert!((parse_current(1000, 304) - (696.0 / 3000.0)).abs() < 1e-12);
        // adc < stim -> negative result.
        assert!((parse_current(100, 304) - (-204.0 / 3000.0)).abs() < 1e-12);
        assert!(parse_current(100, 304) < 0.0);
        // Zero stimulus.
        assert!((parse_current(3000, 0) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn derives_per_sample_timestamps_from_time_and_dt() {
        let raw = build_packet(
            10_000,
            &[(0, 0, 0, 0); SAMPLES_PER_PACKET],
            25,
            &[0u8; SAMPLES_PER_PACKET],
        );
        let packet = BlePacket::from_bytes(&raw).expect("valid packet");
        let samples = packet.samples();
        // First sample: time + 0 * dt.
        assert_eq!(samples[0].timestamp_ms, 10_000);
        // Last sample: time + 9 * dt.
        assert_eq!(
            samples[SAMPLES_PER_PACKET - 1].timestamp_ms,
            10_000 + 9 * 25
        );
    }

    #[test]
    fn computes_currents_from_adc_and_stimulus() {
        let mut samples = [(0u16, 0u16, 0i16, 0i16); SAMPLES_PER_PACKET];
        samples[0] = (0, 0, 1000, 100);
        // 0x88 -> both stim 304 mV.
        let raw = build_packet(0, &samples, 1, &[0x88u8; SAMPLES_PER_PACKET]);
        let packet = BlePacket::from_bytes(&raw).expect("valid packet");
        let s = &packet.samples()[0];
        assert!((s.current_a - (1000.0 - 304.0) / 3000.0).abs() < 1e-12);
        assert!((s.current_b - (100.0 - 304.0) / 3000.0).abs() < 1e-12);
        assert!(s.current_b < 0.0);
    }
}
