package jed

import "testing"

// counting returns a loader that records how many times it actually read a page (a cache miss),
// returning a sentinel node carrying the page id.
func counting(loads *int, page uint32) func() (*pnode, error) {
	return func() (*pnode, error) {
		*loads++
		return &pnode{page: page}, nil
	}
}

func TestBufferPoolHitReturnsCachedWithoutReloading(t *testing.T) {
	t.Parallel()
	pool := newBufferPool(4)
	loads := 0
	if n, _ := pool.getOrLoad(7, counting(&loads, 70)); n.page != 70 {
		t.Fatalf("first load: page %d", n.page)
	}
	if n, _ := pool.getOrLoad(7, counting(&loads, 70)); n.page != 70 {
		t.Fatalf("second load: page %d", n.page)
	}
	if loads != 1 {
		t.Fatalf("second access should be a cache hit; loads = %d", loads)
	}
	if pool.resident() != 1 {
		t.Fatalf("resident = %d", pool.resident())
	}
}

func TestBufferPoolResidentSetNeverExceedsCapacity(t *testing.T) {
	t.Parallel()
	pool := newBufferPool(3)
	loads := 0
	for p := uint32(0); p < 100; p++ {
		pool.getOrLoad(p, counting(&loads, p))
		if pool.resident() > 3 {
			t.Fatalf("resident %d exceeds capacity", pool.resident())
		}
	}
	if loads != 100 {
		t.Fatalf("every distinct page should be a miss; loads = %d", loads)
	}
}

func TestBufferPoolClockGivesReferencedPageSecondChance(t *testing.T) {
	t.Parallel()
	// Fill {0,1,2}; touch 0 (sets its ref bit); inserting 3 should evict 1 (the first unreferenced
	// under the hand), sparing the recently-touched 0.
	pool := newBufferPool(3)
	loads := 0
	for p := uint32(0); p < 3; p++ {
		pool.getOrLoad(p, counting(&loads, p))
	}
	pool.getOrLoad(0, counting(&loads, 0)) // hit → ref bit on 0
	pool.getOrLoad(3, counting(&loads, 3)) // miss → evicts 1
	if loads != 4 {
		t.Fatalf("loads = %d", loads)
	}
	before := loads
	pool.getOrLoad(0, counting(&loads, 0)) // 0 spared — still cached
	if loads != before {
		t.Fatal("0 should have been spared (still cached)")
	}
	pool.getOrLoad(1, counting(&loads, 1)) // 1 was evicted — reload
	if loads != before+1 {
		t.Fatal("1 should have been evicted (reloaded)")
	}
}

func TestBufferPoolCapacityOneEvictsEveryTime(t *testing.T) {
	t.Parallel()
	pool := newBufferPool(1)
	loads := 0
	pool.getOrLoad(1, counting(&loads, 1))
	pool.getOrLoad(2, counting(&loads, 2))
	pool.getOrLoad(1, counting(&loads, 1)) // 1 was evicted by 2 → reload
	if loads != 3 {
		t.Fatalf("loads = %d", loads)
	}
	if pool.resident() != 1 {
		t.Fatalf("resident = %d", pool.resident())
	}
}
