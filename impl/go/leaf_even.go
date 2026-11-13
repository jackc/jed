package jed

import "bytes"

// leafEvenRoom sets the room a sibling needs to take part in evening out: C/leafEvenRoom bytes free
// (spec/fileformat/format.md "Evening out with a sibling").
const leafEvenRoom = 8

// evenLeaves tries to even the overflowing leaf child i of n out with an adjacent sibling instead of
// splitting it (format.md "Evening out with a sibling"). sub is the leaf's own two-way split, key the
// key just written. The right sibling is tried, then the left one; an edit at the leaf's right edge
// tries the right one only, so an append to the tree's last leaf splits as it always did and reads
// no other leaf. A sibling takes part when its payload is at most C - C/leafEvenRoom; the two
// leaves' records are then split in two by the balanced rule, as the delete path's merge-overflow
// does (mergeAt). A candidate fails when the pair has no two-way split that fits — it would have to
// go multi-way, which the parent's key count cannot absorb here — or when the new separator would
// overflow n (mergeAt's second abandon rule).
//
// On ok, the returned halves and separator replace children j and j+1 and separator j of n, which
// keeps its key count. n is not modified here.
func evenLeaves(n *pnode, i int, sub insOut, key []byte, generation *mutationGeneration, src leafSource, cap pageCap, shape leafShape) (j int, out insOut, ok bool, err error) {
	rightEdge := bytes.Equal(sub.right.keys[len(sub.right.keys)-1], key)
	limit := cap.c - cap.c/leafEvenRoom
	for _, s := range [2]int{i + 1, i - 1} {
		if s < 0 || s >= len(n.children) || (s < i && rightEdge) {
			continue
		}
		sib, err := resolveChild(n.children[s], src)
		if err != nil {
			return 0, insOut{}, false, err
		}
		if !sib.payloadAtMost(limit, cap, shape) {
			continue
		}
		sk, sv, sw, err := sib.leafParts()
		if err != nil {
			return 0, insOut{}, false, err
		}
		total := len(sub.left.keys) + len(sub.right.keys) + len(sk)
		keys := make([][]byte, 0, total)
		vals := make([]storedRow, 0, total)
		weights := make([]uint32, 0, total)
		add := func(k [][]byte, v []storedRow, w []uint32) {
			keys, vals, weights = append(keys, k...), append(vals, v...), append(weights, w...)
		}
		if s < i {
			add(sk, sv, sw)
		}
		add(sub.left.keys, sub.left.vals, sub.left.weights)
		add(sub.right.keys, sub.right.vals, sub.right.weights)
		if s > i {
			add(sk, sv, sw)
		}
		// The pair overflows, because the leaf alone does: a whole result means a single record that
		// fits no leaf, and a multi-way one that no two halves fit. Either way the leaf splits.
		merged := buildLeaf(keys, vals, weights, cap, shape, -1, generation, nil, nil)
		j := min(i, s)
		if merged.whole != nil || len(merged.more) > 0 ||
			interiorPayload(cap, n.keys)-len(n.keys[j])+len(merged.sep) > cap.c {
			continue
		}
		return j, merged, true, nil
	}
	return 0, insOut{}, false, nil
}
