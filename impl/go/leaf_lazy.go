package jed

// Lazy dirty leaves (all three cores; no byte, result or cost change).
//
// Changing one row of a clean leaf used to decode every row of its page, measure every row for the
// content-dependent size, and at commit encode every value again. With JED_LEAF_LAZY on (the
// default; "off" restores the old paths, "check" also turns on the two cross-checks):
//
//  1. A dirty leaf copied from a packed leaf keeps its rows as that page's until they are replaced
//     (pnode.lazyBase / lazy). Readers decode such a row from the page each time, as they do for a
//     packed leaf; nothing is cached on a read path.
//  2. The leaf's size facts come from the page. A column no replacement changed is frozen: its
//     saving is the one the page shows. A changed column is measured from stored spans and dense
//     slots, without decoding values.
//  3. A leaf whose rows were only replaced is encoded by copying its page: the key section and every
//     unchanged column region verbatim, and a changed column rebuilt from the page's stored spans
//     plus the replaced rows' new values.
//
// Disabled while symbol tables are on (leaf_sym.go): a table changes a region's size and bytes.

import (
	"bytes"
	"fmt"
	"maps"
	"os"
	"slices"
	"sync/atomic"
)

// leafLazy turns the lazy paths on. A variable so one binary measures both ways and tests can run a
// workload under each.
var leafLazy atomic.Bool

// leafPatchCheck makes the commit encode a patched leaf both ways and compare the bytes (tests).
var leafPatchCheck atomic.Bool

// lazyStats counts how dirty lazy leaves were encoded: patched from their page, rebuilt from stored
// spans (rows were inserted or removed), or by the full encoder.
var lazyStats struct {
	patched, spans, full atomic.Int64
}

// setLeafLazy turns the lazy paths on or off, and reports whether they are now on. They stay off
// while a symbol-table switch is on (leaf_sym.go): the patch encoder writes no symbol-table region,
// so a lazy leaf would silently drop one. Every writer of leafLazy goes through here.
func setLeafLazy(on bool) bool {
	on = on && !leafSym && !leafSymCol
	leafLazy.Store(on)
	return on
}

func init() {
	v := os.Getenv("JED_LEAF_LAZY")
	setLeafLazy(v != "off")
	if v == "check" {
		leafSizesCheck.Store(true)
		leafPatchCheck.Store(true)
	}
}

// stored returns the packed leaf holding row i's stored bytes and the row's index there: the node's
// own page, or a lazy node's base page for a row it has not replaced. nil when vals[i] holds the row.
func (n *pnode) stored(i int) (*packedLeaf, int) {
	if n.packed != nil {
		return n.packed, i
	}
	if n.lazyBase != nil {
		if at := n.lazy[i]; at >= 0 {
			return n.lazyBase, int(at)
		}
	}
	return nil, 0
}

// arrSpanLen is the length of the plain span a typed-array region's count byte stands for
// (lazyRegion.decodeArr): nothing for NULL, the 3-byte empty array, or an 11-byte header and count
// elements of width w.
func arrSpanLen(count byte, w int) int {
	switch count {
	case 255:
		return 0
	case 0:
		return 3
	}
	return 11 + int(count)*w
}

// recordWeights is recordWeight for every record, read column by column from the directories, and
// their sum.
func (p *packedLeaf) recordWeights() ([]uint32, int) {
	l := p.dirs
	n := p.n
	w := make([]uint32, n)
	fixed := uint32(0)
	if l.keyLen > 0 {
		fixed = uint32(l.keyLen)
	} else {
		kp, lo := uint32(len(l.keyPrefix)), uint32(0)
		for i := range w {
			hi := paxDirEnd(l.keyEnd, i, l.dw)
			w[i] = kp + hi - lo
			lo = hi
		}
	}
	for c := range l.regions {
		r := &l.regions[c]
		switch {
		case r.elided:
		case r.width > 0:
			fixed += uint32(r.width)
		case r.lazy != nil && r.lazy.kind == regionArr:
			ew := arrElemWidthOf(r.lazy.ty)
			for i := range w {
				w[i] += uint32(arrSpanLen(r.lazy.raw[i], ew))
			}
		case r.lazy != nil:
			for i := range w {
				w[i] += uint32(l.valueLen(c, i))
			}
		case r.codes != nil:
			var lens [256]uint32
			lo := uint32(0)
			for e := 0; e < len(r.ends)/l.dw; e++ {
				hi := paxDirEnd(r.ends, e, l.dw)
				lens[e] = hi - lo
				lo = hi
			}
			for i := range w {
				w[i] += lens[r.codes[i]]
			}
		default:
			lo := uint32(0)
			for i := range w {
				hi := paxDirEnd(r.ends, i, l.dw)
				w[i] += hi - lo
				lo = hi
			}
		}
	}
	total := 0
	for i := range w {
		w[i] += fixed
		total += int(w[i])
	}
	if leafSizesCheck.Load() {
		for i := range w {
			if want := p.recordWeight(i); w[i] != want {
				panic(fmt.Sprintf("BUG: record %d weighs %d by columns, %d by record", i, w[i], want))
			}
		}
	}
	return w, total
}

// ownKeys copies keys into one allocation.
func ownKeys(keys [][]byte) [][]byte {
	total := 0
	for _, k := range keys {
		total += len(k)
	}
	blob := make([]byte, 0, total)
	out := make([][]byte, len(keys))
	for i, k := range keys {
		at := len(blob)
		blob = append(blob, k...)
		out[i] = blob[at:len(blob):len(blob)]
	}
	return out
}

// lazyCopy returns a dirty copy of leaf n that generation owns, with no row decoded: n is a packed
// leaf, or a lazy leaf an earlier generation built.
//
// The copy's keys are views of the page (or of the keys the packed leaf rebuilt from its prefix). The
// copy already holds the page through lazyBase, and a page block is never written again. A split or
// merge copies the keys out first (materialize, decodedParts), so no separator keeps a page alive.
func lazyCopy(n *pnode, generation *mutationGeneration) *pnode {
	origin, _ := originOf(n)
	out := &pnode{owner: generation, origin: origin}
	if p := n.packed; p != nil {
		out.lazyBase = p
		out.keys = make([][]byte, p.n)
		out.lazy = make([]int32, p.n)
		for i := range out.keys {
			out.keys[i] = p.key(i)
			out.lazy[i] = int32(i)
		}
		out.vals = make([]storedRow, p.n)
		out.weights, out.wsum = p.recordWeights()
		out.wsumOK = true
		out.sameRows = true
		out.patchOK = p.inlineOnly()
		out.colDirty = make([]bool, len(p.colTypes))
		if !out.patchOK {
			// A record of the page has a large-value form. Its plan can change with any column, so
			// every column counts as changed from the first replacement.
			for c := range out.colDirty {
				out.colDirty[c] = true
			}
		}
		return out
	}
	out.lazyBase = n.lazyBase
	out.keys = cloneKeys(n.keys)
	out.vals = cloneVals(n.vals)
	out.weights = cloneWeights(n.weights)
	out.lazy = slices.Clone(n.lazy)
	out.wsum, out.wsumOK = n.totalWeight(), true
	out.sameRows, out.patchOK = n.sameRows, n.patchOK
	out.colDirty = slices.Clone(n.colDirty)
	out.sizes = n.sizes.clone()
	return out
}

// trackWeight starts the running weight total of a leaf its writer is about to edit in place.
func (n *pnode) trackWeight() {
	if n.wsumOK || !leafLazy.Load() {
		return
	}
	n.wsum = 0
	for _, w := range n.weights {
		n.wsum += int(w)
	}
	n.wsumOK = true
}

// rowSetChanged records that a row was inserted into or removed from a lazy leaf: its rows are no
// longer its page's, so no region can be copied and frozen facts no longer hold.
func (n *pnode) rowSetChanged() {
	n.sameRows, n.colDirty = false, nil
	if n.sizes != nil && n.sizes.frozen > 0 {
		n.sizes = nil
	}
}

// materialize decodes every row a lazy leaf still reads from its page and copies its keys, making it
// an ordinary decoded leaf. Only the owning writer calls it.
func (n *pnode) materialize() error {
	rows, err := n.decodedRows()
	if err != nil {
		return err
	}
	n.vals = rows
	n.keys = ownKeys(n.keys)
	n.lazyBase, n.lazy = nil, nil
	n.sameRows, n.patchOK, n.colDirty = false, false, nil
	n.sizes = nil // buildLeaf measures the rows
	return nil
}

// storedSame reports whether the leaf encoder, writing v as column c of an all-inline record, would
// write exactly the bytes page p stores for record i of that column. It answers false whenever that
// is not certain.
//
// An UPDATE's unassigned variable-width value reaches the write as the deferred handle the scan read
// from this page, so the first test is identity: the same typed-array region and index, or a body
// that starts at the same address with the same length. Otherwise the bodies are compared, which
// also covers a page re-read since the scan and a value assigned its old content. A fixed-width
// value is compared with the dense slot.
func storedSame(ty colType, v Value, p *packedLeaf, c, i int) bool {
	l := p.dirs
	r := &l.regions[c]
	if r.elided {
		return true // rebuilt from the key, which is the same
	}
	null := l.isNull(c, i)
	if v.IsNull() || null {
		return v.IsNull() && null
	}
	if r.width > 0 {
		if v.Kind == ValUnfetched {
			return false
		}
		slot := r.body[i*r.width : (i+1)*r.width]
		if forEligible(ty) {
			return beUint(slot) == forSlot(forCol{col: c, w: r.width, dec: ty.DecSlot != 0, scale: ty.DecScale}, v)
		}
		return bytes.Equal(encodeValue(ty, v)[1:], slot)
	}
	str := stringPayload(ty)
	var u *Unfetched
	if v.Kind == ValUnfetched {
		u = v.unfetched()
		if u.Form != 0x00 {
			return false
		}
		if u.lazy != nil && u.lazy == r.lazy {
			return u.lazyIdx == i
		}
	}
	span, err := l.value(c, i)
	if err != nil || len(span) == 0 {
		return false
	}
	// The inline form this process writes: a bare string, or the tag and the body.
	if str {
		if span[0] != tagInlineBare {
			return false
		}
	} else if span[0] != 0x00 {
		return false
	}
	old := span[1:]
	if u == nil {
		if str {
			return string(old) == v.str()
		}
		return bytes.Equal(encodeValue(ty, v)[1:], old)
	}
	var body []byte
	if str {
		body = u.inlinePayload()
	} else if body, err = u.bodyErr(); err != nil {
		return false
	}
	if len(body) != len(old) {
		return false
	}
	return len(body) == 0 || &body[0] == &old[0] || bytes.Equal(body, old)
}

// rowAllInline reports whether a record is stored all-inline: its all-inline size fits RECORD_MAX,
// so no disposition plan runs (rowItems).
func rowAllInline(shape leafShape, pcap pageCap, key []byte, row storedRow) bool {
	size := len(key) + shape.fixedBytes
	for _, c := range shape.varCols {
		inline, _ := valueFacts(shape.colTypes[c], row[c])
		size += inline
	}
	return size <= recordMaxFor(pcap.c, len(shape.colTypes))
}

// replaceRow replaces row i of a lazy leaf its writer owns.
func (n *pnode) replaceRow(i int, key []byte, val storedRow, weight uint32, shape leafShape, pcap pageCap) {
	wasLazy := n.lazy[i] >= 0
	var buf [32]bool
	var changed []bool
	if n.sameRows {
		// Row i is the page's record i. Which columns does the new row store differently?
		base := n.lazyBase
		k := len(shape.colTypes)
		if k <= len(buf) {
			changed = buf[:k]
		} else {
			changed = make([]bool, k)
		}
		size := len(key) + shape.fixedBytes
		for c, ty := range shape.colTypes {
			same := storedSame(ty, val[c], base, c, i)
			changed[c] = !same
			if _, fixed := fixedValueWidth(ty); !fixed {
				if same {
					size += base.dirs.valueLen(c, i)
				} else {
					inline, _ := valueFacts(ty, val[c])
					size += inline
				}
			}
		}
		// size is the record's all-inline size (rowItems). Past RECORD_MAX its disposition plan
		// decides each value's form, so no column is known unchanged and the leaf is not patched.
		if size > recordMaxFor(pcap.c, k) {
			n.patchOK = false
		}
		for c := range changed {
			if !n.patchOK {
				changed[c] = true
			}
			if changed[c] {
				n.colDirty[c] = true
			}
		}
	} else if n.patchOK && !rowAllInline(shape, pcap, key, val) {
		n.patchOK = false
	}
	redo := false
	if sz := n.sizes; sz != nil {
		switch {
		case sz.n != len(n.vals):
			n.sizes = nil
		case n.sameRows:
			if redo = sz.replaceCols(n, i, key, val, shape, pcap, changed, wasLazy); !redo {
				n.sizes = nil
			}
		default:
			if old, err := n.rowAt(i); err == nil && sz.removeRow(shape, pcap, key, old) {
				sz.addRow(shape, pcap, key, val)
			} else {
				n.sizes = nil
			}
		}
	}
	n.vals[i] = val
	n.lazy[i] = -1
	n.wsum += int(weight) - int(n.weights[i])
	n.weights[i] = weight
	if redo {
		n.sizes.remeasure(n, shape, pcap)
	}
}

// --- size facts from the page ---

// regionSaving is the bytes column c's region saves on this page: its plain size less its stored
// size, 0 for a plain region.
func (p *packedLeaf) regionSaving(c int) int {
	l := p.dirs
	r := &l.regions[c]
	if r.stored == 0 {
		return 0
	}
	n := p.n
	plain := 1 + l.dw*n
	switch {
	case r.width > 0:
		plain = 1 + (n+7)/8 + n*r.width
	case r.codes != nil:
		var lens [256]int
		lo := uint32(0)
		for e := 0; e < len(r.ends)/l.dw; e++ {
			hi := paxDirEnd(r.ends, e, l.dw)
			lens[e] = int(hi - lo)
			lo = hi
		}
		for _, code := range r.codes {
			plain += lens[code]
		}
	case r.lazy.kind == regionArr:
		ew := arrElemWidthOf(r.lazy.ty)
		for _, count := range r.lazy.raw[:n] {
			plain += arrSpanLen(count, ew)
		}
	default:
		plain += len(r.lazy.plain().body)
	}
	return plain - r.stored
}

// forRangeOf is the span and count of base-plus-offsets column fc's present values over a lazy
// leaf's rows: dense slots for the rows still on the page, values for the others.
func (n *pnode) forRangeOf(fc forCol) (r forRange, count int) {
	reg := &n.lazyBase.dirs.regions[fc.col]
	for i, at := range n.lazy {
		if at < 0 {
			if v := n.vals[i][fc.col]; !v.IsNull() {
				r.add(forSlot(fc, v))
				count++
			}
			continue
		}
		if reg.bitmap[at/8]&(0x80>>(at%8)) != 0 {
			continue
		}
		r.add(beUint(reg.body[int(at)*fc.w : (int(at)+1)*fc.w]))
		count++
	}
	return r, count
}

// addN is add for k values with the same item.
func (a *dictAcc) addN(it dictItem, header, k int) {
	switch it.kind {
	case dictNull:
		if a.nulls == 0 {
			a.d++
		}
		a.nulls += k
	case dictOwn:
		a.d += k
	default:
		if a.seen == nil {
			a.seen = map[string]int{}
		}
		if have := a.seen[string(it.payload)]; have > 0 {
			a.seen[string(it.payload)] = have + k
			a.dup += k * (header + len(it.payload))
		} else {
			a.seen[string(it.payload)] = k
			a.d++
			a.dup += (k - 1) * (header + len(it.payload))
		}
	}
}

// itemAccOf measures item column j over a lazy leaf's rows. A row still on the page is classified
// from its stored span (spanItem), with no value decoded; where the page stores the column as a
// dictionary, the rows are counted by code and each entry is added once. scratch holds one row's
// items.
func (n *pnode) itemAccOf(shape leafShape, pcap pageCap, j int, scratch []dictItem) colAcc {
	c := shape.itemCols[j]
	ty := shape.colTypes[c]
	l := n.lazyBase.dirs
	r := &l.regions[c]
	var acc colAcc
	byCode := r.codes != nil && shape.itemDict[j] && shape.itemArr[j] == 0
	if shape.itemDict[j] && !byCode {
		acc.dict.seen = make(map[string]int, len(n.vals))
	}
	var counts [256]int
	for i, at := range n.lazy {
		switch {
		case at < 0:
			rowItems(shape, pcap, n.keys[i], n.vals[i], scratch)
			acc.add(shape, j, scratch[j], nil)
		case byCode:
			counts[r.codes[at]]++
		default:
			it := dictItem{kind: dictOwn}
			if span, err := l.value(c, int(at)); err == nil {
				it = spanItem(ty, span)
			}
			acc.add(shape, j, it, nil)
		}
	}
	if byCode {
		header := inlineHeaderLen(ty)
		lo := uint32(0)
		for e := 0; e < len(r.ends)/l.dw; e++ {
			hi := paxDirEnd(r.ends, e, l.dw)
			if k := counts[e]; k > 0 {
				acc.dict.addN(spanDictItem(r.body[lo:hi]), header, k)
			}
			lo = hi
		}
		acc.f.d, acc.f.dup = acc.dict.d, acc.dict.dup
	}
	return acc
}

// newLazySizes builds a lazy leaf's size facts.
//
// While the leaf's rows are its page's (sameRows), a column no replacement changed is frozen: its
// saving is taken to be what the page shows, regionSaving. That rests on one invariant: the writer
// stores a region in a compact form exactly when the size rule says that form is smaller, and the
// form's stored size is the plain size less the rule's saving (encodeLeafPAXMemo keeps its running
// size that way). A page some other writer produced without it would be measured differently here
// than by newLeafSizes; leafSizesCheck compares the two at every use.
func (n *pnode) newLazySizes(shape leafShape, pcap pageCap) *leafSizes {
	sz := &leafSizes{
		n:        len(n.vals),
		accs:     make([]colAcc, len(shape.itemCols)),
		fors:     make([]forRange, len(shape.forCols)),
		forCnt:   make([]int, len(shape.forCols)),
		forStale: make([]bool, len(shape.forCols)),
		items:    make([]dictItem, len(shape.itemCols)),
	}
	if n.sameRows {
		sz.forFrozen, sz.forSave = make([]bool, len(shape.forCols)), make([]int, len(shape.forCols))
		sz.itemFrozen, sz.itemSave = make([]bool, len(shape.itemCols)), make([]int, len(shape.itemCols))
	}
	for j, fc := range shape.forCols {
		if n.sameRows && !n.colDirty[fc.col] {
			sz.forFrozen[j], sz.forSave[j] = true, n.lazyBase.regionSaving(fc.col)
			sz.frozen++
			continue
		}
		sz.fors[j], sz.forCnt[j] = n.forRangeOf(fc)
	}
	for j, c := range shape.itemCols {
		if n.sameRows && !n.colDirty[c] {
			sz.itemFrozen[j], sz.itemSave[j] = true, n.lazyBase.regionSaving(c)
			sz.frozen++
			continue
		}
		sz.accs[j] = n.itemAccOf(shape, pcap, j, sz.items)
	}
	return sz
}

// newLeafSizesPacked measures a packed leaf's rows from its page, with no value decoded and no
// column frozen. It equals newLeafSizes over the decoded rows.
func newLeafSizesPacked(shape leafShape, pcap pageCap, p *packedLeaf) *leafSizes {
	view := &pnode{lazyBase: p, lazy: make([]int32, p.n), vals: make([]storedRow, p.n)}
	for i := range view.lazy {
		view.lazy[i] = int32(i)
	}
	return view.newLazySizes(shape, pcap)
}

// lazySaving is contentSaving's item and base-plus-offsets part for a lazy leaf.
func (n *pnode) lazySaving(pcap pageCap, shape leafShape) int {
	sz := n.sizes
	if sz == nil || sz.n != len(n.vals) {
		sz = n.newLazySizes(shape, pcap)
		if n.page == 0 && n.owner != nil {
			n.sizes = sz // a dirty node is its writer's alone
		}
	}
	if sz.stale {
		for j, fc := range shape.forCols {
			if sz.forStale[j] {
				sz.fors[j], sz.forCnt[j] = n.forRangeOf(fc)
				sz.forStale[j] = false
			}
		}
		sz.stale = false
	}
	got := sz.saving(shape, false)
	if leafSizesCheck.Load() {
		rows, err := n.decodedRows()
		if err != nil {
			panic(err)
		}
		if want := newLeafSizes(shape, pcap, n.keys, rows, nil, false).saving(shape, false); want != got {
			panic(fmt.Sprintf("BUG: a lazy leaf's size facts save %d bytes (%d columns frozen), a fresh measurement %d", got, sz.frozen, want))
		}
	}
	return got
}

// replaceCols follows the replacement of row i (not yet applied to n) in a leaf whose rows are its
// page's. changed[c] says the new row stores column c differently from the page; wasLazy that the
// row being replaced is still the page's. A frozen column the replacement changes is thawed; a
// measured column takes the old value out and the new one in. Columns to measure again are left in
// redoFor / redoItem for remeasure, which runs once the row is in place. False when the facts cannot
// be kept.
func (sz *leafSizes) replaceCols(n *pnode, i int, key []byte, val storedRow, shape leafShape, pcap pageCap, changed []bool, wasLazy bool) bool {
	sz.redoFor, sz.redoItem = sz.redoFor[:0], sz.redoItem[:0]
	// A measured column is untouched only when the old row is the page's and the new one stores the
	// same bytes there.
	touched := func(c int) bool { return changed[c] || !wasLazy }
	need := false
	for j, fc := range shape.forCols {
		need = need || (!sz.forFrozen[j] && touched(fc.col))
	}
	needItems := false
	for j, c := range shape.itemCols {
		needItems = needItems || (!sz.itemFrozen[j] && touched(c))
	}
	if need || needItems {
		old, err := n.rowAt(i)
		if err != nil {
			return false
		}
		for j, fc := range shape.forCols {
			if sz.forFrozen[j] || !touched(fc.col) {
				continue
			}
			ov, nv := old[fc.col], val[fc.col]
			if !ov.IsNull() {
				sz.forCnt[j]--
				r := &sz.fors[j]
				if sz.forCnt[j] == 0 {
					*r = forRange{}
					sz.forStale[j] = false
				} else if s := forSlot(fc, ov); s == r.lo || s == r.hi {
					sz.forStale[j] = true
					sz.stale = true
				}
			}
			if !nv.IsNull() {
				sz.fors[j].add(forSlot(fc, nv))
				sz.forCnt[j]++
			}
		}
		if needItems {
			if sz.items2 == nil {
				sz.items2 = make([]dictItem, len(shape.itemCols))
			}
			rowItems(shape, pcap, key, old, sz.items)
			rowItems(shape, pcap, key, val, sz.items2)
			for j, c := range shape.itemCols {
				if sz.itemFrozen[j] || !touched(c) {
					continue
				}
				a, b := sz.items[j], sz.items2[j]
				if a.kind == b.kind && bytes.Equal(a.payload, b.payload) {
					continue
				}
				if sz.accs[j].remove(shape, j, a) {
					sz.accs[j].add(shape, j, b, nil)
				} else {
					sz.redoItem = append(sz.redoItem, j)
				}
			}
		}
	}
	for j, fc := range shape.forCols {
		if sz.forFrozen[j] && changed[fc.col] {
			sz.forFrozen[j] = false
			sz.frozen--
			sz.redoFor = append(sz.redoFor, j)
		}
	}
	for j, c := range shape.itemCols {
		if sz.itemFrozen[j] && changed[c] {
			sz.itemFrozen[j] = false
			sz.frozen--
			sz.redoItem = append(sz.redoItem, j)
		}
	}
	return true
}

// remeasure measures the columns replaceCols left for it, over the leaf's rows as they now are.
func (sz *leafSizes) remeasure(n *pnode, shape leafShape, pcap pageCap) {
	for _, j := range sz.redoFor {
		sz.fors[j], sz.forCnt[j] = n.forRangeOf(shape.forCols[j])
		sz.forStale[j] = false
	}
	for _, j := range sz.redoItem {
		sz.accs[j] = n.itemAccOf(shape, pcap, j, sz.items)
	}
	sz.redoFor, sz.redoItem = sz.redoFor[:0], sz.redoItem[:0]
}

// clone copies the facts for a new generation's copy of the leaf.
func (sz *leafSizes) clone() *leafSizes {
	if sz == nil {
		return nil
	}
	out := *sz
	out.accs = slices.Clone(sz.accs)
	for j := range out.accs {
		out.accs[j].dict.seen = maps.Clone(sz.accs[j].dict.seen)
	}
	out.fors = slices.Clone(sz.fors)
	out.forCnt = slices.Clone(sz.forCnt)
	out.forStale = slices.Clone(sz.forStale)
	out.forFrozen, out.forSave = slices.Clone(sz.forFrozen), slices.Clone(sz.forSave)
	out.itemFrozen, out.itemSave = slices.Clone(sz.itemFrozen), slices.Clone(sz.itemSave)
	out.items = make([]dictItem, len(sz.items))
	out.items2, out.redoFor, out.redoItem = nil, nil, nil
	return &out
}

// --- patch encode ---

// inlineOnly reports whether every variable-width value of the page is NULL or stored in the inline
// form this process writes: tag 0x05 and the bytes for a string, tag 0x00 and the body for any other
// type, or an element of a typed-array region. Such a page holds no pointer to an overflow chain and
// no compressed block, and its records all fit RECORD_MAX all-inline.
func (p *packedLeaf) inlineOnly() bool {
	switch p.inline.Load() {
	case 1:
		return true
	case 2:
		return false
	}
	ok := p.scanInline()
	if ok {
		p.inline.Store(1)
	} else {
		p.inline.Store(2)
	}
	return ok
}

func (p *packedLeaf) scanInline() bool {
	l := p.dirs
	for c, ty := range p.colTypes {
		r := &l.regions[c]
		if r.width > 0 {
			continue
		}
		if r.lazy != nil {
			if r.lazy.kind != regionArr {
				return false
			}
			continue
		}
		want := byte(0x00)
		if stringPayload(ty) {
			want = tagInlineBare
		}
		// A dictionary's directory and blob are its entries'; a plain region's are its values'.
		lo := uint32(0)
		for e := 0; e < len(r.ends)/l.dw; e++ {
			hi := paxDirEnd(r.ends, e, l.dw)
			if int(hi) > len(r.body) || (hi > lo && r.body[lo] != want) {
				return false
			}
			lo = hi
		}
	}
	return true
}

// hasChains reports whether any value of the page is an external pointer: its bytes are on an
// overflow chain. A region that cannot be read counts as holding one.
func (p *packedLeaf) hasChains() bool {
	l := p.dirs
	for c := range l.regions {
		r := &l.regions[c]
		if r.width > 0 || r.elided {
			continue
		}
		if r.lazy != nil && r.lazy.kind == regionArr {
			continue // a typed-array region holds inline arrays and NULLs only
		}
		// A dictionary's directory and blob are its entries'; a plain region's are its values'.
		ends, body, dw, err := l.variable(r)
		if err != nil {
			return true
		}
		lo := uint32(0)
		for e := 0; e < len(ends)/dw; e++ {
			hi := paxDirEnd(ends, e, dw)
			if int(hi) > len(body) {
				return true
			}
			if hi > lo && (body[lo] == tagExternal || body[lo] == tagExternalComp) {
				return true
			}
			lo = hi
		}
	}
	return false
}

// decodedLeafCopy returns leaf n as an ordinary decoded leaf with the same page: every row decoded,
// its keys copied. For a persisted lazy root that is not re-faulted (storage.go demoteCleanLeaves).
func decodedLeafCopy(n *pnode) (*pnode, error) {
	keys, vals, weights, err := n.decodedParts()
	if err != nil {
		return nil, err
	}
	return &pnode{keys: keys, vals: vals, weights: weights, page: n.page, tag: n.tag}, nil
}

// encodeLazyLeaf is a dirty lazy leaf's payload: patched from its page when it can be, else the full
// encoder's over its rows.
func encodeLazyLeaf(n *pnode, colTypes []colType, keyLen int, seal sealer, take func() uint32, ovf *[]dirtyPage, src *chainSource, memo *compressMemo) ([]byte, error) {
	patch, ok := patchLeaf(n, colTypes, keyLen, seal, src)
	if ok {
		if n.sameRows {
			lazyStats.patched.Add(1)
		} else {
			lazyStats.spans.Add(1)
		}
		if !leafPatchCheck.Load() {
			return patch, nil
		}
	} else {
		lazyStats.full.Add(1)
	}
	rows, err := n.decodedRows()
	if err != nil {
		return nil, err
	}
	// A patched leaf's records are all inline, so encoding it again takes no page and keeps no chain.
	full, err := encodeLeafPAXMemo(colTypes, keyLen, n.keys, rows, seal, take, ovf, src, nil, false, memo)
	if err != nil {
		return nil, err
	}
	if ok && !bytes.Equal(patch, full) {
		at := 0
		for at < len(patch) && at < len(full) && patch[at] == full[at] {
			at++
		}
		panic(fmt.Sprintf("BUG: a patched leaf of %d records differs from the full encoder's (%d and %d bytes) at byte %d", len(rows), len(patch), len(full), at))
	}
	return full, nil
}

// patchLeaf encodes a lazy leaf from its page, or reports false when the full encoder must run.
//
// It applies when every record of the page and every row written since is stored all-inline (so no
// overflow chain is kept, shared or written, and no disposition plan is consulted) and the column
// types are the page's. A row still on the page then contributes its stored spans, undecoded, and a
// row written since its newly encoded values. While the leaf's keys are the page's (rows were only
// replaced) the key section is the page's and a column no replacement changed keeps the page's
// region; otherwise the key section and every region are built again.
//
// This is the full encoder's output because that encoder, given a page's own rows, writes the page
// again: an inline value read from a page is written from its stored bytes, and a region's form
// depends only on its values.
func patchLeaf(n *pnode, colTypes []colType, keyLen int, seal sealer, src *chainSource) ([]byte, bool) {
	p := n.lazyBase
	if !n.patchOK || (n.sameRows && len(n.vals) != p.n) || !sameLeafShape(p.colTypes, colTypes) || !p.inlineOnly() {
		return nil, false
	}
	l := p.dirs
	rows := len(n.vals)
	k := len(colTypes)
	const dw = 2
	if l.dw != dw {
		return nil, false
	}
	regions := make([][]byte, k) // a changed column's new region
	plan := recordPlan{disp: make([]valueDisp, k), payloadLen: make([]int, k)}
	var zero [16]byte
	for c, ty := range colTypes {
		if leafElideKey && ty.keyAt != 0 {
			// Not stored. A replaced row's value must still be the one its key gives.
			for i, at := range n.lazy {
				if at >= 0 {
					continue
				}
				if v := n.vals[i][c]; v.IsNull() || !bytes.Equal(keyColumnSlot(ty, n.keys[i]), encodeValue(ty, v)[1:]) {
					return nil, false // the full encoder reports it
				}
			}
			continue
		}
		if n.sameRows && !n.colDirty[c] {
			continue
		}
		w, fixed := fixedValueWidth(ty)
		vb := make([][]byte, rows)
		nulls := make([]bool, rows)
		for i, at := range n.lazy {
			if at >= 0 {
				// Still the page's record.
				switch {
				case l.isNull(c, int(at)):
					nulls[i] = true
					if fixed {
						vb[i] = zero[:w]
					}
				default:
					span, err := l.value(c, int(at))
					if err != nil {
						return nil, false // the full encoder reports it
					}
					vb[i] = span
				}
				continue
			}
			v := n.vals[i][c]
			switch {
			case v.IsNull():
				nulls[i] = true
				if fixed {
					vb[i] = zero[:w]
				}
			case fixed:
				vb[i] = encodeValue(ty, v)[1:]
			default:
				b, err := encodeDisposedValue(ty, v, &plan, c, seal, nil, nil, src)
				if err != nil {
					return nil, false // the full encoder reports it
				}
				vb[i] = b
			}
		}
		regions[c] = encodeRegion(ty, rows, vb, nulls)
	}
	// The key section, a new column directory, then each region.
	var out []byte
	if n.sameRows {
		out = append(make([]byte, 0, len(l.payload)), l.payload[:l.keyStored]...)
	} else {
		var err error
		if out, err = encodeLeafKeys(keyLen, n.keys); err != nil {
			return nil, false // the full encoder reports it
		}
	}
	// A column with no new region keeps the page's: unchanged, or not stored at all.
	extent := func(c int) (int, int) {
		at := l.keyStored + dw*c
		return int(l.payload[at])<<8 | int(l.payload[at+1]), int(l.payload[at+2])<<8 | int(l.payload[at+3])
	}
	cur := len(out) + dw*(k+1)
	for c := 0; c <= k; c++ {
		out = append(out, byte(cur>>8), byte(cur))
		if c == k {
			break
		}
		if regions[c] != nil {
			cur += len(regions[c])
		} else {
			lo, hi := extent(c)
			cur += hi - lo
		}
	}
	for c := 0; c < k; c++ {
		if regions[c] != nil {
			out = append(out, regions[c]...)
		} else {
			lo, hi := extent(c)
			out = append(out, l.payload[lo:hi]...)
		}
	}
	return out, true
}

// encodeRegion is one column region of n values: vb[i] is value i's slot (fixed-width; zeros for
// NULL) or tagged span (variable-width; empty for NULL). It is the one place a region's form and
// bytes are decided — plain, base plus offsets, dictionary or packed array — for the full encoder
// (encodeLeafPAXMemo, which alone may then prefer a symbol-table region) and the patch encoder alike.
func encodeRegion(ty colType, n int, vb [][]byte, nulls []bool) []byte {
	const dw = 2
	appendDir := func(b []byte, v int) []byte { return append(b, byte(v>>8), byte(v)) }
	if w, fixed := fixedValueWidth(ty); fixed {
		out := make([]byte, 1, 1+(n+7)/8+n*w)
		useFOR, base, bw := false, uint64(0), 0
		if forEligible(ty) {
			var r forRange
			for i := 0; i < n; i++ {
				if !nulls[i] {
					r.add(beUint(vb[i]))
				}
			}
			if forSaving(n, w, r.lo, r.hi, r.any) > 0 {
				useFOR, base, bw = true, r.lo, forWidth(w, r.lo, r.hi)
				out[0] = regionFOR
			}
		}
		bm := len(out)
		out = append(out, make([]byte, (n+7)/8)...)
		for i, isNull := range nulls {
			if isNull {
				out[bm+i/8] |= 0x80 >> (i % 8)
			}
		}
		if useFOR {
			at := len(out)
			out = append(out, make([]byte, w+1+n*bw)...)
			putBEUint(out[at:at+w], base)
			out[at+w] = byte(bw)
			at += w + 1
			for i := 0; i < n; i++ {
				if !nulls[i] {
					putBEUint(out[at+i*bw:at+(i+1)*bw], beUint(vb[i])-base)
				}
			}
			return out
		}
		for i := 0; i < n; i++ {
			out = append(out, vb[i]...)
		}
		return out
	}
	bodies := 0
	for i := 0; i < n; i++ {
		bodies += len(vb[i])
	}
	if leafDict && stringPayload(ty) {
		// A dictionary: entries in order of first use.
		var acc dictAcc
		var entries [][]byte
		codes := make([]byte, n)
		index := map[string]int{}
		nullAt := -1
		header := inlineHeaderLen(ty)
		for i := 0; i < n && len(entries) <= 256; i++ {
			span := vb[i]
			it := spanDictItem(span)
			acc.add(it, header)
			at := -1
			switch it.kind {
			case dictNull:
				at = nullAt
			case dictShared:
				if j, ok := index[string(it.payload)]; ok {
					at = j
				}
			}
			if at < 0 {
				at = len(entries)
				entries = append(entries, span)
				switch it.kind {
				case dictNull:
					nullAt = at
				case dictShared:
					index[string(it.payload)] = at
				}
			}
			codes[i] = byte(at)
		}
		if dictSaving(n, acc.d, acc.dup) > 0 && len(entries) <= 256 {
			out := []byte{regionDict, byte(len(entries) >> 8), byte(len(entries))}
			off := 0
			for _, e := range entries {
				off += len(e)
				out = appendDir(out, off)
			}
			for _, e := range entries {
				out = append(out, e...)
			}
			return append(out, codes...)
		}
	}
	if w := arrElemWidth(ty); w != 0 {
		var acc arrAcc
		items := make([]dictItem, n)
		for i := 0; i < n; i++ {
			items[i] = spanItem(ty, vb[i])
			acc.add(items[i], w)
		}
		if arrSaving(n, w, dw, acc) > 0 {
			out := make([]byte, 1, 1+n+w+1)
			out[0] = regionArr
			for _, it := range items {
				if it.kind == dictNull {
					out = append(out, 255)
					continue
				}
				elems, _ := arrElems(it.payload, w)
				out = append(out, byte(len(elems)/w))
			}
			at := len(out)
			bw := arrBits(acc.r)
			out = append(out, make([]byte, w+1+(acc.elems*bw+7)/8)...)
			putBEUint(out[at:at+w], acc.r.lo)
			out[at+w] = byte(bw)
			data, bit := out[at+w+1:], 0
			for _, it := range items {
				if it.kind != dictShared {
					continue
				}
				elems, _ := arrElems(it.payload, w)
				for e := 0; e < len(elems); e += w {
					putBits(data, bit, bw, beUint(elems[e:e+w])-acc.r.lo)
					bit += bw
				}
			}
			return out
		}
	}
	out := make([]byte, 1, 1+dw*n+bodies)
	off := 0
	for i := 0; i < n; i++ {
		off += len(vb[i])
		out = appendDir(out, off)
	}
	for i := 0; i < n; i++ {
		out = append(out, vb[i]...)
	}
	return out
}
