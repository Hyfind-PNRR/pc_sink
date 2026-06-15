//! Pure encoding of downlink commands pc_sink writes to a tag's COMMAND
//! characteristic.
//!
//! This module performs **no** BLE, I/O, or clock access — it is a pure
//! transform from a typed command into the exact packed little-endian bytes the
//! firmware expects. The caller supplies the timestamp; reading the clock and
//! performing the BLE write are out of scope (see §A.6 and Issue 4).
//!
//! Wire layout (packed, little-endian), mirroring firmware
//! `hyfind_downlink_cmd { u8 type; i64 time_ms }`:
//!
//! | Offset | Size | Field     | Type   | Meaning                              |
//! |--------|------|-----------|--------|--------------------------------------|
//! | 0      | 1    | `type`    | u8     | command type (`HYFIND_CMD_TIME = 0`) |
//! | 1      | 8    | `time_ms` | i64 LE | epoch milliseconds to set            |
//!
//! The firmware handler requires an exact length of [`SET_TIME_LEN`] (9) bytes.

/// Command type byte for the time-sync command (`HYFIND_CMD_TIME`).
pub const HYFIND_CMD_TIME: u8 = 0;

/// Encoded length, in bytes, of the [`DownlinkCommand::SetTime`] command.
///
/// One type byte followed by an `i64` little-endian timestamp. The firmware
/// handler accepts only this exact length.
pub const SET_TIME_LEN: usize = 9;

/// Downlink commands `pc_sink` can send to a tag's COMMAND characteristic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownlinkCommand {
    /// Set the tag's wall-clock time, in epoch milliseconds.
    ///
    /// The firmware computes `offset = time_ms - uptime` and thereafter stamps
    /// every `BlePacket.time` in real epoch milliseconds.
    SetTime {
        /// Epoch milliseconds to set as the tag's wall-clock time.
        time_ms: i64,
    },
}

impl DownlinkCommand {
    /// Encodes the command as the exact packed little-endian bytes the firmware
    /// expects.
    ///
    /// `SetTime` produces [`SET_TIME_LEN`] (9) bytes: `[0x00][i64 LE time_ms]`.
    pub fn to_bytes(&self) -> [u8; SET_TIME_LEN] {
        match self {
            DownlinkCommand::SetTime { time_ms } => {
                let mut bytes = [0u8; SET_TIME_LEN];
                bytes[0] = HYFIND_CMD_TIME;
                bytes[1..].copy_from_slice(&time_ms.to_le_bytes());
                bytes
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_time_encodes_to_exactly_nine_bytes() {
        let bytes = DownlinkCommand::SetTime { time_ms: 0 }.to_bytes();
        assert_eq!(bytes.len(), SET_TIME_LEN);
        assert_eq!(bytes.len(), 9);
    }

    #[test]
    fn set_time_type_byte_is_hyfind_cmd_time_and_payload_is_le() {
        let time_ms: i64 = 1_700_000_000_000;
        let bytes = DownlinkCommand::SetTime { time_ms }.to_bytes();
        assert_eq!(bytes[0], HYFIND_CMD_TIME);
        assert_eq!(bytes[0], 0x00);
        assert_eq!(&bytes[1..9], &time_ms.to_le_bytes());
    }

    #[test]
    fn set_time_locks_full_known_vector() {
        // 1_700_000_000_000 ms = 0x0000018BCFE56800 → LE: 00 68 E5 CF 8B 01 00 00.
        let bytes = DownlinkCommand::SetTime {
            time_ms: 1_700_000_000_000,
        }
        .to_bytes();
        assert_eq!(
            bytes,
            [0x00, 0x00, 0x68, 0xE5, 0xCF, 0x8B, 0x01, 0x00, 0x00]
        );
    }

    #[test]
    fn set_time_encodes_negative_timestamp_as_twos_complement_le() {
        let time_ms: i64 = -1;
        let bytes = DownlinkCommand::SetTime { time_ms }.to_bytes();
        assert_eq!(bytes[0], HYFIND_CMD_TIME);
        assert_eq!(&bytes[1..9], &[0xFF; 8]);
    }

    #[test]
    fn set_time_round_trips_through_le_bytes() {
        for time_ms in [i64::MIN, -42, 0, 42, i64::MAX] {
            let bytes = DownlinkCommand::SetTime { time_ms }.to_bytes();
            let recovered = i64::from_le_bytes(
                bytes[1..9]
                    .try_into()
                    .expect("slice of 8 bytes is an [u8; 8]"),
            );
            assert_eq!(recovered, time_ms);
        }
    }
}
