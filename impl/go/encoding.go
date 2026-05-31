package abide

// Order-preserving key encoding (CLAUDE.md §8; spec/design/encoding.md). Encoded
// keys sort byte-for-byte identically to logical order, so stored key order needs no
// comparator. Method int-be-signflip: fixed-width big-endian with the sign bit
// inverted (add bias 2^(bits-1), emit unsigned BE). Verified byte-for-byte against
// spec/encoding/integers.toml in tests — this is what guarantees the Rust and Go
// cores iterate keys identically.

// EncodeInt encodes a non-null integer value of the given type to its
// order-preserving key bytes. value is assumed in range for t (callers range-check).
func EncodeInt(t ScalarType, value int64) []byte {
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

// DecodeInt is the inverse of EncodeInt. len(b) must equal the type's width.
func DecodeInt(t ScalarType, b []byte) int64 {
	width := t.WidthBytes()
	shift := uint(width * 8)
	var u uint64
	for _, x := range b {
		u = (u << 8) | uint64(x)
	}
	return int64(u - (uint64(1) << (shift - 1)))
}

// EncodeNullable encodes a nullable key slot: a 1-byte presence tag (0x00 NULL,
// 0x01 present) followed by the value bytes when present. Makes NULLs sort first in
// ascending order (spec/design/encoding.md §2/§4). A nil pointer means NULL.
func EncodeNullable(t ScalarType, value *int64) []byte {
	if value == nil {
		return []byte{0x00}
	}
	return append([]byte{0x01}, EncodeInt(t, *value)...)
}
