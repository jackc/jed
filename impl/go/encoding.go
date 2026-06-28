package jed

// Order-preserving key encoding (CLAUDE.md §8; spec/design/encoding.md). Encoded
// keys sort byte-for-byte identically to logical order, so stored key order needs no
// comparator. Method int-be-signflip: fixed-width big-endian with the sign bit
// inverted (add bias 2^(bits-1), emit unsigned BE). Verified byte-for-byte against
// spec/encoding/integers.toml in tests — this is what guarantees the Rust and Go
// cores iterate keys identically.

// EncodeInt encodes a non-null integer value of the given type to its
// order-preserving key bytes. value is assumed in range for t (callers range-check).
func encodeInt(t scalarType, value int64) []byte {
	width := t.WidthBytes()
	shift := uint(width * 8)
	// value + 2^(bits-1), in uint64 arithmetic. For width 8 the add wraps mod 2^64,
	// which is exactly the sign-flip; for narrower widths we keep the low `width`
	// bytes. uint64(value) sign-extends a negative value to 64 bits first.
	u := uint64(value) + (uint64(1) << (shift - 1))
	out := make([]byte, width)
	for i := 0; i < width; i++ {
		out[width-1-i] = byte(u >> (8 * uint(i)))
	}
	return out
}

// EncodeBool encodes a non-null boolean to its order-preserving key body: a single
// bool-byte, 0x00 for false < 0x01 for true (method bool-byte, spec/design/encoding.md
// §2.9). Fixed-width 1, so self-delimiting with no sign-flip / escape / terminator — like
// uuid. Byte-identical to the boolean value-codec body (a stored boolean reuses these bytes
// behind the §2.2 presence tag — spec/fileformat/format.md). A PK is NOT NULL, so no
// presence tag.
func encodeBool(value bool) []byte {
	if value {
		return []byte{0x01}
	}
	return []byte{0x00}
}

// EncodeTerminated encodes a non-null text/bytea value to its order-preserving key body
// (method text-terminated-escape / bytea-terminated-escape, spec/design/encoding.md
// §2.4/§2.6). content is the value's raw bytes — UTF-8 for text (the C collation, so
// bytes.Compare equals code-point order), raw bytes for bytea. Variable-width, so it must be
// self-delimiting: escape every 0x00 to 0x00 0xFF and terminate with 0x00 0x01. The terminator
// is the only place a 0x00 is followed by a byte < 0xFF, so it sorts below any real continuation
// — a value sorts before any value that extends it. A PK is NOT NULL, so the stored key is this
// bare body with no presence tag.
func encodeTerminated(content []byte) []byte {
	out := make([]byte, 0, len(content)+2)
	for _, b := range content {
		out = append(out, b)
		if b == 0x00 {
			out = append(out, 0xFF)
		}
	}
	return append(out, 0x00, 0x01)
}

// DecodeInt is the inverse of EncodeInt. len(b) must equal the type's width.
func decodeInt(t scalarType, b []byte) int64 {
	width := t.WidthBytes()
	shift := uint(width * 8)
	var u uint64
	for _, x := range b {
		u = (u << 8) | uint64(x)
	}
	return int64(u - (uint64(1) << (shift - 1)))
}

// EncodeNullable encodes a nullable key slot: a 1-byte presence tag (0x00 present,
// 0x01 NULL), with the value bytes following the tag when present. Because 0x00 <
// 0x01, present values sort before NULL, so NULLs sort LAST in ascending order;
// descending inverts the component, lifting NULL to first (the PostgreSQL model —
// NULL is the largest value; spec/design/encoding.md §2/§4). A nil pointer means NULL.
func encodeNullable(t scalarType, value *int64) []byte {
	if value == nil {
		return []byte{0x01}
	}
	return append([]byte{0x00}, encodeInt(t, *value)...)
}
