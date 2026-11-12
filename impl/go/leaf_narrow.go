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

// leafNarrow makes this process write narrow leaves.
const leafNarrow = true

// leafElideKey makes this process write leaves without the primary-key columns whose values the key
// already holds (JED_LEAF_ELIDE_KEY=on). Such a column keeps its column-directory entry but has an
// empty region; a reader rebuilds its dense slots from the keys when it parses the page. The column
// contributes nothing to record_size, and leafOverhead counts it only in the column directory.
const leafElideKey = true

// leafFixedKey makes this process write a table leaf whose keys all have one length, fixed by the
// primary key's column types, without a key directory (JED_LEAF_FIXED_KEY=on): the layout byte is
// followed by the key length as a u16, and key i is at i·key_len in the key blob. The page names its
// own key length, so a reader needs no schema. Index trees keep their directory: an index key's
// length depends on whether a key column is NULL.
const leafFixedKey = true

// leafBareString makes this process write an inline text / bytea / json value in a leaf without its
// u16 length (JED_LEAF_BARE_STRING=on): the value directory's span already gives the length. Such a
// value carries the presence tag tagInlineBare instead of 0x00, so each value names its own form and
// both read. Its record_size contribution is 1 + len instead of 3 + len.
const leafBareString = true

// tagInlineBare is the presence tag of an inline-plain string body stored without its length: the
// rest of the value's span is the content payload.
const tagInlineBare byte = 0x05

// bareString reports whether a column's inline values are written bare.
func bareString(ty colType) bool { return leafBareString && stringPayload(ty) }

// inlinePayload is the content payload of an inline-deferred string value, whichever form it was
// read from.
func (u *Unfetched) inlinePayload() []byte {
	if u.bare {
		return u.body()
	}
	return u.Comp[min(2, len(u.Comp)):]
}

// The leaf flags byte leads every leaf payload (format.md "Leaf node"). Other bits are reserved.
const (
	leafFlagFixedKey = 1 // a u16 key length follows; no key directory
	leafFlagPrefix   = 2 // a u16 prefix length and the prefix follow; the key blob holds suffixes
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
const leafKeyPrefix = true

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

// leafFlagsFor is the leaf flags byte of a tree whose keys have the fixed length keyLen (0 when they
// vary), before the prefix bit, which is per leaf.
func leafFlagsFor(keyLen int) byte {
	if keyLen > 0 {
		return leafFlagFixedKey
	}
	return 0
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
const leafFOR = true

// regionFOR is the region flags byte of a base-plus-offsets fixed-width region.
const regionFOR byte = 1

// forCol is one column a leaf may write as base plus offsets.
type forCol struct {
	col, w int
	// dec marks a fixed-width numeric(p,s) column: its slot is the value's unscaled integer at scale.
	dec   bool
	scale uint16
}

// forEligible reports whether a column's value body is the sign-flipped big-endian integer body (the
// integers, the date/time instants, and a fixed-width numeric(p,s) slot), so that a range of nearby
// values has a small difference. A column the leaf does not store (keyAt) is not.
func forEligible(ty colType) bool {
	if ty.Composite || ty.Elem != nil || ty.RangeElem != nil || ty.keyAt != 0 {
		return false
	}
	switch ty.Scalar {
	case scalarInt16, scalarInt32, scalarInt64, scalarTimestamp, scalarTimestamptz, scalarDate:
		return true
	}
	return ty.DecSlot != 0
}

// forSlot is the unsigned image of an eligible column's present value: its value body read as a
// big-endian integer (format.md "Value codec": the sign bit flipped at the column's width).
func forSlot(fc forCol, v Value) uint64 {
	x := v.Int
	if fc.dec {
		u, ok := v.decimal().unscaledInt64(uint32(fc.scale))
		if !ok {
			panic("BUG: a decimal outside its column's typmod reached the leaf codec")
		}
		x = u
	}
	switch fc.w {
	case 2:
		return uint64(uint16(x) ^ 0x8000)
	case 4:
		return uint64(uint32(x) ^ 0x80000000)
	}
	return uint64(x) ^ (1 << 63)
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
				r.add(forSlot(fc, v))
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

// leafDict makes this process write a string column of a leaf as a dictionary (JED_LEAF_DICT=on,
// with JED_LEAF_NARROW=on) when the region is smaller that way. The region's flags byte is
// regionDict; then a u16 entry count D (at most 256), D u16 end offsets, the entries, and N one-byte
// codes. An entry is a value's whole span (tag and body; the empty span for NULL), so a reader
// returns it as it returns a plain region's span. Two values share an entry only when both are
// inline-plain strings with equal content, or both NULL; a compressed or external value always has
// its own entry. Entries are in order of first use. The saving is
//
//	N − 2 − 2·D + (bytes of the spans that repeat an earlier one), when positive and D ≤ 256
//
// and joins the content-dependent leaf size of the key prefix and base-plus-offsets columns.
const leafDict = true

// regionDict is the region flags byte of a dictionary region.
const regionDict byte = 2

const (
	dictNull   = iota // a NULL: every NULL of the region shares the empty entry
	dictShared        // an inline-plain string: shares an entry with equal content
	dictOwn           // any other form: its own entry
)

// dictItem is one value as the dictionary sees it.
type dictItem struct {
	kind    uint8
	payload []byte // dictShared: the string's content
}

// dictAcc accumulates a region's dictionary facts.
type dictAcc struct {
	seen   map[string]int // rows holding each shared payload
	d, dup int
	nulls  int
	// addOnly says no value is ever removed. More than 256 entries then rule the dictionary out for
	// good (dictSaving), so the rest are not counted.
	addOnly bool
}

func (a *dictAcc) add(it dictItem, header int) {
	if a.addOnly && a.d > 256 {
		return
	}
	switch it.kind {
	case dictNull:
		if a.nulls == 0 {
			a.d++
		}
		a.nulls++
	case dictOwn:
		a.d++
	default:
		if a.seen == nil {
			a.seen = map[string]int{}
		}
		if k := a.seen[string(it.payload)]; k > 0 {
			a.seen[string(it.payload)] = k + 1
			a.dup += header + len(it.payload)
		} else {
			a.seen[string(it.payload)] = 1
			a.d++
		}
	}
}

// remove takes out one value add was given. The facts are those of the remaining values in any
// order: an entry is counted once however many rows share it.
func (a *dictAcc) remove(it dictItem, header int) {
	switch it.kind {
	case dictNull:
		a.nulls--
		if a.nulls == 0 {
			a.d--
		}
	case dictOwn:
		a.d--
	default:
		if k := a.seen[string(it.payload)]; k > 1 {
			a.seen[string(it.payload)] = k - 1
			a.dup -= header + len(it.payload)
		} else {
			delete(a.seen, string(it.payload))
			a.d--
		}
	}
}

// dictSaving is the bytes a dictionary saves in a region of n values with d entries and dup bytes
// of repeated spans; 0 when the plain region is no larger or the entries exceed a one-byte code.
func dictSaving(n, d, dup int) int {
	if d > 256 {
		return 0
	}
	if s := n - 2 - 2*d + dup; s > 0 {
		return s
	}
	return 0
}

// spanDictItem classifies a stored span.
func spanDictItem(span []byte) dictItem {
	if len(span) == 0 {
		return dictItem{kind: dictNull}
	}
	if span[0] == 0x00 || span[0] == tagInlineBare {
		if p, ok := inlineStringPayload(span); ok {
			return dictItem{kind: dictShared, payload: p}
		}
	}
	return dictItem{kind: dictOwn}
}

// rowItems classifies a decoded row's item-column values into out (one per shape.itemCols). A value
// is inline-plain when the record's disposition plan leaves it so; the plan is consulted only when the
// all-inline record does not fit RECORD_MAX.
func rowItems(shape leafShape, pcap pageCap, key []byte, row storedRow, out []dictItem) {
	size := len(key) + shape.fixedBytes
	for _, c := range shape.varCols {
		inline, _ := valueFacts(shape.colTypes[c], row[c])
		size += inline
	}
	var plan *recordPlan
	if size > recordMaxFor(pcap.c, len(shape.colTypes)) {
		p := planDispositionsMemo(shape.colTypes, key, row, pcap, shape.memo)
		plan = &p
	}
	for j, c := range shape.itemCols {
		v := row[c]
		switch {
		case v.IsNull():
			out[j] = dictItem{kind: dictNull}
		case plan != nil && plan.disp[c] != dispInline:
			out[j] = dictItem{kind: dictOwn}
		case v.Kind == ValUnfetched && v.unfetched().Form != 0x00:
			// A stored row's deferred values equal its own plan's output (large-values.md §15.2), so a
			// record that fits all-inline holds no large form.
			panic("BUG: a stored large form in a record whose plan is all-inline")
		case !isSpillable(shape.colTypes[c]):
			out[j] = dictItem{kind: dictOwn}
		default:
			out[j] = dictItem{kind: dictShared, payload: planningPayload(shape.colTypes[c], v)}
		}
	}
}

// leafSizes is the content-dependent size facts of a dirty decoded leaf, kept with the node so an
// insert updates them for one row instead of recomputing them for the leaf: each item column's
// dictionary, symbol-table gain and typed-array facts, and each base-plus-offsets column's value
// span. addRow and removeRow keep them equal to a fresh measurement of the node's rows.
type leafSizes struct {
	n    int
	accs []colAcc
	fors []forRange
	// forCnt is each base-plus-offsets column's present values. forStale marks a column whose range
	// lost a value at one of its ends: fors then only contains the true range, until refresh rescans
	// the column. stale says any column is.
	forCnt   []int
	forStale []bool
	stale    bool
	// tabs is the leaf's symbol tables by column (leaf_sym.go); nil until the leaf needs them.
	tabs []*symTable
	// coded says the gains were measured (under tabs, or the shape's per-column tables): the leaf did
	// not fit as plain regions when these facts were built.
	coded bool
	items []dictItem // scratch
}

// allRowItems classifies every row (items[i][j] is row i's item for shape.itemCols[j]); nil when the
// shape has no item column.
func allRowItems(shape leafShape, pcap pageCap, keys [][]byte, rows []storedRow) [][]dictItem {
	if len(shape.itemCols) == 0 {
		return nil
	}
	flat := make([]dictItem, len(rows)*len(shape.itemCols))
	items := make([][]dictItem, len(rows))
	for i, row := range rows {
		items[i] = flat[i*len(shape.itemCols) : (i+1)*len(shape.itemCols)]
		rowItems(shape, pcap, keys[i], row, items[i])
	}
	return items
}

// newLeafSizes measures rows. tabs is the symbol tables the leaf carries; when it carries none and
// train is set, they are trained from the rows.
func newLeafSizes(shape leafShape, pcap pageCap, keys [][]byte, rows []storedRow, tabs []*symTable, train bool) *leafSizes {
	return leafSizesOf(shape, rows, allRowItems(shape, pcap, keys, rows), tabs, train)
}

// leafSizesOf is newLeafSizes over rows already classified.
func leafSizesOf(shape leafShape, rows []storedRow, items [][]dictItem, tabs []*symTable, train bool) *leafSizes {
	sz := &leafSizes{
		n:        len(rows),
		accs:     make([]colAcc, len(shape.itemCols)),
		fors:     make([]forRange, len(shape.forCols)),
		forCnt:   make([]int, len(shape.forCols)),
		forStale: make([]bool, len(shape.forCols)),
		items:    make([]dictItem, len(shape.itemCols)),
		tabs:     tabs,
		coded:    train || tabs != nil,
	}
	// With per-column tables a leaf trains its own only when it overflows under those (buildLeaf).
	if tabs == nil && train && !leafSymCol {
		sz.tabs, _ = trainTabs(shape, items, nil)
	}
	for j, fc := range shape.forCols {
		for _, row := range rows {
			if v := row[fc.col]; !v.IsNull() {
				sz.fors[j].add(forSlot(fc, v))
				sz.forCnt[j]++
			}
		}
	}
	for j := range shape.itemCols {
		if shape.itemDict[j] {
			sz.accs[j].dict.seen = make(map[string]int, len(rows))
		}
	}
	// Dictionary and array facts first; then a column's coded gain, which is the expensive part,
	// only where coding could save more than the dictionary does (symCanBeat). A gain left at zero
	// understates the saving at worst, and rows added later add theirs.
	for i := range items {
		for j := range shape.itemCols {
			sz.accs[j].add(shape, j, items[i][j], nil)
		}
	}
	for j, c := range shape.itemCols {
		tb := sz.tab(shape, j, c)
		if tb == nil {
			continue
		}
		header := inlineHeaderLen(shape.colTypes[c])
		bound := 0
		for i := range items {
			if it := items[i][j]; it.kind == dictShared {
				bound += symGainMax(header, len(it.payload))
			}
		}
		f := &sz.accs[j].f
		dictS := 0
		if shape.itemDict[j] {
			dictS = dictSaving(sz.n, f.d, f.dup)
		}
		if !symCanBeat(sz.n, bound, dictS, tb, tabFor(sz.tabs, c) == nil) {
			continue
		}
		for i := range items {
			if it := items[i][j]; it.kind == dictShared {
				f.gain += header + len(it.payload) - tb.encLen(it.payload)
			}
		}
	}
	return sz
}

func (sz *leafSizes) addRow(shape leafShape, pcap pageCap, key []byte, row storedRow) {
	sz.n++
	for j, fc := range shape.forCols {
		if v := row[fc.col]; !v.IsNull() {
			sz.fors[j].add(forSlot(fc, v))
			sz.forCnt[j]++
		}
	}
	if len(shape.itemCols) == 0 {
		return
	}
	rowItems(shape, pcap, key, row, sz.items)
	for j, c := range shape.itemCols {
		sz.accs[j].add(shape, j, sz.items[j], sz.tab(shape, j, c))
	}
}

// removeRow takes one of the measured rows out. It reports false when the facts cannot be kept
// exact this way (a coded leaf, or a packed array holding an end of its element range); the caller
// then drops them and the leaf is measured again.
func (sz *leafSizes) removeRow(shape leafShape, pcap pageCap, key []byte, row storedRow) bool {
	if sz.coded {
		return false
	}
	if len(shape.itemCols) > 0 {
		rowItems(shape, pcap, key, row, sz.items)
		for j := range shape.itemCols {
			if !sz.accs[j].remove(shape, j, sz.items[j]) {
				return false
			}
		}
	}
	sz.n--
	for j, fc := range shape.forCols {
		v := row[fc.col]
		if v.IsNull() {
			continue
		}
		sz.forCnt[j]--
		r := &sz.fors[j]
		if sz.forCnt[j] == 0 {
			*r = forRange{}
			sz.forStale[j] = false
		} else if s := forSlot(fc, v); s == r.lo || s == r.hi {
			sz.forStale[j] = true
			sz.stale = true
		}
	}
	return true
}

// refresh rescans the columns whose range went stale. rows are the node's.
func (sz *leafSizes) refresh(shape leafShape, rows []storedRow) {
	if !sz.stale {
		return
	}
	for j, fc := range shape.forCols {
		if !sz.forStale[j] {
			continue
		}
		var r forRange
		for _, row := range rows {
			if v := row[fc.col]; !v.IsNull() {
				r.add(forSlot(fc, v))
			}
		}
		sz.fors[j] = r
		sz.forStale[j] = false
	}
	sz.stale = false
}

// tab is the table item column j's gain is measured under, nil while the leaf is not coded.
func (sz *leafSizes) tab(shape leafShape, j, c int) *symTable {
	if !sz.coded {
		return nil
	}
	return leafOrColTab(shape, sz.tabs, j, c)
}

// saving is the bytes the leaf's content-dependent regions save. sym says whether the leaf is
// written with its symbol tables: only a leaf that does not fit without them is. While a column's
// range is stale the result is an upper bound: the column is taken at its narrowest offsets.
func (sz *leafSizes) saving(shape leafShape, sym bool) int {
	total := 0
	for j, fc := range shape.forCols {
		if sz.forStale[j] {
			total += forSaving(sz.n, fc.w, 0, 0, true)
			continue
		}
		total += forSaving(sz.n, fc.w, sz.fors[j].lo, sz.fors[j].hi, sz.fors[j].any)
	}
	for j, c := range shape.itemCols {
		var tb *symTable
		if sym {
			tb = tabFor(sz.tabs, c)
		}
		total += itemSaving(shape, j, sz.n, sz.accs[j].f, tb, sym && sz.coded)
	}
	return total
}
