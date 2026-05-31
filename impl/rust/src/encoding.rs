//! Order-preserving key encoding (CLAUDE.md §8; spec/design/encoding.md).
//!
//! Encoded keys sort byte-for-byte (`memcmp`) identically to logical order, so the
//! stored key order needs no comparator. Method `int-be-signflip`: fixed-width
//! big-endian with the sign bit inverted (add bias 2^(bits-1), emit unsigned BE).
//! Verified byte-for-byte against spec/encoding/integers.toml in tests.

use crate::types::ScalarType;

/// Encode a non-null integer value of the given type to its order-preserving key
/// bytes. `value` is assumed in range for `ty` (callers range-check first).
pub fn encode_int(ty: ScalarType, value: i64) -> Vec<u8> {
    let width = ty.width_bytes();
    let bias = 1u128 << (width * 8 - 1);
    // value + bias lands in [0, 2^bits); take the low `width` big-endian bytes.
    let u = (value as i128 + bias as i128) as u128;
    let be = u.to_be_bytes(); // 16 bytes
    be[(be.len() - width)..].to_vec()
}

/// Decode order-preserving key bytes back to the logical integer (inverse of
/// `encode_int`). `bytes.len()` must equal the type's width.
pub fn decode_int(ty: ScalarType, bytes: &[u8]) -> i64 {
    let width = ty.width_bytes();
    debug_assert_eq!(bytes.len(), width);
    let bias = 1u128 << (width * 8 - 1);
    let mut u: u128 = 0;
    for &b in bytes {
        u = (u << 8) | b as u128;
    }
    (u as i128 - bias as i128) as i64
}

/// Encode a nullable key slot: a 1-byte presence tag (0x00 NULL, 0x01 present)
/// followed by the value bytes when present. Makes NULLs sort first in ascending
/// order (spec/design/encoding.md §2/§4).
pub fn encode_nullable(ty: ScalarType, value: Option<i64>) -> Vec<u8> {
    match value {
        None => vec![0x00],
        Some(v) => {
            let mut out = vec![0x01];
            out.extend_from_slice(&encode_int(ty, v));
            out
        }
    }
}
