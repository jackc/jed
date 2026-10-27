package jed

// key_slot.go — the self-delimiting index key-slot skip (spec/design/indexes.md §5.1): how an
// index entry's storage-key suffix is recovered past the known equality prefix by skipping each
// remaining key component by its own encoding (a fixed width or a terminator).

import "bytes"

// keySlotKind is how one index key component's nullable slot is skipped to recover an entry's
// storage-key suffix (indexes.md §5.1, encoding.md §2.2): every scalar key encoding is
// self-delimiting, either by a fixed width or by its own terminator.
type keySlotKind uint8

const (
	// keySlotFixed is a fixed-width body (the integers, bool, uuid, date, timestamps, floats,
	// interval's 16 bytes); keySlot.width holds the width.
	keySlotFixed keySlotKind = iota
	// keySlotTerminated is text-terminated-escape / bytea-terminated-escape (encoding.md §2.4/§2.6).
	keySlotTerminated
	// keySlotDecimal is decimal-order-preserving (encoding.md §2.5).
	keySlotDecimal
	// keySlotCollated is text-collated-sortkey (encoding.md §2.12).
	keySlotCollated
)

// keySlot is one index key component's slot form (indexes.md §5.1) under its sort order
// (indexes.md §11, encoding.md §2.16): the order picks the slot's NULL tag, and a DESC slot's body
// is the one's complement of the ASC body, which the skip reads through an XOR mask (inversion
// keeps every body self-delimiting — the no-prefix property, spec/encoding/ordered.toml). The zero
// order is the plain §2.2 ASC NULLS LAST slot.
type keySlot struct {
	kind  keySlotKind
	width int // keySlotFixed only
	order keyOrder
}

// withOrder is k under the key element's sort order o.
func (k keySlot) withOrder(o keyOrder) keySlot {
	k.order = o
	return k
}

// keySlotForScalar is the slot form of a scalar key component of type ty; collated selects the
// sort-key form of a non-C text key. ok=false for a type with no key encoding.
func keySlotForScalar(ty scalarType, collated bool) (keySlot, bool) {
	switch ty {
	case scalarText:
		if collated {
			return keySlot{kind: keySlotCollated}, true
		}
		return keySlot{kind: keySlotTerminated}, true
	case scalarBytea:
		return keySlot{kind: keySlotTerminated}, true
	case scalarDecimal:
		return keySlot{kind: keySlotDecimal}, true
	case scalarInterval:
		return keySlot{kind: keySlotFixed, width: 16}, true
	case scalarJson, scalarJsonb, scalarJsonPath:
		return keySlot{}, false
	default:
		return keySlot{kind: keySlotFixed, width: ty.WidthBytes()}, true
	}
}

// slotLen is the byte length of the nullable slot starting at key[at] (its tag included): 1 for the
// order's NULL tag, else 1 + the self-delimited body (inverted under DESC). ok=false for a malformed
// (truncated) slot.
func (k keySlot) slotLen(key []byte, at int) (int, bool) {
	if at >= len(key) {
		return 0, false
	}
	if key[at] == k.order.nullTag() {
		return 1, true
	}
	// m un-inverts a DESC body byte (b^m is the ASC byte).
	var m byte
	if k.order.Desc {
		m = 0xFF
	}
	n, ok := k.bodyLen(key[at+1:], m)
	if !ok {
		return 0, false
	}
	return 1 + n, true
}

// bodyLen is the length of the self-delimited key body at the start of body, each byte read as b^m
// (m = 0xFF under a DESC slot's whole-key inversion). ok=false for a malformed (truncated) body.
func (k keySlot) bodyLen(body []byte, m byte) (int, bool) {
	var n int
	switch k.kind {
	case keySlotFixed:
		if len(body) < k.width {
			return 0, false
		}
		n = k.width
	case keySlotTerminated:
		l, ok := terminatedLen(body, m)
		if !ok {
			return 0, false
		}
		n = l
	case keySlotDecimal:
		if len(body) == 0 {
			return 0, false
		}
		switch class := body[0] ^ m; class {
		case 0x04:
			n = 1
		case 0x03, 0x05:
			// class ‖ 4-byte exponent ‖ pair bytes ‖ terminator (0x00; complemented 0xFF).
			term := byte(0xFF)
			if class == 0x05 {
				term = 0x00
			}
			if len(body) < 5 {
				return 0, false
			}
			pos := bytes.IndexByte(body[5:], term^m)
			if pos < 0 {
				return 0, false
			}
			n = 5 + pos + 1
		default:
			return 0, false
		}
	case keySlotCollated:
		// Three levels of 2-byte weights, each ended by a 0x0000 separator, then the identical
		// level: the original string's terminated C key.
		i, seps := 0, 0
		for seps < 3 {
			if i+2 > len(body) {
				return 0, false
			}
			zero := body[i]^m == 0 && body[i+1]^m == 0
			i += 2
			if zero {
				seps++
			}
		}
		l, ok := terminatedLen(body[i:], m)
		if !ok {
			return 0, false
		}
		n = i + l
	}
	return n, true
}

// colTypeKeyBodyLen is the length of the order-preserving key body of a value of resolved type ct at
// the start of body, read through the inversion mask m — a scalar's own body, or a container key
// recursed into (range-bounds §2.11, array-elements-terminated §2.14, composite-field-slots §2.15 of
// spec/design/encoding.md). collated selects the sort-key form of a non-C text (or text[] element)
// key. ok=false for a malformed body or a type with no key encoding.
func colTypeKeyBodyLen(ct colType, collated bool, body []byte, m byte) (int, bool) {
	at := func(i int) (byte, bool) {
		if i >= len(body) {
			return 0, false
		}
		return body[i] ^ m, true
	}
	switch {
	case ct.Composite:
		pos := 0
		for _, f := range ct.Fields {
			tag, ok := at(pos)
			if !ok {
				return 0, false
			}
			pos++
			switch tag {
			case 0x01:
			case 0x00:
				n, ok := colTypeKeyBodyLen(f.Type, false, body[pos:], m)
				if !ok {
					return 0, false
				}
				pos += n
			default:
				return 0, false
			}
		}
		return pos, true
	case ct.RangeElem != nil:
		tag, ok := at(0)
		if !ok {
			return 0, false
		}
		if tag == 0x00 {
			return 1, true // empty
		}
		if tag != 0x01 {
			return 0, false
		}
		pos := 1
		for _, infinite := range []byte{0x00, 0x02} {
			marker, ok := at(pos)
			if !ok {
				return 0, false
			}
			pos++
			if marker == infinite {
				continue
			}
			if marker != 0x01 {
				return 0, false
			}
			n, ok := colTypeKeyBodyLen(*ct.RangeElem, false, body[pos:], m)
			if !ok {
				return 0, false
			}
			pos += n + 1 // the element key, then the inclusivity byte
		}
		if pos > len(body) {
			return 0, false
		}
		return pos, true
	case ct.Elem != nil:
		pos := 0
		for {
			marker, ok := at(pos)
			if !ok {
				return 0, false
			}
			pos++
			if marker == 0x00 {
				break
			}
			if marker == 0x02 {
				continue
			}
			if marker != 0x01 {
				return 0, false
			}
			n, ok := colTypeKeyBodyLen(*ct.Elem, collated, body[pos:], m)
			if !ok {
				return 0, false
			}
			pos += n
		}
		ndim, ok := at(pos)
		if !ok {
			return 0, false
		}
		pos += 1 + 8*int(ndim)
		if pos > len(body) {
			return 0, false
		}
		return pos, true
	default:
		slot, ok := keySlotForScalar(ct.Scalar, collated)
		if !ok {
			return 0, false
		}
		return slot.bodyLen(body, m)
	}
}

// terminatedLen is the length of a *-terminated-escape body (0x00 0xFF an escaped NUL, 0x00 0x01
// the end), each byte read as b^m (m = 0xFF for a DESC slot's inverted body).
func terminatedLen(body []byte, m byte) (int, bool) {
	i := 0
	for {
		if i >= len(body) {
			return 0, false
		}
		if body[i]^m != 0x00 {
			i++
			continue
		}
		if i+1 >= len(body) {
			return 0, false
		}
		if body[i+1]^m == 0x01 {
			return i + 2, true
		}
		i += 2
	}
}

// indexEntryRowKey skips slots past prefixLen in an index entry key and returns the storage-key
// suffix after them (indexes.md §3). A corrupt entry shorter than its components yields an empty
// storage key, which names no row.
func indexEntryRowKey(key []byte, prefixLen int, slots []keySlot) []byte {
	at := prefixLen
	for _, slot := range slots {
		n, ok := slot.slotLen(key, at)
		if !ok {
			return nil
		}
		at += n
	}
	if at > len(key) {
		return nil
	}
	return key[at:]
}
