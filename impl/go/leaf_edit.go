package jed

// PROTOTYPE (spec/design/leaf-write-cost.md §3): how a dirty leaf's parallel arrays — keys, vals,
// weights and, for a lazy leaf, lazy — take an inserted or removed row. No byte, result or cost
// changes; the arrays are in-memory only. JED_LEAF_EDIT selects:
//
//   - unset / "tail" (the default): shift the rows after the edited one, as always.
//   - "short": shift whichever side of the edited row is shorter. Removing from the front side moves
//     the slice start forward; inserting there uses headroom the leaf keeps before its first row.
//   - "gap": a movable gap. The arrays hold logical rows plus a run of empty slots at gapAt; an edit
//     moves the gap to the edited row (copying only the rows between) and then grows or shrinks it,
//     so consecutive edits at nearby positions copy almost nothing. The hot paths (search, the row
//     and key accessors, the in-place edits, the size facts) read through the gap; every other site
//     that indexes the arrays closes it first (closeGap). A gap lives only in a node its active
//     generation owns, and the generation closes all of them when it is frozen, so no other
//     reader ever sees one.

import (
	"math"
	"os"
)

const (
	leafEditTail = iota
	leafEditShort
	leafEditGap
)

var leafEditMode = func() int {
	switch os.Getenv("JED_LEAF_EDIT") {
	case "short":
		return leafEditShort
	case "gap":
		return leafEditGap
	}
	return leafEditTail
}()

// leafEditStats counts the array slots edits moved (JED_LEAF_EDIT_STATS: tests and the design note).
var leafEditStats struct {
	edits, moved, closes, closeMoved int64
}

var leafEditCount = os.Getenv("JED_LEAF_EDIT_STATS") != ""

// gapPoison marks an empty lazy slot: indexing a page with it panics, so a site that reads the
// arrays without closing the gap fails loudly in the corpus.
const gapPoison = math.MaxInt32

// leafEdit is a dirty leaf's edit state.
type leafEdit struct {
	gapAt, gapN int // gap mode: slots [gapAt, gapAt+gapN) are empty
	// short mode: the arrays' allocations and where each slice starts in its own.
	fk frontRoom[[]byte]
	fv frontRoom[storedRow]
	fw frontRoom[uint32]
	fl frontRoom[int32]
}

// gapSize is the number of empty slots in n's arrays.
func (n *pnode) gapSize() int {
	if n.edit == nil {
		return 0
	}
	return n.edit.gapN
}

// pi maps logical row i to its slot.
func (n *pnode) pi(i int) int {
	if e := n.edit; e != nil && e.gapN > 0 && i >= e.gapAt {
		return i + e.gapN
	}
	return i
}

// inGap reports whether slot p is empty.
func (n *pnode) inGap(p int) bool {
	e := n.edit
	return e != nil && e.gapN > 0 && p >= e.gapAt && p < e.gapAt+e.gapN
}

// rowCount is the number of rows of a decoded leaf.
func (n *pnode) rowCount() int { return len(n.vals) - n.gapSize() }

// gapMove moves a gap of size slots from at to to. The slots the rows left still hold copies of
// rows that live on in their new slots, so they keep nothing alive; they are emptied only when empty
// must be visible (fill, for the weights summed by trackWeight and the lazy poison).
func gapMove[T any](s []T, at, size, to int, fill bool, zero T) {
	if to < at {
		copy(s[to+size:at+size], s[to:at])
		if fill {
			for k := to; k < min(at, to+size); k++ {
				s[k] = zero
			}
		}
	} else if to > at {
		copy(s[at:to], s[at+size:to+size])
		if fill {
			for k := max(to, at+size); k < to+size; k++ {
				s[k] = zero
			}
		}
	}
}

// moveGap moves the gap to logical position to.
func (n *pnode) moveGap(to int) {
	e := n.edit
	if e == nil || e.gapN == 0 || e.gapAt == to {
		if e != nil && e.gapN == 0 {
			e.gapAt = to
		}
		return
	}
	if leafEditCount {
		d := to - e.gapAt
		if d < 0 {
			d = -d
		}
		leafEditStats.moved += int64(d)
	}
	gapMove(n.keys, e.gapAt, e.gapN, to, false, nil)
	gapMove(n.vals, e.gapAt, e.gapN, to, false, nil)
	gapMove(n.weights, e.gapAt, e.gapN, to, true, 0)
	if n.lazy != nil {
		gapMove(n.lazy, e.gapAt, e.gapN, to, true, gapPoison)
	}
	e.gapAt = to
}

// closeGap makes n's arrays exactly its rows again. Safe on any node: only a node its active
// generation owns has a gap.
func (n *pnode) closeGap() {
	e := n.edit
	if e == nil || e.gapN == 0 {
		return
	}
	if leafEditCount {
		leafEditStats.closes++
		leafEditStats.closeMoved += int64(len(n.keys) - e.gapAt - e.gapN)
	}
	rows := len(n.keys) - e.gapN
	n.moveGap(rows)
	n.keys = n.keys[:rows]
	n.vals = n.vals[:rows]
	n.weights = n.weights[:rows]
	if n.lazy != nil {
		n.lazy = n.lazy[:rows]
	}
	e.gapN = 0
}

func gapGrow[T any](s []T, at, size, rows int, zero T, poison bool) []T {
	out := make([]T, rows+size)
	copy(out, s[:at])
	copy(out[at+size:], s[at:rows])
	if poison {
		for k := at; k < at+size; k++ {
			out[k] = zero
		}
	}
	return out
}

// gapInsertAt makes room for a row at logical position i of a leaf whose arrays may hold a gap,
// returning the slot to fill.
func (n *pnode) gapInsertAt(i int, generation *mutationGeneration) int {
	e := n.edit
	if e == nil {
		e = &leafEdit{}
		n.edit = e
	}
	if e.gapN == 0 {
		rows := len(n.keys)
		size := max(16, rows) // doubling, as append grows a slice
		n.keys = gapGrow(n.keys, i, size, rows, nil, false)
		n.vals = gapGrow(n.vals, i, size, rows, nil, false)
		n.weights = gapGrow(n.weights, i, size, rows, 0, false)
		if n.lazy != nil {
			n.lazy = gapGrow(n.lazy, i, size, rows, gapPoison, true)
		}
		if leafEditCount {
			leafEditStats.moved += int64(rows)
		}
		e.gapAt, e.gapN = i, size
		generation.gapped = append(generation.gapped, n)
	} else {
		n.moveGap(i)
	}
	if leafEditCount {
		leafEditStats.edits++
	}
	e.gapAt++
	e.gapN--
	return i
}

// gapRemoveAt empties the slot of logical row i.
func (n *pnode) gapRemoveAt(i int, generation *mutationGeneration) {
	e := n.edit
	if e == nil {
		e = &leafEdit{}
		n.edit = e
	}
	if e.gapN == 0 {
		e.gapAt = i
		generation.gapped = append(generation.gapped, n)
	} else {
		n.moveGap(i)
	}
	if leafEditCount {
		leafEditStats.edits++
	}
	// Row i is the slot just after the gap; the gap takes it.
	p := i + e.gapN
	n.keys[p] = nil
	n.vals[p] = nil
	n.weights[p] = 0
	if n.lazy != nil {
		n.lazy[p] = gapPoison
	}
	e.gapN++
}

// closeGaps closes every gap generation's nodes hold, before the generation is frozen.
func (g *mutationGeneration) closeGaps() {
	for _, n := range g.gapped {
		n.closeGap()
	}
	g.gapped = nil
}

// --- short mode ------------------------------------------------------------------------------

// frontRoom remembers the allocation a slice was cut from and where it starts there, so the slice
// can grow at its front. A record that no longer describes the slice (the slice was replaced) is
// ignored and the slice adopted afresh.
type frontRoom[T any] struct {
	base []T
	off  int
}

func (f *frontRoom[T]) of(s []T) {
	if len(s) > 0 && f.off < len(f.base) && &f.base[f.off] == &s[0] {
		return
	}
	f.base, f.off = s[:cap(s)], 0
}

func shortInsert[T any](s []T, f *frontRoom[T], i int, x T, count bool) []T {
	n := len(s)
	if i >= n/2 || n == 0 {
		var z T
		s = append(s, z)
		copy(s[i+1:], s[i:])
		s[i] = x
		if leafEditCount && count {
			leafEditStats.moved += int64(n - i)
		}
		return s
	}
	f.of(s)
	if f.off == 0 {
		// No headroom: reallocate with room at both ends.
		room := max(8, n/4)
		base := make([]T, room+n, room+n+room)
		copy(base[room:], s)
		f.base, f.off = base, room
		s = base[room : room+n]
		if leafEditCount && count {
			leafEditStats.moved += int64(n)
		}
	}
	f.off--
	s = f.base[f.off : f.off+n+1]
	copy(s[:i], s[1:i+1])
	s[i] = x
	if leafEditCount && count {
		leafEditStats.moved += int64(i)
	}
	return s
}

func shortRemove[T any](s []T, f *frontRoom[T], i int, count bool) []T {
	var zero T
	n := len(s)
	if i >= n/2 {
		copy(s[i:], s[i+1:])
		s[n-1] = zero
		if leafEditCount && count {
			leafEditStats.moved += int64(n - 1 - i)
		}
		return s[:n-1]
	}
	f.of(s)
	copy(s[1:i+1], s[:i])
	s[0] = zero
	f.off++
	if leafEditCount && count {
		leafEditStats.moved += int64(i)
	}
	return s[1:]
}

func (n *pnode) shortEdit() *leafEdit {
	if n.edit == nil {
		n.edit = &leafEdit{}
	}
	if leafEditCount {
		leafEditStats.edits++
	}
	return n.edit
}

// --- the edits nodeInsert and nodeRemove make ---------------------------------------------------

// editInsert inserts a row at logical position i of a leaf its writer owns. A lazy leaf's lazy
// entry is -1 (the row is in vals).
func (n *pnode) editInsert(i int, key []byte, val storedRow, weight uint32, generation *mutationGeneration) {
	switch leafEditMode {
	case leafEditGap:
		p := n.gapInsertAt(i, generation)
		n.keys[p], n.vals[p], n.weights[p] = key, val, weight
		if n.lazy != nil {
			n.lazy[p] = -1
		}
	case leafEditShort:
		e := n.shortEdit()
		n.keys = shortInsert(n.keys, &e.fk, i, key, true)
		n.vals = shortInsert(n.vals, &e.fv, i, val, false)
		n.weights = shortInsert(n.weights, &e.fw, i, weight, false)
		if n.lazy != nil {
			n.lazy = shortInsert(n.lazy, &e.fl, i, -1, false)
		}
	default:
		if leafEditCount {
			leafEditStats.edits++
			leafEditStats.moved += int64(len(n.keys) - i)
		}
		n.keys = insertKeyAtMutable(n.keys, i, key)
		n.vals = insertValAtMutable(n.vals, i, val)
		n.weights = insertWeightAtMutable(n.weights, i, weight)
		if n.lazy != nil {
			n.lazy = append(n.lazy, 0)
			copy(n.lazy[i+1:], n.lazy[i:])
			n.lazy[i] = -1
		}
	}
}

// editRemove removes logical row i of a leaf its writer owns, returning its vals entry.
func (n *pnode) editRemove(i int, generation *mutationGeneration) storedRow {
	switch leafEditMode {
	case leafEditGap:
		removed := n.vals[n.pi(i)]
		n.gapRemoveAt(i, generation)
		return removed
	case leafEditShort:
		removed := n.vals[i]
		e := n.shortEdit()
		n.keys = shortRemove(n.keys, &e.fk, i, true)
		n.vals = shortRemove(n.vals, &e.fv, i, false)
		n.weights = shortRemove(n.weights, &e.fw, i, false)
		if n.lazy != nil {
			n.lazy = shortRemove(n.lazy, &e.fl, i, false)
		}
		return removed
	}
	if leafEditCount {
		leafEditStats.edits++
		leafEditStats.moved += int64(len(n.keys) - 1 - i)
	}
	if n.lazy != nil {
		copy(n.lazy[i:], n.lazy[i+1:])
		n.lazy = n.lazy[:len(n.lazy)-1]
	}
	var removed storedRow
	n.keys = removeKeyAtMutable(n.keys, i)
	n.vals, removed = removeValAtMutable(n.vals, i)
	n.weights = removeWeightAtMutable(n.weights, i)
	return removed
}
