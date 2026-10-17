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

/// The longest source a variable-width GIN term keeps (spec/design/gin.md §4.1/§4.2): a longer
/// string (or `jsonb` number key) is cut to its first `GIN_MAX_TERM_BYTES` bytes, so a GIN entry
/// always fits a node. The cut is at a byte, never decoded.
pub const GIN_MAX_TERM_BYTES: usize = 128;

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

/// One ordered-index key element's sort order (spec/design/encoding.md §2.16, indexes.md §11):
/// `desc` for a `DESC` key, `nulls_first` for `NULLS FIRST`. The default (`ASC NULLS LAST`) is the
/// plain §2.2 slot; the PostgreSQL defaults follow direction (`DESC` ⇒ `NULLS FIRST`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Hash)]
pub struct KeyOrder {
    pub desc: bool,
    pub nulls_first: bool,
}

impl KeyOrder {
    /// `ASC NULLS LAST` — the plain §2.2 slot (`0x00` present, `0x01` NULL).
    pub const ASC: KeyOrder = KeyOrder {
        desc: false,
        nulls_first: false,
    };

    /// Whether this is the default `ASC NULLS LAST` order (no persisted option byte).
    pub fn is_default(self) -> bool {
        !self.desc && !self.nulls_first
    }

    /// The persisted option byte (format.md, v46): bit0 `desc`, bit1 `nulls_first`.
    pub fn to_byte(self) -> u8 {
        u8::from(self.desc) | (u8::from(self.nulls_first) << 1)
    }

    /// Decode a persisted option byte; `None` when a reserved bit is set.
    pub fn from_byte(b: u8) -> Option<KeyOrder> {
        (b & !0b11 == 0).then_some(KeyOrder {
            desc: b & 1 != 0,
            nulls_first: b & 2 != 0,
        })
    }

    /// The slot's presence tag for a present value (§2.16): `0x00` ASC NULLS LAST, `0x01` ASC NULLS
    /// FIRST, `0xFF` DESC NULLS FIRST, `0xFE` DESC NULLS LAST.
    pub fn present_tag(self) -> u8 {
        match (self.desc, self.nulls_first) {
            (false, false) => 0x00,
            (false, true) => 0x01,
            (true, true) => 0xFF,
            (true, false) => 0xFE,
        }
    }

    /// The slot's lone NULL byte (§2.16): `0x01` ASC NULLS LAST, `0x00` ASC NULLS FIRST, `0xFE` DESC
    /// NULLS FIRST, `0xFF` DESC NULLS LAST.
    pub fn null_tag(self) -> u8 {
        match (self.desc, self.nulls_first) {
            (false, false) => 0x01,
            (false, true) => 0x00,
            (true, true) => 0xFE,
            (true, false) => 0xFF,
        }
    }

    /// Append a present slot: the presence tag, then the bare order-preserving key `body` — bitwise
    /// inverted (one's complement) for a `DESC` key.
    pub fn push_present(self, out: &mut Vec<u8>, body: &[u8]) {
        out.push(self.present_tag());
        if self.desc {
            out.extend(body.iter().map(|b| !b));
        } else {
            out.extend_from_slice(body);
        }
    }

    /// Append the slot for an optional value: [`push_present`](Self::push_present) or the NULL byte.
    pub fn push_slot(self, out: &mut Vec<u8>, body: Option<&[u8]>) {
        match body {
            Some(b) => self.push_present(out, b),
            None => out.push(self.null_tag()),
        }
    }

    /// The present slot for `body` as a fresh vector.
    pub fn present_slot(self, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 1);
        self.push_present(&mut out, body);
        out
    }

    /// Whether `tag` (a slot's first byte) is this order's NULL byte.
    pub fn is_null_tag(self, tag: u8) -> bool {
        tag == self.null_tag()
    }
}
