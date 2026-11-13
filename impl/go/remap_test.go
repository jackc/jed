package jed

// PROTOTYPE (spec/design/deferred-interior.md): the remap list's bookkeeping invariant. These are
// internal-invariant tests, out of the conformance corpus's reach (CLAUDE.md §10), and they run with
// the prototype's switch off because they drive remapState directly.

import "testing"

// check is the invariant every operation must leave: fwd is injective on target pages and rev is its
// exact inverse. Two keys naming one page make rev depend on map iteration order in clone(), so a
// later replace updates the wrong key and the other entry outlives the page it names.
func (r *remapState) check(t *testing.T, what string) {
	t.Helper()
	if len(r.fwd) != len(r.rev) {
		t.Fatalf("%s: %d entries but %d reverse entries (fwd %v, rev %v)", what, len(r.fwd), len(r.rev), r.fwd, r.rev)
	}
	for k, v := range r.fwd {
		if got, ok := r.rev[v.page]; !ok || got != k {
			t.Fatalf("%s: entry %d -> %d has rev[%d] = %d (ok %v)", what, k, v.page, v.page, got, ok)
		}
	}
}

func TestRemapReplaceKeepsOneEntryPerTargetPage(t *testing.T) {
	// A child's entry survives its child leaving the tree: the branch that writes an interior fresh
	// drops entries for the children that remain, and a merged-away child is not among them. Its
	// target page is then orphaned, reused, and claimed by another child's move — which must retire
	// the stale entry rather than leave two keys naming the page.
	r := newRemapState()
	r.replace(271, pageRef{page: 42}) // the merged-away child's last move
	r.check(t, "one entry")
	r.replace(246, pageRef{page: 42}) // page 42 reused by another child
	r.check(t, "after the collision")
	if _, ok := r.fwd[271]; ok {
		t.Fatalf("the stale entry 271 -> 42 outlived page 42's reuse: %v", r.fwd)
	}
	if to, ok := r.lookup(246); !ok || to.page != 42 {
		t.Fatalf("lookup(246) = %d, %v; want 42", to.page, ok)
	}
	// The surviving entry must now move as itself, not as the stale key.
	r.replace(42, pageRef{page: 105})
	r.check(t, "after the move")
	if to, ok := r.lookup(246); !ok || to.page != 105 {
		t.Fatalf("lookup(246) = %d, %v after the move; want 105", to.page, ok)
	}
	if _, ok := r.lookup(271); ok {
		t.Fatalf("271 still resolves: %v", r.fwd)
	}
	if got := r.held(); len(got) != 1 || got[0] != 246 {
		t.Fatalf("held() = %v; want [246] — a retired key must stop holding its page off the free list", got)
	}
}

func TestRemapReplaceRetiresItsOwnPreviousTarget(t *testing.T) {
	// The same key's target moving twice must not leave the first target in rev.
	r := newRemapState()
	r.replace(10, pageRef{page: 20})
	r.replace(10, pageRef{page: 30}) // the key's own move, seen without replace(20, 30)
	r.check(t, "after the second move")
	if to, _ := r.lookup(10); to.page != 30 {
		t.Fatalf("lookup(10) = %d; want 30", to.page)
	}
}

func TestRemapCloneAndDropPreserveTheInvariant(t *testing.T) {
	r := newRemapState()
	r.replace(1, pageRef{page: 11})
	r.replace(2, pageRef{page: 12})
	r.replace(11, pageRef{page: 13}) // 1 -> 13, keyed by the on-disk id
	c := r.clone()
	c.check(t, "clone")
	if to, _ := c.lookup(1); to.page != 13 {
		t.Fatalf("clone lookup(1) = %d; want 13", to.page)
	}
	c.drop(13) // its parent was written fresh and names 13 directly
	c.check(t, "after drop")
	if _, ok := c.lookup(1); ok {
		t.Fatal("drop left the entry resolvable")
	}
	r.check(t, "original after the clone was changed")
}
