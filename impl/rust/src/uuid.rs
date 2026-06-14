// UUID bit-level operations (spec/design/functions.md §12). Value<->text rendering/parsing
// lives in value.rs (`render_uuid`/`parse_uuid`); this module is the SEMANTIC bit work —
// extracting the version and the embedded timestamp from the 16 raw big-endian bytes (byte 0
// is the most-significant), plus the pure generator byte builders (build_v4/build_v7 below). The
// PRNG draws + clock for the generators live on the entropy+clock seam (seam.rs); the functions
// here are PURE — deterministic functions of their input bytes.

/// 100-ns intervals between the Gregorian UUID epoch (1582-10-15 00:00:00 UTC) and the Unix
/// epoch (1970-01-01 00:00:00 UTC) — the v1/v6 timestamp base (= 0x01B21DD213814000).
const GREGORIAN_OFFSET_100NS: i64 = 122_192_928_000_000_000;

/// True iff the value carries the RFC 4122 variant (the top two bits of byte 8 are `10`).
/// Microsoft GUIDs (`11`), the legacy NCS variant (`0`), and the nil UUID (all zero) are not.
fn is_rfc4122(b: &[u8; 16]) -> bool {
    (b[8] & 0xC0) == 0x80
}

/// The version nibble (high nibble of byte 6), 0..15, for an RFC 4122 UUID; `None` for a
/// non-RFC variant. Matches PostgreSQL 18 `uuid_extract_version` (returns NULL off-variant).
pub fn extract_version(b: &[u8; 16]) -> Option<i64> {
    if !is_rfc4122(b) {
        return None;
    }
    Some(i64::from((b[6] >> 4) & 0x0F))
}

/// The embedded instant as microseconds since the Unix epoch (a `timestamptz` value), for an
/// RFC 4122 UUID of VERSION 1 or 7 only; `None` for every other version and for a non-RFC
/// variant. Matches PostgreSQL 18 `uuid_extract_timestamp` — which extracts from v1 and v7
/// only (v6 returns NULL there, oracle-verified).
pub fn extract_timestamp_micros(b: &[u8; 16]) -> Option<i64> {
    if !is_rfc4122(b) {
        return None;
    }
    match (b[6] >> 4) & 0x0F {
        7 => Some(v7_micros(b)),
        1 => Some(v1_micros(b)),
        _ => None,
    }
}

/// v7: the first 6 bytes are a 48-bit big-endian Unix-millisecond count; micros = ms * 1000.
/// A 48-bit ms (< 2.8e14) times 1000 stays well within i64, so this cannot overflow.
fn v7_micros(b: &[u8; 16]) -> i64 {
    let ms = (i64::from(b[0]) << 40)
        | (i64::from(b[1]) << 32)
        | (i64::from(b[2]) << 24)
        | (i64::from(b[3]) << 16)
        | (i64::from(b[4]) << 8)
        | i64::from(b[5]);
    ms * 1000
}

/// v1: reassemble the 60-bit Gregorian 100-ns count from time_low (bytes 0..3), time_mid
/// (bytes 4..5), and time_hi (the low 12 bits of bytes 6..7, version nibble masked off),
/// subtract the 1582→1970 epoch offset, then truncate 100-ns ticks to microseconds (`/10`,
/// toward zero — PG drops the sub-microsecond remainder, oracle-verified).
fn v1_micros(b: &[u8; 16]) -> i64 {
    let time_low = u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let time_mid = u64::from(u16::from_be_bytes([b[4], b[5]]));
    let time_hi = u64::from(u16::from_be_bytes([b[6], b[7]]) & 0x0FFF);
    let ticks = (time_hi << 48) | (time_mid << 32) | time_low;
    let unix_100ns = ticks as i64 - GREGORIAN_OFFSET_100NS;
    unix_100ns / 10
}

// --- generator byte builders (spec/design/entropy.md §3) ---------------------
// Pure assembly of the 16 bytes from already-drawn random bytes (and, for v7, the timestamp +
// monotonic counter). The PRNG draws + clock resolution live on `StmtRng` (seam.rs); these set
// the version nibble and RFC 4122 variant bits over the supplied randomness.

/// uuidv4: 16 random bytes with the version (4) and variant overwritten in place.
pub fn build_v4(mut b: [u8; 16]) -> [u8; 16] {
    b[6] = (b[6] & 0x0F) | 0x40; // version 4
    b[8] = (b[8] & 0x3F) | 0x80; // RFC 4122 variant
    b
}

/// uuidv7: a 48-bit big-endian Unix-millisecond timestamp (bytes 0..5), a 12-bit monotonic
/// `counter` in rand_a (bytes 6..7 low, RFC 9562 Method 1), and 8 `rand_b` bytes (bytes 8..15),
/// with the version (7) and variant overwritten. `counter` is masked to 12 bits by the caller.
pub fn build_v7(unix_ms: u64, counter: u16, rand_b: [u8; 8]) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0] = (unix_ms >> 40) as u8;
    b[1] = (unix_ms >> 32) as u8;
    b[2] = (unix_ms >> 24) as u8;
    b[3] = (unix_ms >> 16) as u8;
    b[4] = (unix_ms >> 8) as u8;
    b[5] = unix_ms as u8;
    let rand_a = counter & 0x0FFF;
    b[6] = 0x70 | ((rand_a >> 8) as u8 & 0x0F); // version 7 + rand_a high nibble
    b[7] = (rand_a & 0xFF) as u8; // rand_a low byte
    b[8..16].copy_from_slice(&rand_b);
    b[8] = (b[8] & 0x3F) | 0x80; // RFC 4122 variant (overwrites the top 2 bits)
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::parse_uuid;

    fn u(s: &str) -> [u8; 16] {
        parse_uuid(s).unwrap()
    }

    #[test]
    fn version_gates_on_rfc_variant() {
        // PG 18 oracle (spec/design/functions.md §12).
        assert_eq!(
            extract_version(&u("5b2cc7f0-9a3e-4e7b-8c1d-2f3a4b5c6d7e")),
            Some(4)
        );
        assert_eq!(
            extract_version(&u("0190b6f7-8000-7000-8000-000000000000")),
            Some(7)
        );
        assert_eq!(
            extract_version(&u("c232ab00-9414-11ec-b3c8-9e6bdeced846")),
            Some(1)
        );
        assert_eq!(
            extract_version(&u("1ec9414c-232a-6b00-b3c8-9e6bdeced846")),
            Some(6)
        );
        // nil (variant 0), non-RFC (variant 0), Microsoft GUID (variant 11) → NULL.
        assert_eq!(
            extract_version(&u("00000000-0000-0000-0000-000000000000")),
            None
        );
        assert_eq!(
            extract_version(&u("5b2cc7f0-9a3e-4e7b-0c1d-2f3a4b5c6d7e")),
            None
        );
        assert_eq!(
            extract_version(&u("5b2cc7f0-9a3e-4e7b-cc1d-2f3a4b5c6d7e")),
            None
        );
    }

    #[test]
    fn timestamp_v1_and_v7_only() {
        // micros oracle-verified against PG 18.
        assert_eq!(
            extract_timestamp_micros(&u("0190b6f7-8000-7000-8000-000000000000")),
            Some(1_721_056_591_872_000)
        );
        assert_eq!(
            extract_timestamp_micros(&u("c232ab00-9414-11ec-b3c8-9e6bdeced846")),
            Some(1_645_557_742_000_000)
        );
        // v1 sub-microsecond 100-ns ticks are truncated (same micros as the round value).
        assert_eq!(
            extract_timestamp_micros(&u("c232ab07-9414-11ec-b3c8-9e6bdeced846")),
            Some(1_645_557_742_000_000)
        );
        // v6 (no PG-18 timestamp), v4 (no timestamp), non-RFC → NULL.
        assert_eq!(
            extract_timestamp_micros(&u("1ec9414c-232a-6b00-b3c8-9e6bdeced846")),
            None
        );
        assert_eq!(
            extract_timestamp_micros(&u("5b2cc7f0-9a3e-4e7b-8c1d-2f3a4b5c6d7e")),
            None
        );
        assert_eq!(
            extract_timestamp_micros(&u("00000000-0000-0000-0000-000000000000")),
            None
        );
    }
}
