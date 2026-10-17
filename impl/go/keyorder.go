package jed

// keyOrder is one ordered-index key element's sort order (spec/design/encoding.md §2.16,
// indexes.md §11): Desc for a DESC key, NullsFirst for NULLS FIRST. The zero value (ASC NULLS LAST)
// is the plain §2.2 slot; the PostgreSQL defaults follow direction (DESC ⇒ NULLS FIRST).
type keyOrder struct {
	Desc       bool
	NullsFirst bool
}

// isDefault reports whether this is the default ASC NULLS LAST order (no persisted option byte).
func (o keyOrder) isDefault() bool { return !o.Desc && !o.NullsFirst }

// toByte is the persisted option byte (format.md, v46): bit0 desc, bit1 nulls_first.
func (o keyOrder) toByte() byte {
	var b byte
	if o.Desc {
		b |= 1
	}
	if o.NullsFirst {
		b |= 2
	}
	return b
}

// keyOrderFromByte decodes a persisted option byte; ok is false when a reserved bit is set.
func keyOrderFromByte(b byte) (keyOrder, bool) {
	if b&^0b11 != 0 {
		return keyOrder{}, false
	}
	return keyOrder{Desc: b&1 != 0, NullsFirst: b&2 != 0}, true
}

// presentTag is the slot's presence tag for a present value (§2.16): 0x00 ASC NULLS LAST, 0x01 ASC
// NULLS FIRST, 0xFF DESC NULLS FIRST, 0xFE DESC NULLS LAST.
func (o keyOrder) presentTag() byte {
	switch {
	case !o.Desc && !o.NullsFirst:
		return 0x00
	case !o.Desc && o.NullsFirst:
		return 0x01
	case o.Desc && o.NullsFirst:
		return 0xFF
	default:
		return 0xFE
	}
}

// nullTag is the slot's lone NULL byte (§2.16): 0x01 ASC NULLS LAST, 0x00 ASC NULLS FIRST, 0xFE
// DESC NULLS FIRST, 0xFF DESC NULLS LAST.
func (o keyOrder) nullTag() byte {
	switch {
	case !o.Desc && !o.NullsFirst:
		return 0x01
	case !o.Desc && o.NullsFirst:
		return 0x00
	case o.Desc && o.NullsFirst:
		return 0xFE
	default:
		return 0xFF
	}
}

// pushPresent appends a present slot to out: the presence tag, then the bare order-preserving key
// body — bitwise inverted (one's complement) for a DESC key.
func (o keyOrder) pushPresent(out []byte, body []byte) []byte {
	out = append(out, o.presentTag())
	if o.Desc {
		for _, b := range body {
			out = append(out, ^b)
		}
		return out
	}
	return append(out, body...)
}

// presentSlot is the present slot for body as a fresh slice.
func (o keyOrder) presentSlot(body []byte) []byte {
	return o.pushPresent(make([]byte, 0, len(body)+1), body)
}

// isNullTag reports whether tag (a slot's first byte) is this order's NULL byte.
func (o keyOrder) isNullTag(tag byte) bool { return tag == o.nullTag() }
