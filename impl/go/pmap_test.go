package jed

import (
	"bytes"
	"encoding/binary"
	"reflect"
	"sort"
	"testing"
)

// A small page cap so a few-thousand-entry map is several levels deep — exercises split,
// merge-then-split, root growth and collapse (the in-RAM analog of page_size 256). pmW is a
// realistic per-entry weight (8-byte key + an 8-byte i64 slot = 16 bytes, so a 240-byte leaf holds
// ~12 entries before splitting, well under RECORD_MAX). pmShape is the leaf column-class shape —
// pmRow has one fixed-width value column, so the v24 leaf overhead scales with {fixed: 1, var: 0}
// (format.md "Leaf node"). These maps are in-memory (no paging), so every traversal passes a nil
// leaf source and never faults.
const (
	pmCap = 240
	pmW   = 16
)

var pmShape = leafShape{fixed: 1}

func pmKey(n uint64) []byte {
	b := make([]byte, 8)
	binary.BigEndian.PutUint64(b, n)
	return b
}

// pmLen returns m's exact row count, failing if the map does not know it. An in-memory map (built
// from empty by Insert) always knows its count; table skeletons restore it from v28 catalog data.
func pmLen(t *testing.T, m *pMap) int {
	t.Helper()
	n, known := m.Count()
	if !known {
		t.Fatal("expected a known row count on an in-memory map")
	}
	return int(n)
}

func pmRow(n int64) storedRow { return storedRow{IntValue(n)} }

// pmShuffled is a deterministic permutation of 0..n (LCG-driven) — no RNG / wall-clock, so the
// test is reproducible (CLAUDE.md §10).
func pmShuffled(n uint64) []uint64 {
	v := make([]uint64, n)
	for i := range v {
		v[i] = uint64(i)
	}
	state := uint64(0x9e3779b97f4a7c15)
	for i := len(v) - 1; i >= 1; i-- {
		state = state*6364136223846793005 + 1442695040888963407
		j := int((state >> 33) % uint64(i+1))
		v[i], v[j] = v[j], v[i]
	}
	return v
}

// pmCheckInvariants asserts the structural invariants the byte contract relies on (format.md
// "Fan-out"): every node fits a page; every leaf is non-empty; an interior node has N+1 children
// (N ≥ 0 only in the degenerate near-cap-separator case — these small-key tests never produce it,
// so N ≥ 1 is asserted); records (vals/weights) live only in leaves; all leaves at the same depth;
// and every key in a subtree respects its bounding separators (lo ≤ key < hi).
func pmCheckInvariants(t *testing.T, pm *pMap) {
	t.Helper()
	var walk func(n *pnode, isRoot bool, lo, hi []byte) int
	walk = func(n *pnode, isRoot bool, lo, hi []byte) int {
		if n.isLeaf() {
			if len(n.keys) == 0 && !isRoot {
				t.Fatal("non-root leaf is empty")
			}
			if n.packed == nil && len(n.keys) != len(n.vals) {
				t.Fatal("keys/vals length mismatch")
			}
			if len(n.keys) != len(n.weights) {
				t.Fatal("keys/weights length mismatch")
			}
		} else {
			if len(n.keys) == 0 && !isRoot {
				t.Fatal("0-key interior unexpected")
			}
			if len(n.vals) != 0 || len(n.weights) != 0 {
				t.Fatal("interior node carries records")
			}
			if len(n.children) != len(n.keys)+1 {
				t.Fatal("interior child count")
			}
		}
		for i := 1; i < len(n.keys); i++ {
			if bytes.Compare(n.keys[i-1], n.keys[i]) >= 0 {
				t.Fatal("keys out of order")
			}
		}
		// Subtree keys respect the bounding separators: lo ≤ key < hi (lo inclusive because a
		// separator equals the right subtree's first key at split time).
		for _, k := range n.keys {
			if lo != nil && bytes.Compare(k, lo) < 0 {
				t.Fatal("key below its subtree's low separator")
			}
			if hi != nil && bytes.Compare(k, hi) >= 0 {
				t.Fatal("key at/above its subtree's high separator")
			}
		}
		if n.payload(pmShape) > pmCap {
			t.Fatalf("node payload %d exceeds cap %d", n.payload(pmShape), pmCap)
		}
		if n.isLeaf() {
			return 1
		}
		depth := -1
		for i, c := range n.children {
			clo, chi := lo, hi
			if i > 0 {
				clo = n.keys[i-1]
			}
			if i < len(n.keys) {
				chi = n.keys[i]
			}
			d := walk(c.node, false, clo, chi) // fully resident in-memory tree
			if depth == -1 {
				depth = d
			} else if depth != d {
				t.Fatal("leaves at unequal depth")
			}
		}
		return depth + 1
	}
	if pm.root != nil {
		walk(pm.root, true, nil, nil)
	}
}

func TestPMapInsertGetRemoveVsReference(t *testing.T) {
	t.Parallel()
	var pm pMap
	ref := map[string]storedRow{}
	const n = 4000

	for _, k := range pmShuffled(n) {
		_, had, _ := pm.Insert(pmKey(k), pmRow(int64(k)), pmW, pmCap, pmShape, nil)
		_, refHad := ref[string(pmKey(k))]
		if had != refHad {
			t.Fatalf("insert 'had' mismatch at %d: %v vs %v", k, had, refHad)
		}
		ref[string(pmKey(k))] = pmRow(int64(k))
	}
	if pmLen(t, &pm) != len(ref) {
		t.Fatalf("len %d != %d", pmLen(t, &pm), len(ref))
	}
	pmCheckInvariants(t, &pm)
	for k := uint64(0); k < n; k++ {
		got, _, _ := pm.Get(pmKey(k), nil)
		if !reflect.DeepEqual(got, ref[string(pmKey(k))]) {
			t.Fatalf("get mismatch at %d", k)
		}
	}

	// Iteration is in ascending key order and matches the reference.
	keys, vals, _ := pm.inorder(nil)
	if !sort.SliceIsSorted(keys, func(a, b int) bool { return bytes.Compare(keys[a], keys[b]) < 0 }) {
		t.Fatal("iteration not in key order")
	}
	if len(keys) != len(ref) {
		t.Fatalf("inorder len %d != %d", len(keys), len(ref))
	}
	for i := range keys {
		if !reflect.DeepEqual(vals[i], ref[string(keys[i])]) {
			t.Fatalf("inorder value mismatch at %d", i)
		}
	}

	// Overwrite returns the old value and does not change len (kept in sync with the reference).
	before := pmLen(t, &pm)
	old, replaced, _ := pm.Insert(pmKey(7), pmRow(777), pmW, pmCap, pmShape, nil)
	if !replaced || !reflect.DeepEqual(old, pmRow(7)) {
		t.Fatalf("overwrite: old=%v replaced=%v", old, replaced)
	}
	ref[string(pmKey(7))] = pmRow(777)
	if pmLen(t, &pm) != before {
		t.Fatal("overwrite changed len")
	}

	// Interleave removes with invariant checks so merge-then-split is exercised mid-stream.
	for step, k := range pmShuffled(n) {
		got, ok, _ := pm.Remove(pmKey(k), pmCap, pmShape, nil)
		want, wok := ref[string(pmKey(k))]
		delete(ref, string(pmKey(k)))
		if ok != wok || !reflect.DeepEqual(got, want) {
			t.Fatalf("remove mismatch at %d: got %v/%v want %v/%v", k, got, ok, want, wok)
		}
		if step%257 == 0 {
			pmCheckInvariants(t, &pm)
		}
	}
	if pmLen(t, &pm) != 0 {
		t.Fatalf("not empty after removing all: len %d", pmLen(t, &pm))
	}
	if _, ok, _ := pm.Remove(pmKey(123), pmCap, pmShape, nil); ok {
		t.Fatal("remove of absent key reported present")
	}
}

func TestPMapCloneIsIndependentSnapshot(t *testing.T) {
	t.Parallel()
	var base pMap
	for k := uint64(0); k < 2000; k++ {
		base.Insert(pmKey(k), pmRow(int64(k)), pmW, pmCap, pmShape, nil)
	}
	snap := base.clone()
	// Capture the complete encoded-key/value sequence before the working copy starts changing.
	// Equality after the churn below is the byte/value alias guard for transient mutation work: an
	// unsafe in-place edit of a shared leaf must make this test fail.
	snapKeysBefore, snapValsBefore, err := snap.inorder(nil)
	if err != nil {
		t.Fatal(err)
	}

	// Mutate a separate copy heavily; the snapshot must be untouched.
	other := base.clone()
	for k := uint64(0); k < 2000; k++ {
		other.Insert(pmKey(k), pmRow(-int64(k)), pmW, pmCap, pmShape, nil) // overwrite every value
	}
	for k := uint64(2000); k < 3000; k++ {
		other.Insert(pmKey(k), pmRow(int64(k)), pmW, pmCap, pmShape, nil) // grow
	}
	for k := uint64(0); k < 500; k++ {
		other.Remove(pmKey(k), pmCap, pmShape, nil) // shrink
	}

	if pmLen(t, &snap) != 2000 {
		t.Fatalf("snapshot len changed: %d", pmLen(t, &snap))
	}
	for k := uint64(0); k < 2000; k++ {
		got, _, _ := snap.Get(pmKey(k), nil)
		if !reflect.DeepEqual(got, pmRow(int64(k))) {
			t.Fatalf("snapshot mutated at %d", k)
		}
	}
	snapKeysAfter, snapValsAfter, err := snap.inorder(nil)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(snapKeysAfter, snapKeysBefore) || !reflect.DeepEqual(snapValsAfter, snapValsBefore) {
		t.Fatal("pinned key/value bytes changed")
	}
	pmCheckInvariants(t, &snap)
	if pmLen(t, &other) != 2500 {
		t.Fatalf("other len %d", pmLen(t, &other))
	}
	if _, ok, _ := other.Get(pmKey(0), nil); ok {
		t.Fatal("other should have removed key 0")
	}
	if v, _, _ := other.Get(pmKey(1000), nil); !reflect.DeepEqual(v, pmRow(-1000)) {
		t.Fatal("other key 1000 wrong")
	}
	if v, _, _ := other.Get(pmKey(2500), nil); !reflect.DeepEqual(v, pmRow(2500)) {
		t.Fatal("other key 2500 wrong")
	}
	pmCheckInvariants(t, &other)
}

func TestPMapInsertReusesOnlyCurrentGenerationDirtyPaths(t *testing.T) {
	t.Parallel()
	path := func(pm *pMap, key []byte) []*pnode {
		n := pm.root
		if n == nil {
			t.Fatal("nonempty map has nil root")
		}
		var out []*pnode
		for {
			out = append(out, n)
			if n.isLeaf() {
				return out
			}
			n = n.children[n.childSlot(key)].node
			if n == nil {
				t.Fatal("test map unexpectedly contains an on-disk child")
			}
		}
	}

	var pm pMap
	for k := uint64(0); k < 2000; k++ {
		if _, _, err := pm.Insert(pmKey(k), pmRow(int64(k)), pmW, pmCap, pmShape, nil); err != nil {
			t.Fatal(err)
		}
	}
	if pm.height() <= 2 {
		t.Fatal("test needs a multi-level dirty path")
	}

	// Repeated equal-weight overwrites under one unaliased generation retain every node address.
	before := path(&pm, pmKey(777))
	if _, _, err := pm.Insert(pmKey(777), pmRow(-777), pmW, pmCap, pmShape, nil); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(path(&pm, pmKey(777)), before) {
		t.Fatal("current-generation dirty path was rebuilt")
	}

	// Snapshot clone invalidates the generation. The next write path-copies while the pin retains
	// its old node identities and value; the following unaliased write may reuse the new path.
	pin := pm.clone()
	shared := path(&pm, pmKey(777))
	if _, _, err := pm.Insert(pmKey(777), pmRow(-778), pmW, pmCap, pmShape, nil); err != nil {
		t.Fatal(err)
	}
	if reflect.DeepEqual(path(&pm, pmKey(777)), shared) {
		t.Fatal("snapshot-aliased path was mutated in place")
	}
	if got, _, _ := pin.Get(pmKey(777), nil); !reflect.DeepEqual(got, pmRow(-777)) {
		t.Fatalf("snapshot value changed: %v", got)
	}
	owned := path(&pm, pmKey(777))
	if _, _, err := pm.Insert(pmKey(777), pmRow(-779), pmW, pmCap, pmShape, nil); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(path(&pm, pmKey(777)), owned) {
		t.Fatal("fresh generation did not retain its dirty path")
	}

	// A pull cursor is another immutable root alias. It must keep the value from cursor-open time
	// while a later write copies the touched path.
	b := keyBound{lo: pmKey(777), loInc: true, hi: pmKey(777), hiInc: true}
	c := pm.rangeCursor(b, nil, false)
	cursorPath := path(&pm, pmKey(777))
	if _, _, err := pm.Insert(pmKey(777), pmRow(-780), pmW, pmCap, pmShape, nil); err != nil {
		t.Fatal(err)
	}
	if reflect.DeepEqual(path(&pm, pmKey(777)), cursorPath) {
		t.Fatal("cursor-aliased path was mutated in place")
	}
	_, got, ok, err := c.next()
	if err != nil || !ok || !reflect.DeepEqual(got, pmRow(-779)) {
		t.Fatalf("pinned cursor got %v/%v/%v, want -779", got, ok, err)
	}

	// Clean page state is an independent guard even when the generation still matches.
	var clean pMap
	clean.Insert(pmKey(1), pmRow(1), pmW, pmCap, pmShape, nil)
	cleanRoot := clean.root
	cleanRoot.page = 9
	clean.Insert(pmKey(1), pmRow(-1), pmW, pmCap, pmShape, nil)
	if clean.root == cleanRoot {
		t.Fatal("clean node was mutated in place")
	}
	pmCheckInvariants(t, &pm)
	pmCheckInvariants(t, &clean)
}

// Wide values (near RECORD_MAX) force tiny fan-out — the stress case for the split point and the
// non-empty-halves guarantee. With weight 100 (≤ RECORD_MAX(240,1) = (240−28)/2 = 106) a
// two-record leaf (2·100 + leafOverhead(2, {fixed:1}) = 218 ≤ 240) fits but a third record
// overflows, so a node holds ~2 entries.
func TestPMapWideValuesKeepNodesValid(t *testing.T) {
	t.Parallel()
	var pm pMap
	for _, k := range pmShuffled(300) {
		pm.Insert(pmKey(k), pmRow(int64(k)), 100, pmCap, pmShape, nil)
		pmCheckInvariants(t, &pm)
	}
	for _, k := range pmShuffled(300) {
		pm.Remove(pmKey(k), pmCap, pmShape, nil)
		pmCheckInvariants(t, &pm)
	}
	if pmLen(t, &pm) != 0 {
		t.Fatalf("not empty: %d", pmLen(t, &pm))
	}
}

// Near-cap KEYS (the max-size-separator case, format.md "Interior node"): separators are key
// copies, so two of them overflow an interior node, forcing the pinned degenerate N = 2 → m = 1
// split and legal 0-key interiors. The map must stay correct through inserts, scans, and removes
// (a looser invariant check — 0-key interiors are legal here).
func TestPMapNearCapKeysDegenerateInterior(t *testing.T) {
	t.Parallel()
	// Index-tree shape: zero value columns, record = key alone. RECORD_MAX(0) = (240−12)/2 = 114;
	// keys of 110 bytes keep records under the cap while two separators (2·110 + 20) overflow an
	// interior.
	shape := leafShape{}
	bigKey := func(n uint64) []byte {
		k := make([]byte, 110)
		for i := range k {
			k[i] = 0xAB
		}
		binary.BigEndian.PutUint64(k[:8], n)
		return k
	}
	var pm pMap
	ref := map[string]bool{}
	for _, k := range pmShuffled(60) {
		pm.Insert(bigKey(k), storedRow{}, 110, pmCap, shape, nil)
		ref[string(bigKey(k))] = true
	}
	if pmLen(t, &pm) != len(ref) {
		t.Fatalf("len %d != %d", pmLen(t, &pm), len(ref))
	}
	// Structure: fits + routing correctness (0-key interiors allowed).
	var walk func(n *pnode)
	walk = func(n *pnode) {
		if n.payload(shape) > pmCap {
			t.Fatalf("node payload %d overflows its page", n.payload(shape))
		}
		if !n.isLeaf() {
			if len(n.children) != len(n.keys)+1 {
				t.Fatal("interior child count")
			}
			for _, c := range n.children {
				walk(c.node)
			}
		}
	}
	walk(pm.root)
	keys, _, err := pm.inorder(nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(keys) != 60 {
		t.Fatalf("inorder len %d != 60", len(keys))
	}
	if !sort.SliceIsSorted(keys, func(a, b int) bool { return bytes.Compare(keys[a], keys[b]) < 0 }) {
		t.Fatal("iteration not in key order")
	}
	for k := uint64(0); k < 60; k++ {
		if _, ok, _ := pm.Get(bigKey(k), nil); !ok {
			t.Fatalf("get miss at %d", k)
		}
	}
	for _, k := range pmShuffled(60) {
		if _, ok, _ := pm.Remove(bigKey(k), pmCap, shape, nil); !ok {
			t.Fatalf("remove miss at %d", k)
		}
	}
	if pmLen(t, &pm) != 0 {
		t.Fatalf("not empty: %d", pmLen(t, &pm))
	}
}

func TestPMapDirectPointGetCountsOneDescentAndReconstruction(t *testing.T) {
	t.Parallel()
	var pm pMap
	for k := uint64(0); k < 2000; k++ {
		pm.Insert(pmKey(k), pmRow(int64(k)), pmW, pmCap, pmShape, nil)
	}
	if pm.height() <= 1 {
		t.Fatal("test needs a multi-level tree")
	}
	hit, found, hitNodes, hitRows, err := pm.GetCounted(pmKey(777), nil)
	if err != nil || !found || !reflect.DeepEqual(hit, pmRow(777)) {
		t.Fatalf("hit = %v/%v, err=%v", hit, found, err)
	}
	if hitNodes != pm.height() || hitRows != 1 {
		t.Fatalf("hit counts nodes=%d rows=%d, want height=%d rows=1", hitNodes, hitRows, pm.height())
	}
	miss, found, missNodes, missRows, err := pm.GetCounted(pmKey(3000), nil)
	if err != nil || found || miss != nil {
		t.Fatalf("miss = %v/%v, err=%v", miss, found, err)
	}
	if missNodes != pm.height() || missRows != 0 {
		t.Fatalf("miss counts nodes=%d rows=%d, want height=%d rows=0", missNodes, missRows, pm.height())
	}
}

// The bounded scan yields exactly the in-bound rows, in order, and the nodes counted during the
// one windowed walk match overlapNodeCount's second counting descent (the page_read contract,
// cost.md §3).
func TestPMapBoundedScanCountsAgree(t *testing.T) {
	t.Parallel()
	var pm pMap
	for _, k := range pmShuffled(2000) {
		pm.Insert(pmKey(k), pmRow(int64(k)), pmW, pmCap, pmShape, nil)
	}
	b := keyBound{lo: pmKey(500), loInc: true, hi: pmKey(1500), hiInc: false}
	keys, _, nodes, err := pm.rangeEntriesCounted(b, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(keys) != 1000 {
		t.Fatalf("entries %d != 1000", len(keys))
	}
	if !bytes.Equal(keys[0], pmKey(500)) || !bytes.Equal(keys[999], pmKey(1499)) {
		t.Fatal("bounded scan endpoints wrong")
	}
	if want := pm.overlapNodeCount(b); nodes != want {
		t.Fatalf("counted nodes %d != overlapNodeCount %d", nodes, want)
	}

	// Exclusive lo / inclusive hi.
	b2 := keyBound{lo: pmKey(500), loInc: false, hi: pmKey(1500), hiInc: true}
	keys2, _, err := pm.rangeEntries(b2, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(keys2) != 1000 || !bytes.Equal(keys2[0], pmKey(501)) || !bytes.Equal(keys2[999], pmKey(1500)) {
		t.Fatal("exclusive-lo/inclusive-hi window wrong")
	}
}

func TestPMapEmptyAndSingle(t *testing.T) {
	t.Parallel()
	var pm pMap
	if pmLen(t, &pm) != 0 {
		t.Fatal("fresh map not empty")
	}
	if _, ok, _ := pm.Get(pmKey(1), nil); ok {
		t.Fatal("get on empty")
	}
	if _, ok, _ := pm.Remove(pmKey(1), pmCap, pmShape, nil); ok {
		t.Fatal("remove on empty")
	}
	if _, replaced, _ := pm.Insert(pmKey(1), pmRow(1), pmW, pmCap, pmShape, nil); replaced {
		t.Fatal("first insert reported overwrite")
	}
	if v, ok, _ := pm.Get(pmKey(1), nil); !ok || !reflect.DeepEqual(v, pmRow(1)) {
		t.Fatal("get after insert")
	}
	if v, ok, _ := pm.Remove(pmKey(1), pmCap, pmShape, nil); !ok || !reflect.DeepEqual(v, pmRow(1)) {
		t.Fatal("remove returns the value")
	}
	if pmLen(t, &pm) != 0 || pm.root != nil {
		t.Fatal("not empty after removing the only key")
	}
}

// TestPMapReverseScanIsForwardReversed checks scanRangeRev yields the EXACT reverse of scanRange's
// row sequence over a MULTI-LEVEL tree — the interior child windowing (descend high→low, v24) that
// single-leaf conformance tables (the DESC-LIMIT corpus cases) cannot exercise. 200 entries at
// pmCap build several levels.
func TestPMapReverseScanIsForwardReversed(t *testing.T) {
	t.Parallel()
	var pm pMap
	for n := uint64(0); n < 200; n++ {
		pm.Insert(pmKey(n), pmRow(int64(n)), pmW, pmCap, pmShape, nil)
	}
	if pm.nodeCount() <= 2 {
		t.Fatal("test needs a multi-level tree")
	}
	decode := func(k []byte) uint64 { return binary.BigEndian.Uint64(k) }
	collect := func(b keyBound, rev bool) []uint64 {
		var out []uint64
		visit := func(k []byte, _ storedRow) (bool, error) { out = append(out, decode(k)); return true, nil }
		if rev {
			pm.scanRangeRev(b, nil, visit)
		} else {
			pm.scanRange(b, nil, visit)
		}
		return out
	}
	bounds := []keyBound{
		unboundedBound(),
		{lo: pmKey(50), loInc: true, hi: pmKey(150), hiInc: true},
		{lo: pmKey(50), loInc: false, hi: pmKey(150), hiInc: false},
		{lo: pmKey(195), loInc: true, hi: nil, hiInc: false},
		{lo: pmKey(100), loInc: true, hi: pmKey(100), hiInc: true},
		{lo: pmKey(73), loInc: true, hi: pmKey(181), hiInc: false},
	}
	for i, b := range bounds {
		fwd := collect(b, false)
		rev := collect(b, true)
		for j, k := 0, len(fwd)-1; j < len(fwd); j, k = j+1, k-1 {
			if rev[j] != fwd[k] {
				t.Fatalf("reverse scan must equal forward-reversed for bound #%d", i)
			}
		}
	}
	// The reverse short-circuit stops from the HIGH end: stopping after 3 visits yields the 3
	// largest keys descending, faulting no further.
	var got []uint64
	n := 0
	pm.scanRangeRev(unboundedBound(), nil, func(k []byte, _ storedRow) (bool, error) {
		got = append(got, decode(k))
		n++
		return n < 3, nil
	})
	if !reflect.DeepEqual(got, []uint64{199, 198, 197}) {
		t.Fatalf("reverse short-circuit got %v, want [199 198 197]", got)
	}
}

// TestPMapRangeCursorMatchesScanRange checks the S2 pull cursor (rangeCursor) yields the EXACT same
// (key, row) sequence as the push scanRange / scanRangeRev over a MULTI-LEVEL tree — the contract the
// streaming pipeline (S3) rests on. Internal machinery, not corpus-expressible (CLAUDE.md §10), so it
// is unit-tested per core against the existing push scan.
func TestPMapRangeCursorMatchesScanRange(t *testing.T) {
	t.Parallel()
	var pm pMap
	for n := uint64(0); n < 200; n++ {
		pm.Insert(pmKey(n), pmRow(int64(n)), pmW, pmCap, pmShape, nil)
	}
	if pm.nodeCount() <= 2 {
		t.Fatal("test needs a multi-level tree")
	}
	decode := func(k []byte) uint64 { return binary.BigEndian.Uint64(k) }
	val := func(r storedRow) int64 {
		if r[0].Kind != ValInt {
			t.Fatalf("unexpected row value %#v", r[0])
		}
		return r[0].Int
	}
	type pair struct {
		k uint64
		v int64
	}
	// Collect the push scan's sequence.
	pushed := func(b keyBound, rev bool) []pair {
		var out []pair
		visit := func(k []byte, r storedRow) (bool, error) {
			out = append(out, pair{decode(k), val(r)})
			return true, nil
		}
		if rev {
			pm.scanRangeRev(b, nil, visit)
		} else {
			pm.scanRange(b, nil, visit)
		}
		return out
	}
	// Drain the pull cursor into the same shape.
	pulled := func(b keyBound, rev bool) []pair {
		c := pm.rangeCursor(b, nil, rev)
		var out []pair
		for {
			k, r, ok, err := c.next()
			if err != nil {
				t.Fatalf("cursor next: %v", err)
			}
			if !ok {
				break
			}
			out = append(out, pair{decode(k), val(r)})
		}
		return out
	}
	bounds := []keyBound{
		unboundedBound(),
		{lo: pmKey(50), loInc: true, hi: pmKey(150), hiInc: true},
		{lo: pmKey(50), loInc: false, hi: pmKey(150), hiInc: false},
		{lo: pmKey(195), loInc: true, hi: nil, hiInc: false},
		{lo: pmKey(100), loInc: true, hi: pmKey(100), hiInc: true},
		{lo: pmKey(73), loInc: true, hi: pmKey(181), hiInc: false},
		{lo: pmKey(150), loInc: true, hi: pmKey(50), hiInc: true}, // empty (lo > hi)
	}
	for i, b := range bounds {
		for _, rev := range []bool{false, true} {
			push := pushed(b, rev)
			pull := pulled(b, rev)
			if !reflect.DeepEqual(push, pull) {
				t.Fatalf("cursor must match scanRange for bound #%d rev=%v:\n push=%v\n pull=%v", i, rev, push, pull)
			}
		}
	}
	// Early abandonment: pulling only 3 rows then dropping the cursor yields the first 3 of the full
	// sequence (forward and reverse), proving the pull short-circuit (the streaming win).
	for _, rev := range []bool{false, true} {
		full := pushed(unboundedBound(), rev)
		c := pm.rangeCursor(unboundedBound(), nil, rev)
		var got []pair
		for n := 0; n < 3; n++ {
			k, r, ok, err := c.next()
			if err != nil || !ok {
				t.Fatalf("cursor next %d: ok=%v err=%v", n, ok, err)
			}
			got = append(got, pair{decode(k), val(r)})
		}
		if !reflect.DeepEqual(got, full[:3]) {
			t.Fatalf("early-abandoned cursor must be the prefix (rev=%v): got %v want %v", rev, got, full[:3])
		}
	}
}
