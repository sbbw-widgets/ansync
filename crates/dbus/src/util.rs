//! Small helpers shared across interface impls.

use ansync_core::DeviceId;

/// Parse a 32-character hex device id (as displayed by `DeviceId`).
pub fn parse_device_id(s: &str) -> Option<DeviceId> {
    if s.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        bytes[i] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(DeviceId(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let id = DeviceId([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]);
        let s = id.to_string();
        let back = parse_device_id(&s).unwrap();
        assert_eq!(back.0, id.0);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_device_id("xyz").is_none());
        assert!(parse_device_id("0011").is_none());
    }
}
