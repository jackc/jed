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
// realistic per-entry weight (8-byte key + a ~5-byte int value record), well under RECORD_MAX.
// These maps are in-memory (no paging), so every traversal passes a nil leaf source and never faults.
const (
	pmCap = 240
	pmW   = 15
)

func pmKey(n uint64) []byte {
	b := make([]byte, 8)
	binary.BigEndian.PutUint64(b, n)
	return b
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

// pmCheckInvariants asserts every node (except the root) fits a page and stays non-empty — the
// structural invariant the byte contract relies on (spec/fileformat/format.md).
func pmCheckInvariants(t *testing.T, pm *pMap) {
	t.Helper()
	var walk func(n *pnode, isRoot bool)
	walk = func(n *pnode, isRoot bool) {
		if n == nil {
			return
		}
		if len(n.keys) == 0 && !isRoot {
			t.Fatal("non-root node is empty")
		}
		if len(n.keys) != len(n.vals) || len(n.keys) != len(n.weights) {
			t.Fatal("keys/vals/weights length mismatch")
		}
		if !n.isLeaf() && len(n.children) != len(n.keys)+1 {
			t.Fatal("interior child count")
		}
		if n.payload() > pmCap {
			t.Fatalf("node payload %d exceeds cap %d", n.payload(), pmCap)
		}
		for _, c := range n.children {
			walk(c.node, false) // fully resident in-memory tree
		}
	}
	walk(pm.root, true)
}

func TestPMapInsertGetRemoveVsReference(t *testing.T) {
	var pm pMap
	ref := map[string]storedRow{}
	const n = 4000

	for _, k := range pmShuffled(n) {
		_, had, _ := pm.Insert(pmKey(k), pmRow(int64(k)), pmW, pmCap, nil)
		_, refHad := ref[string(pmKey(k))]
		if had != refHad {
			t.Fatalf("insert 'had' mismatch at %d: %v vs %v", k, had, refHad)
		}
		ref[string(pmKey(k))] = pmRow(int64(k))
	}
	if pm.Len() != len(ref) {
		t.Fatalf("len %d != %d", pm.Len(), len(ref))
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
	before := pm.Len()
	old, replaced, _ := pm.Insert(pmKey(7), pmRow(777), pmW, pmCap, nil)
	if !replaced || !reflect.DeepEqual(old, pmRow(7)) {
		t.Fatalf("overwrite: old=%v replaced=%v", old, replaced)
	}
	ref[string(pmKey(7))] = pmRow(777)
	if pm.Len() != before {
		t.Fatal("overwrite changed len")
	}

	// Interleave removes with invariant checks so merge-then-split is exercised mid-stream.
	for step, k := range pmShuffled(n) {
		got, ok, _ := pm.Remove(pmKey(k), pmCap, nil)
		want, wok := ref[string(pmKey(k))]
		delete(ref, string(pmKey(k)))
		if ok != wok || !reflect.DeepEqual(got, want) {
			t.Fatalf("remove mismatch at %d: got %v/%v want %v/%v", k, got, ok, want, wok)
		}
		if step%257 == 0 {
			pmCheckInvariants(t, &pm)
		}
	}
	if pm.Len() != 0 {
		t.Fatalf("not empty after removing all: len %d", pm.Len())
	}
	if _, ok, _ := pm.Remove(pmKey(123), pmCap, nil); ok {
		t.Fatal("remove of absent key reported present")
	}
}

func TestPMapCloneIsIndependentSnapshot(t *testing.T) {
	var base pMap
	for k := uint64(0); k < 2000; k++ {
		base.Insert(pmKey(k), pmRow(int64(k)), pmW, pmCap, nil)
	}
	snap := base // an O(1) value-copy snapshot

	// Mutate a separate copy heavily; the snapshot must be untouched.
	other := base
	for k := uint64(0); k < 2000; k++ {
		other.Insert(pmKey(k), pmRow(-int64(k)), pmW, pmCap, nil) // overwrite every value
	}
	for k := uint64(2000); k < 3000; k++ {
		other.Insert(pmKey(k), pmRow(int64(k)), pmW, pmCap, nil) // grow
	}
	for k := uint64(0); k < 500; k++ {
		other.Remove(pmKey(k), pmCap, nil) // shrink
	}

	if snap.Len() != 2000 {
		t.Fatalf("snapshot len changed: %d", snap.Len())
	}
	for k := uint64(0); k < 2000; k++ {
		got, _, _ := snap.Get(pmKey(k), nil)
		if !reflect.DeepEqual(got, pmRow(int64(k))) {
			t.Fatalf("snapshot mutated at %d", k)
		}
	}
	pmCheckInvariants(t, &snap)
	if other.Len() != 2500 {
		t.Fatalf("other len %d", other.Len())
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

// Wide values (near RECORD_MAX) force tiny fan-out — the stress case for the split point and the
// non-empty-halves guarantee. With weight 110 (≤ 114 cap) a node holds ~2 entries.
func TestPMapWideValuesKeepNodesValid(t *testing.T) {
	var pm pMap
	ref := map[string]bool{}
	for _, k := range pmShuffled(300) {
		pm.Insert(pmKey(k), pmRow(int64(k)), 110, pmCap, nil)
		ref[string(pmKey(k))] = true
		pmCheckInvariants(t, &pm)
	}
	for _, k := range pmShuffled(300) {
		pm.Remove(pmKey(k), pmCap, nil)
		pmCheckInvariants(t, &pm)
	}
	if pm.Len() != 0 {
		t.Fatalf("not empty: %d", pm.Len())
	}
}

func TestPMapEmptyAndSingle(t *testing.T) {
	var pm pMap
	if pm.Len() != 0 {
		t.Fatal("fresh map not empty")
	}
	if _, ok, _ := pm.Get(pmKey(1), nil); ok {
		t.Fatal("get on empty")
	}
	if _, ok, _ := pm.Remove(pmKey(1), pmCap, nil); ok {
		t.Fatal("remove on empty")
	}
	if _, replaced, _ := pm.Insert(pmKey(1), pmRow(1), pmW, pmCap, nil); replaced {
		t.Fatal("first insert reported overwrite")
	}
	if v, ok, _ := pm.Get(pmKey(1), nil); !ok || !reflect.DeepEqual(v, pmRow(1)) {
		t.Fatal("get after insert")
	}
	if v, ok, _ := pm.Remove(pmKey(1), pmCap, nil); !ok || !reflect.DeepEqual(v, pmRow(1)) {
		t.Fatal("remove returns the value")
	}
	if pm.Len() != 0 || pm.root != nil {
		t.Fatal("not empty after removing the only key")
	}
}

// TestPMapReverseScanIsForwardReversed checks scanRangeRev yields the EXACT reverse of scanRange's
// row sequence over a MULTI-LEVEL tree — the interior-node interleaving (separators between
// children) and the asymmetric inclusive-lo edge that single-leaf conformance tables (the
// DESC-LIMIT corpus cases) cannot exercise. 200 entries at pmCap build several levels.
func TestPMapReverseScanIsForwardReversed(t *testing.T) {
	var pm pMap
	for n := uint64(0); n < 200; n++ {
		pm.Insert(pmKey(n), pmRow(int64(n)), pmW, pmCap, nil)
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
	var pm pMap
	for n := uint64(0); n < 200; n++ {
		pm.Insert(pmKey(n), pmRow(int64(n)), pmW, pmCap, nil)
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
