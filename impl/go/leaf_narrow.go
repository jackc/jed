package jed

// PROTOTYPE (spec/design/leaf-structural.md): the narrow leaf layout.
//
// A v50 leaf spends 4 bytes on every directory entry: one per key, one per value of a variable-width
// column, and K+1 for the column directory. Every such entry is an offset within one page, and a page
// is at most 64 KiB, so 2 bytes suffice. With JED_LEAF_NARROW=on a leaf is written in the narrow
// layout: a leading layout byte, then the v50 payload with every directory entry a big-endian u16.
// A v50 leaf's first payload byte is the high byte of a u32 offset and is always 0, so the layout
// byte tells the two apart and both read. record_size is unchanged; leafOverhead is
//
//	1 + 2·N + 2·(K+1) + F·(1 + ceil(N/8)) + V·(1 + 2·N)
//
// which stays a function of N and the schema, so the split rules are unchanged.
//
// Go-only, no format version bump. The same layout byte also carries the key-column flag below.

import "os"

// leafNarrow makes this process write narrow leaves.
var leafNarrow = os.Getenv("JED_LEAF_NARROW") == "on"

// leafElideKey makes this process write leaves without the primary-key columns whose values the key
// already holds (JED_LEAF_ELIDE_KEY=on). Such a column keeps its column-directory entry but has an
// empty region; a reader rebuilds its dense slots from the keys when it parses the page. The column
// contributes nothing to record_size, and leafOverhead counts it only in the column directory.
var leafElideKey = os.Getenv("JED_LEAF_ELIDE_KEY") == "on"

// leafFixedKey makes this process write a table leaf whose keys all have one length, fixed by the
// primary key's column types, without a key directory (JED_LEAF_FIXED_KEY=on): the layout byte is
// followed by the key length as a u16, and key i is at i·key_len in the key blob. The page names its
// own key length, so a reader needs no schema. Index trees keep their directory: an index key's
// length depends on whether a key column is NULL.
var leafFixedKey = os.Getenv("JED_LEAF_FIXED_KEY") == "on"

// leafBareString makes this process write an inline text / bytea / json value in a leaf without its
// u16 length (JED_LEAF_BARE_STRING=on): the value directory's span already gives the length. Such a
// value carries the presence tag tagInlineBare instead of 0x00, so each value names its own form and
// both read. Its record_size contribution is 1 + len instead of 3 + len.
var leafBareString = os.Getenv("JED_LEAF_BARE_STRING") == "on"

// tagInlineBare is the presence tag of an inline-plain string body stored without its length: the
// rest of the value's span is the content payload.
const tagInlineBare byte = 0x05

// bareString reports whether a column's inline values are written bare.
func bareString(ty colType) bool { return leafBareString && stringPayload(ty) }

// inlinePayload is the content payload of an inline-deferred string value, whichever form it was
// read from.
func (u *Unfetched) inlinePayload() []byte {
	if u.bare {
		return u.Comp
	}
	return u.Comp[min(2, len(u.Comp)):]
}

// The layout byte is a set of flags. A v50 leaf has no layout byte (its first payload byte is 0).
const (
	leafLayoutNarrow   = 1 // every directory entry is a u16
	leafLayoutElideKey = 2 // key-derivable columns have no region
	leafLayoutFixedKey = 4 // a u16 key length follows; no key directory
)

// leafFlagsFor is the layout byte of a leaf in a tree whose keys have the fixed length keyLen (0 when
// they vary).
func leafFlagsFor(keyLen int) byte {
	f := leafLayoutFlags()
	if leafFixedKey && keyLen > 0 {
		f |= leafLayoutFixedKey
	}
	return f
}

// tableKeyLen is the length of every storage key of a table with these column types and primary key
// (member ordinals in key order), or 0 when keys vary in length. A table without a primary key is
// keyed by its 8-byte synthetic rowid; a primary key is fixed-length when every member is a scalar
// with a fixed-width key encoding.
func tableKeyLen(colTypes []colType, pk []int) int {
	if len(pk) == 0 {
		return 8
	}
	total := 0
	for _, c := range pk {
		if c < 0 || c >= len(colTypes) {
			return 0
		}
		ty := colTypes[c]
		if ty.Composite || ty.Elem != nil || ty.RangeElem != nil {
			return 0
		}
		slot, ok := keySlotForScalar(ty.Scalar, false)
		if !ok || slot.kind != keySlotFixed || slot.width <= 0 {
			return 0
		}
		total += slot.width
	}
	if total > 0xFFFF {
		return 0
	}
	return total
}

// leafLayoutFlags is the layout byte this process writes; 0 writes the v50 layout.
func leafLayoutFlags() byte {
	var f byte
	if leafNarrow {
		f |= leafLayoutNarrow
	}
	if leafElideKey {
		f |= leafLayoutElideKey
	}
	return f
}

// keyDerivable reports whether a column of type ty, as a primary-key member, has a fixed-width key
// encoding from which its value body is exactly recoverable: the integers and the date/time types
// boolean and uuid, whose value body is byte-for-byte their key encoding. Floats and interval are
// excluded: their keys canonicalize (−0, NaN payloads, interval normalization).
func keyDerivable(ty colType) bool {
	if ty.Composite || ty.Elem != nil || ty.RangeElem != nil {
		return false
	}
	switch ty.Scalar {
	case scalarInt16, scalarInt32, scalarInt64, scalarBool, scalarUuid, scalarTimestamp, scalarTimestamptz, scalarDate:
		return true
	}
	return false
}

// markKeyColumns returns colTypes with keyAt set on the leading run of key-derivable primary-key
// columns (pk is the member ordinals in key order). The run stops at the first member that is not
// key-derivable, since the offsets after it are no longer fixed. A copy: the caller's slice may be
// the catalog's.
func markKeyColumns(colTypes []colType, pk []int) []colType {
	out := append([]colType(nil), colTypes...)
	at := 0
	for _, c := range pk {
		if c < 0 || c >= len(out) || !keyDerivable(out[c]) {
			break
		}
		w, _ := fixedValueWidth(out[c])
		out[c].keyAt = uint16(at + 1)
		at += w
	}
	return out
}

// putKeySlot writes into slot the value body of a key-derivable column from its key bytes. The two
// are the same bytes: an integer or date/time value body is already the order-preserving sign-flipped
// form (format.md *Value codec*), and boolean and uuid are stored raw in both. The encoder checks the
// equality for every row it elides.
func putKeySlot(slot []byte, _ colType, key []byte) { copy(slot, key) }

// keyColumnSlot is the value body of column ty rebuilt from a row's storage key.
func keyColumnSlot(ty colType, key []byte) []byte {
	w, _ := fixedValueWidth(ty)
	at := int(ty.keyAt) - 1
	if at+w > len(key) {
		return nil
	}
	slot := make([]byte, w)
	putKeySlot(slot, ty, key[at:at+w])
	return slot
}
