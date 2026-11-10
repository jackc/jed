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

// The layout byte is a set of flags. A v50 leaf has no layout byte (its first payload byte is 0).
const (
	leafLayoutNarrow   = 1 // every directory entry is a u16
	leafLayoutElideKey = 2 // key-derivable columns have no region
)

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
