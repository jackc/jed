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

/// Encode a non-null boolean to its order-preserving key body: a single `bool-byte`,
/// `0x00` for false `<` `0x01` for true (method `bool-byte`, spec/design/encoding.md §2.9).
/// Fixed-width 1, so self-delimiting with no sign-flip / escape / terminator — like uuid.
/// Byte-identical to the boolean value-codec body (a stored boolean reuses these bytes behind
/// the §2.2 presence tag — spec/fileformat/format.md). A PK is NOT NULL, so no presence tag.
pub fn encode_bool(value: bool) -> Vec<u8> {
    vec![u8::from(value)]
}

/// Encode a non-null `text`/`bytea` value to its order-preserving key body
/// (method `text-terminated-escape` / `bytea-terminated-escape`, spec/design/encoding.md
/// §2.4/§2.6). `content` is the value's raw bytes — UTF-8 for `text` (the `C` collation, so
/// `memcmp` of the bytes equals code-point order), raw bytes for `bytea`. Variable-width, so it
/// must be self-delimiting: escape every `0x00` to `0x00 0xFF` and terminate with `0x00 0x01`.
/// The terminator is the only place a `0x00` is followed by a byte `< 0xFF`, so it sorts below
/// any real continuation — a value sorts before any value that extends it. A PK is NOT NULL, so
/// the stored key is this bare body with no presence tag.
pub fn encode_terminated(content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 2);
    for &b in content {
        out.push(b);
        if b == 0x00 {
            out.push(0xFF);
        }
    }
    out.push(0x00);
    out.push(0x01);
    out
}

/// Encode a nullable key slot: a 1-byte presence tag (0x00 present, 0x01 NULL),
/// with the value bytes following the tag when present. Because `0x00 < 0x01`,
/// present values sort before NULL, so NULLs sort **last** in ascending order;
/// descending inverts the component, lifting NULL to first (the PostgreSQL model —
/// NULL is the largest value; spec/design/encoding.md §2/§4).
pub fn encode_nullable(ty: ScalarType, value: Option<i64>) -> Vec<u8> {
    match value {
        None => vec![0x01],
        Some(v) => {
            let mut out = vec![0x00];
            out.extend_from_slice(&encode_int(ty, v));
            out
        }
    }
}
