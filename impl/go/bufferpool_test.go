package jed

import (
	"sync/atomic"
	"testing"
	"time"
)

func BenchmarkBufferPoolPopulate(b *testing.B) {
	const capacity = 32_768
	const populated = 6_900
	node := &pnode{}
	b.ReportAllocs()
	for range b.N {
		pool := newBufferPool(capacity)
		for page := range uint32(populated) {
			pool.insert(page, node)
		}
	}
}

func TestBufferPoolInitialIndexReservationIsBounded(t *testing.T) {
	t.Parallel()
	tests := []struct {
		capacity int
		want     int
	}{
		{capacity: 1, want: 1},
		{capacity: 6_900, want: 6_900},
		{capacity: 8_192, want: 8_192},
		{capacity: int(^uint(0) >> 1), want: 8_192},
	}
	for _, tt := range tests {
		if got := initialBufferPoolIndexCapacity(tt.capacity); got != tt.want {
			t.Errorf("initialBufferPoolIndexCapacity(%d) = %d, want %d", tt.capacity, got, tt.want)
		}
	}
}

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

func TestBufferPoolDistinctPageLoadersRunOutsidePoolLock(t *testing.T) {
	t.Parallel()
	pool := newBufferPool(4)
	started := make(chan uint32, 2)
	release := make(chan struct{})
	results := make(chan uint32, 2)

	for _, page := range []uint32{11, 12} {
		go func() {
			node, err := pool.getOrLoad(page, func() (*pnode, error) {
				started <- page
				<-release
				return &pnode{page: page}, nil
			})
			if err != nil {
				results <- 0
				return
			}
			results <- node.page
		}()
	}

	first := false
	second := false
	for i := 0; i < 2; i++ {
		select {
		case <-started:
			if i == 0 {
				first = true
			} else {
				second = true
			}
		case <-time.After(2 * time.Second):
			i = 2
		}
	}
	close(release)
	got := map[uint32]bool{<-results: true, <-results: true}
	if !first {
		t.Fatal("no page loader entered")
	}
	if !second {
		t.Fatal("a distinct page loader was serialized behind the pool lock")
	}
	if !got[11] || !got[12] {
		t.Fatalf("loaded pages = %v, want 11 and 12", got)
	}
}

func TestBufferPoolSamePageMissIsSingleFlight(t *testing.T) {
	t.Parallel()
	pool := newBufferPool(4)
	var loads atomic.Int32
	leaderStarted := make(chan struct{})
	release := make(chan struct{})
	results := make(chan *pnode, 2)

	go func() {
		node, _ := pool.getOrLoad(7, func() (*pnode, error) {
			loads.Add(1)
			close(leaderStarted)
			<-release
			return &pnode{page: 70}, nil
		})
		results <- node
	}()
	<-leaderStarted
	go func() {
		node, _ := pool.getOrLoad(7, func() (*pnode, error) {
			loads.Add(1)
			return &pnode{page: 71}, nil
		})
		results <- node
	}()
	close(release)

	first, second := <-results, <-results
	if got := loads.Load(); got != 1 {
		t.Fatalf("same page load count = %d, want 1", got)
	}
	if first != second || first.page != 70 {
		t.Fatalf("single-flight results = (%p page %d, %p page %d)", first, first.page, second, second.page)
	}
}

func TestBufferPoolInvalidateDuringLoadPreventsStaleReinsertion(t *testing.T) {
	t.Parallel()
	pool := newBufferPool(4)
	started := make(chan struct{})
	release := make(chan struct{})
	loaded := make(chan *pnode, 1)

	go func() {
		node, _ := pool.getOrLoad(9, func() (*pnode, error) {
			close(started)
			<-release
			return &pnode{page: 90}, nil
		})
		loaded <- node
	}()
	<-started
	pool.invalidate(9)

	// A caller from the newly published snapshot must not join the detached old flight. It loads fresh
	// content immediately while the old caller still holds its immutable bytes.
	fresh, err := pool.getOrLoad(9, func() (*pnode, error) { return &pnode{page: 91}, nil })
	if err != nil {
		t.Fatal(err)
	}
	if fresh.page != 91 {
		t.Fatalf("fresh page = %d, want 91", fresh.page)
	}
	close(release)
	if node := <-loaded; node.page != 90 {
		t.Fatalf("in-flight caller page = %d, want 90", node.page)
	}

	fresh, err = pool.getOrLoad(9, func() (*pnode, error) { return &pnode{page: 92}, nil })
	if err != nil {
		t.Fatal(err)
	}
	if fresh.page != 91 {
		t.Fatalf("cached page = %d, want fresh 91 (old flight replaced it)", fresh.page)
	}
}
