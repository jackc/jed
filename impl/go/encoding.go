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

// ginMaxTermBytes is the longest source a variable-width GIN term keeps (spec/design/gin.md
// §4.1/§4.2): a longer string (or jsonb number key) is cut to its first ginMaxTermBytes bytes, so a
// GIN entry always fits a node. The cut is at a byte, never decoded.
const ginMaxTermBytes = 128

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

// keySlotSkippable reports whether a C-collated key slot of type t can be stepped over in an index
// entry without knowing its value (cost.md §3 "Multi-column secondary indexes"): a fixed-width type
// by its width, and the self-delimiting variable-width text, bytea, decimal, and interval bodies by
// decoding their delimiters (encoding.md §2.4–§2.6, §2.10). A collated text key (the UCA sort key,
// §2.12) is not stepped over.
func keySlotSkippable(t scalarType) bool {
	switch t {
	case scalarText, scalarBytea, scalarDecimal, scalarInterval:
		return true
	}
	return t.IsFixedWidth()
}

// keySlotEnd returns the offset just past the nullable key slot of type t starting at at (the §2.2
// presence tag, then the body): a lone 0x01 NULL tag, or 0x00 followed by the body, stepped over by
// its width or, for a variable-width body, by decoding its delimiter. A truncated slot returns an
// offset past len(key), which names no row.
func keySlotEnd(key []byte, at int, t scalarType) int {
	if at >= len(key) {
		return len(key) + 1
	}
	if key[at] == 0x01 {
		return at + 1
	}
	at++
	switch t {
	case scalarText, scalarBytea:
		// text-terminated-escape: 0x00 0xFF is an escaped content NUL, 0x00 0x01 the terminator.
		for i := at; i+1 < len(key); i++ {
			if key[i] == 0x00 {
				if key[i+1] == 0x01 {
					return i + 2
				}
				i++
			}
		}
		return len(key) + 1
	case scalarDecimal:
		// decimal-order-preserving: the class byte; zero is that byte alone, otherwise a 4-byte
		// exponent and mantissa pairs ending at the terminator (0x00, or 0xFF complemented for a
		// negative value).
		if at >= len(key) {
			return len(key) + 1
		}
		class := key[at]
		if class == 0x04 {
			return at + 1
		}
		term := byte(0x00)
		if class == 0x03 {
			term = 0xFF
		}
		for i := at + 5; i < len(key); i++ {
			if key[i] == term {
				return i + 1
			}
		}
		return len(key) + 1
	case scalarInterval:
		return at + 16
	}
	return at + t.WidthBytes()
}
