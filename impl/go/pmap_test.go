package jed

import (
	"bytes"
	"encoding/binary"
	"reflect"
	"sort"
	"testing"
)

func pmKey(n uint64) []byte {
	b := make([]byte, 8)
	binary.BigEndian.PutUint64(b, n)
	return b
}

func pmRow(n int64) Row { return Row{IntValue(n)} }

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

func TestPMapInsertGetRemoveVsReference(t *testing.T) {
	var pm PMap
	ref := map[string]Row{}
	const n = 4000

	for _, k := range pmShuffled(n) {
		_, had := pm.Insert(pmKey(k), pmRow(int64(k)))
		_, refHad := ref[string(pmKey(k))]
		if had != refHad {
			t.Fatalf("insert 'had' mismatch at %d: %v vs %v", k, had, refHad)
		}
		ref[string(pmKey(k))] = pmRow(int64(k))
	}
	if pm.Len() != len(ref) {
		t.Fatalf("len %d != %d", pm.Len(), len(ref))
	}
	for k := uint64(0); k < n; k++ {
		got, _ := pm.Get(pmKey(k))
		if !reflect.DeepEqual(got, ref[string(pmKey(k))]) {
			t.Fatalf("get mismatch at %d", k)
		}
	}

	// Iteration is in ascending key order and matches the reference.
	keys, vals := pm.inorder()
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
	old, replaced := pm.Insert(pmKey(7), pmRow(777))
	if !replaced || !reflect.DeepEqual(old, pmRow(7)) {
		t.Fatalf("overwrite: old=%v replaced=%v", old, replaced)
	}
	ref[string(pmKey(7))] = pmRow(777)
	if pm.Len() != before {
		t.Fatal("overwrite changed len")
	}

	for _, k := range pmShuffled(n) {
		got, ok := pm.Remove(pmKey(k))
		want, wok := ref[string(pmKey(k))]
		delete(ref, string(pmKey(k)))
		if ok != wok || !reflect.DeepEqual(got, want) {
			t.Fatalf("remove mismatch at %d: got %v/%v want %v/%v", k, got, ok, want, wok)
		}
	}
	if pm.Len() != 0 {
		t.Fatalf("not empty after removing all: len %d", pm.Len())
	}
	if _, ok := pm.Remove(pmKey(123)); ok {
		t.Fatal("remove of absent key reported present")
	}
}

func TestPMapCloneIsIndependentSnapshot(t *testing.T) {
	var base PMap
	for k := uint64(0); k < 2000; k++ {
		base.Insert(pmKey(k), pmRow(int64(k)))
	}
	snap := base // an O(1) value-copy snapshot

	// Mutate a separate copy heavily; the snapshot must be untouched.
	other := base
	for k := uint64(0); k < 2000; k++ {
		other.Insert(pmKey(k), pmRow(-int64(k))) // overwrite every value
	}
	for k := uint64(2000); k < 3000; k++ {
		other.Insert(pmKey(k), pmRow(int64(k))) // grow
	}
	for k := uint64(0); k < 500; k++ {
		other.Remove(pmKey(k)) // shrink
	}

	if snap.Len() != 2000 {
		t.Fatalf("snapshot len changed: %d", snap.Len())
	}
	for k := uint64(0); k < 2000; k++ {
		got, _ := snap.Get(pmKey(k))
		if !reflect.DeepEqual(got, pmRow(int64(k))) {
			t.Fatalf("snapshot mutated at %d", k)
		}
	}
	if other.Len() != 2500 {
		t.Fatalf("other len %d", other.Len())
	}
	if _, ok := other.Get(pmKey(0)); ok {
		t.Fatal("other should have removed key 0")
	}
	if v, _ := other.Get(pmKey(1000)); !reflect.DeepEqual(v, pmRow(-1000)) {
		t.Fatal("other key 1000 wrong")
	}
	if v, _ := other.Get(pmKey(2500)); !reflect.DeepEqual(v, pmRow(2500)) {
		t.Fatal("other key 2500 wrong")
	}
}

func TestPMapEmptyAndSingle(t *testing.T) {
	var pm PMap
	if pm.Len() != 0 {
		t.Fatal("fresh map not empty")
	}
	if _, ok := pm.Get(pmKey(1)); ok {
		t.Fatal("get on empty")
	}
	if _, ok := pm.Remove(pmKey(1)); ok {
		t.Fatal("remove on empty")
	}
	if _, replaced := pm.Insert(pmKey(1), pmRow(1)); replaced {
		t.Fatal("first insert reported overwrite")
	}
	if v, ok := pm.Get(pmKey(1)); !ok || !reflect.DeepEqual(v, pmRow(1)) {
		t.Fatal("get after insert")
	}
	if v, ok := pm.Remove(pmKey(1)); !ok || !reflect.DeepEqual(v, pmRow(1)) {
		t.Fatal("remove returns the value")
	}
	if pm.Len() != 0 || pm.root != nil {
		t.Fatal("not empty after removing the only key")
	}
}
