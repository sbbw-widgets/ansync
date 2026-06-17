//! WiFi-pair PIN confirmation helper.
//!
//! Used by both sides of the WiFi pairing flow to prove possession of
//! a 6-digit PIN that the companion displays on screen and the user
//! types on the host. The protocol is a one-round MAC exchange:
//!
//! ```text
//!   host        →  BootstrapHello(host_pk, host_name)        →  companion
//!   companion   →  BootstrapAck(companion_pk, companion_name)→  host
//!   host        →  PinConfirm(mac_h)                         →  companion
//!   companion   →  PinConfirm(mac_c)                         →  host
//! ```
//!
//! `mac_h = SHA-256("ansync-pair-v1" || "host"      || host_pk || companion_pk || pin)`
//! `mac_c = SHA-256("ansync-pair-v1" || "companion" || host_pk || companion_pk || pin)`
//!
//! Domain separation by role prevents a passive observer from reusing
//! `mac_h` as the companion's reply. The companion rate-limits to 3
//! attempts before rotating the PIN — with a 6-digit decimal PIN
//! (1,000,000 values) the success probability of a blind guess over
//! the lockout window is 3 × 10⁻⁶.

use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

/// Role tag mixed into the MAC. `Host` is the MAC computed by the
/// pairing initiator (the desktop daemon); `Companion` is the MAC
/// computed by the Android device.
#[derive(Debug, Clone, Copy)]
pub enum PinRole {
    Host,
    Companion,
}

impl PinRole {
    const fn tag(self) -> &'static [u8] {
        match self {
            PinRole::Host => b"host",
            PinRole::Companion => b"companion",
        }
    }
}

/// Domain-separated SHA-256 over the transcript + PIN. See module
/// docs for the exact preimage.
pub fn pair_pin_confirm(
    pin: &[u8; 6],
    role: PinRole,
    host_pk: &[u8; 32],
    companion_pk: &[u8; 32],
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"ansync-pair-v1\0");
    h.update(role.tag());
    h.update(b"\0");
    h.update(host_pk);
    h.update(companion_pk);
    h.update(pin);
    h.finalize().into()
}

/// Constant-time MAC comparison. Returns `true` iff `mac` matches the
/// expected MAC for the given `(pin, role, host_pk, companion_pk)`.
pub fn verify_pin_confirm(
    mac: &[u8; 32],
    pin: &[u8; 6],
    role: PinRole,
    host_pk: &[u8; 32],
    companion_pk: &[u8; 32],
) -> bool {
    let expected = pair_pin_confirm(pin, role, host_pk, companion_pk);
    constant_time_eq(mac, &expected)
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut acc = 0u8;
    for i in 0..32 {
        acc |= a[i] ^ b[i];
    }
    acc == 0
}

/// Generate a 6-digit decimal PIN from a CSPRNG. Returned as ASCII
/// bytes (e.g. `b"012345"`) so it can be sent on the wire and rendered
/// on screen without further conversion.
pub fn generate_pin() -> [u8; 6] {
    // Sample u32 mod 1_000_000 — the modulo bias for 6 decimals over
    // a 32-bit range is < 10⁻¹⁰ which is well below any practical
    // detection threshold for a 3-attempt lockout.
    let mut buf = [0u8; 4];
    OsRng.fill_bytes(&mut buf);
    let val = u32::from_le_bytes(buf) % 1_000_000;
    let mut out = [b'0'; 6];
    let mut n = val;
    for i in (0..6).rev() {
        out[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_confirm_roundtrip() {
        let pin = *b"123456";
        let host_pk = [0xAA; 32];
        let companion_pk = [0xBB; 32];
        let mac_h = pair_pin_confirm(&pin, PinRole::Host, &host_pk, &companion_pk);
        let mac_c = pair_pin_confirm(&pin, PinRole::Companion, &host_pk, &companion_pk);
        assert_ne!(mac_h, mac_c, "role tag must domain-separate");
        assert!(verify_pin_confirm(&mac_h, &pin, PinRole::Host, &host_pk, &companion_pk));
        assert!(verify_pin_confirm(&mac_c, &pin, PinRole::Companion, &host_pk, &companion_pk));
    }

    #[test]
    fn pin_confirm_wrong_pin_rejected() {
        let host_pk = [0xAA; 32];
        let companion_pk = [0xBB; 32];
        let good_mac = pair_pin_confirm(b"111111", PinRole::Host, &host_pk, &companion_pk);
        assert!(!verify_pin_confirm(
            &good_mac, b"111112", PinRole::Host, &host_pk, &companion_pk,
        ));
    }

    #[test]
    fn pin_confirm_swapped_pubkeys_rejected() {
        let host_pk = [0xAA; 32];
        let companion_pk = [0xBB; 32];
        let mac = pair_pin_confirm(b"000000", PinRole::Host, &host_pk, &companion_pk);
        assert!(!verify_pin_confirm(
            &mac, b"000000", PinRole::Host, &companion_pk, &host_pk,
        ));
    }

    #[test]
    fn generated_pins_are_ascii_digits() {
        for _ in 0..32 {
            let pin = generate_pin();
            for b in pin {
                assert!(b.is_ascii_digit(), "non-digit byte {b:#x} in {pin:?}");
            }
        }
    }
}
