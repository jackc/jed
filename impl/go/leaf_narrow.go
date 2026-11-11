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
	leafLayoutPrefix   = 8 // a u16 prefix length and the prefix follow; the key blob holds suffixes
)

// leafKeyPrefix makes this process write a leaf's shared key prefix once (JED_LEAF_KEY_PREFIX=on,
// with JED_LEAF_NARROW=on): the bytes common to the leaf's first and last key, which every key
// between them shares because keys are sorted. The key directory and blob then hold suffixes. This is
// the first content-dependent layout: a leaf's size is
//
//	Σ record_size + leafOverhead(N, shape) − leafPrefixSaving(N, first key, last key)
//
// so the split and merge rules measure the keys as well as the weights. record_size is unchanged.
// A reader rebuilds the full keys into one allocation when it parses the page.
var leafKeyPrefix = os.Getenv("JED_LEAF_KEY_PREFIX") == "on" && leafNarrow

// leafPrefixLen is the length of the prefix a leaf of n keys stores once: the common prefix of its
// first and last key when storing it saves bytes (n·p > p + 2), else 0.
func leafPrefixLen(n int, first, last []byte) int {
	if !leafKeyPrefix || n < 2 {
		return 0
	}
	p := 0
	for p < len(first) && p < len(last) && first[p] == last[p] {
		p++
	}
	if p > 0xFFFF {
		p = 0xFFFF
	}
	if n*p <= p+2 {
		return 0
	}
	return p
}

// leafPrefixSaving is the bytes the shared prefix removes from a leaf of n keys: each key loses p
// bytes, and the page gains the u16 prefix length and the prefix.
func leafPrefixSaving(n int, first, last []byte) int {
	p := leafPrefixLen(n, first, last)
	if p == 0 {
		return 0
	}
	return n*p - p - 2
}

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

// leafFOR makes this process write an integer-like fixed-width column of a leaf as a base plus narrow
// offsets (JED_LEAF_FOR=on, with JED_LEAF_NARROW=on): the region's flags byte is regionFOR, and after
// the null bitmap come the base (the smallest present value, at the column's width), a u8 offset
// width bw (0, 1, 2 or 4, below the column's width) and N offsets of bw bytes (a NULL's is 0). The
// region is written this way only when it is smaller: N·(w − bw) > w + 1. Like the key prefix, this
// makes a leaf's size depend on its contents:
//
//	leaf size = Σ record_size + leafOverhead − prefix saving − Σ over such columns of forSaving
//
// record_size is unchanged. A reader rebuilds the dense full-width slots when it parses the page.
var leafFOR = os.Getenv("JED_LEAF_FOR") == "on" && leafNarrow

// regionFOR is the region flags byte of a base-plus-offsets fixed-width region.
const regionFOR byte = 1

// forCol is one column a leaf may write as base plus offsets.
type forCol struct {
	col, w int
}

// forEligible reports whether a column's value body is a big-endian unsigned image of its order (the
// sign-flipped integers and date/time instants), so that a range of nearby values has a small
// difference.
func forEligible(ty colType) bool {
	if !leafFOR || ty.Composite || ty.Elem != nil || ty.RangeElem != nil || (leafElideKey && ty.keyAt != 0) {
		return false
	}
	switch ty.Scalar {
	case scalarInt16, scalarInt32, scalarInt64, scalarTimestamp, scalarTimestamptz, scalarDate:
		return true
	}
	return false
}

// forSlot is the unsigned image of an eligible column's present value: its value body read as a
// big-endian integer (format.md *Value codec*: the sign bit flipped at the column's width).
func forSlot(w int, v Value) uint64 {
	switch w {
	case 2:
		return uint64(uint16(v.Int) ^ 0x8000)
	case 4:
		return uint64(uint32(v.Int) ^ 0x80000000)
	}
	return uint64(v.Int) ^ (1 << 63)
}

// forWidth is the offset width for present values spanning [lo, hi] in a column of width w, or w when
// no narrower width holds them.
func forWidth(w int, lo, hi uint64) int {
	bw := 8
	switch d := hi - lo; {
	case d == 0:
		bw = 0
	case d < 1<<8:
		bw = 1
	case d < 1<<16:
		bw = 2
	case d < 1<<32:
		bw = 4
	}
	if bw >= w {
		return w
	}
	return bw
}

// forSaving is the bytes a column of width w saves in a leaf of n records whose present values span
// [lo, hi] (any reports whether one is present): 0 when the plain region is no larger.
func forSaving(n, w int, lo, hi uint64, any bool) int {
	if !any {
		lo, hi = 0, 0
	}
	bw := forWidth(w, lo, hi)
	if s := n*(w-bw) - (w + 1); s > 0 {
		return s
	}
	return 0
}

// forRange accumulates the span of a column's present values.
type forRange struct {
	lo, hi uint64
	any    bool
}

func (r *forRange) add(v uint64) {
	if !r.any {
		r.lo, r.hi, r.any = v, v, true
		return
	}
	if v < r.lo {
		r.lo = v
	}
	if v > r.hi {
		r.hi = v
	}
}

// rowsFORSaving is the bytes base-plus-offsets regions save in a leaf holding rows.
func rowsFORSaving(cols []forCol, rows []storedRow) int {
	total := 0
	for _, fc := range cols {
		var r forRange
		for _, row := range rows {
			if v := row[fc.col]; !v.IsNull() {
				r.add(forSlot(fc.w, v))
			}
		}
		total += forSaving(len(rows), fc.w, r.lo, r.hi, r.any)
	}
	return total
}

// packedFORSaving is rowsFORSaving for a packed leaf, read from its dense slots.
func packedFORSaving(cols []forCol, l *paxLeaf, n int) int {
	total := 0
	for _, fc := range cols {
		reg := &l.regions[fc.col]
		var r forRange
		for i := 0; i < n; i++ {
			if reg.bitmap[i/8]&(0x80>>(i%8)) != 0 {
				continue
			}
			r.add(beUint(reg.body[i*fc.w : (i+1)*fc.w]))
		}
		total += forSaving(n, fc.w, r.lo, r.hi, r.any)
	}
	return total
}

func beUint(b []byte) uint64 {
	var v uint64
	for _, x := range b {
		v = v<<8 | uint64(x)
	}
	return v
}

func putBEUint(b []byte, v uint64) {
	for i := len(b) - 1; i >= 0; i-- {
		b[i] = byte(v)
		v >>= 8
	}
}
