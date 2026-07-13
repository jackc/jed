package jed

// Bounded top-k selection for a blocking ORDER BY ... LIMIT (planner.md §4, spill.md §5).
// The heap root is the worst retained row. A monotonically increasing input position completes
// the ORDER BY comparator, so ties retain exactly the stable full-sort order.

import (
	"container/heap"
	"sort"
)

type topKItem struct {
	row  storedRow
	keys [][]byte // one per collated key; nil for the all-C streaming lane
	pos  uint64
}

type topKHeap struct {
	items    []topKItem
	order    []orderSlot
	collated bool
}

func (h topKHeap) Len() int { return len(h.items) }
func (h topKHeap) Less(i, j int) bool {
	// container/heap is a min-heap; invert the exact order so the worst row is the root.
	return compareTopKItems(h.items[i], h.items[j], h.order, h.collated) > 0
}
func (h topKHeap) Swap(i, j int) { h.items[i], h.items[j] = h.items[j], h.items[i] }
func (h *topKHeap) Push(x any)   { h.items = append(h.items, x.(topKItem)) }
func (h *topKHeap) Pop() any {
	n := len(h.items)
	x := h.items[n-1]
	h.items = h.items[:n-1]
	return x
}

func compareTopKItems(a, b topKItem, order []orderSlot, collated bool) int {
	c := 0
	if collated {
		c = cmpDecorated(a.keys, a.row, b.keys, b.row, order)
	} else {
		c = cmpRowsByOrder(a.row, b.row, order)
	}
	if c != 0 {
		return c
	}
	if a.pos < b.pos {
		return -1
	}
	if a.pos > b.pos {
		return 1
	}
	return 0
}

type topKKeeper struct {
	k         int64
	nextPos   uint64
	collated  bool
	selection topKHeap
}

func newTopKKeeper(k int64, order []orderSlot, collated bool) *topKKeeper {
	return &topKKeeper{k: k, collated: collated, selection: topKHeap{order: order, collated: collated}}
}

func (t *topKKeeper) push(row storedRow) error {
	item := topKItem{row: row, pos: t.nextPos}
	t.nextPos++
	if t.collated {
		var err error
		item.keys, err = collationKeysForRow(row, t.selection.order)
		if err != nil {
			return err
		}
	}
	// A collated LIMIT 0 must still build every sort key: the old decorate-sort path could trap
	// here after scan/filter completed. The all-C path has no fallible sort work to preserve.
	if t.k == 0 {
		return nil
	}
	if int64(t.selection.Len()) < t.k {
		heap.Push(&t.selection, item)
		return nil
	}
	if compareTopKItems(item, t.selection.items[0], t.selection.order, t.collated) < 0 {
		t.selection.items[0] = item
		heap.Fix(&t.selection, 0)
	}
	return nil
}

func (t *topKKeeper) finish() []storedRow {
	items := t.selection.items
	sort.Slice(items, func(i, j int) bool {
		return compareTopKItems(items[i], items[j], t.selection.order, t.collated) < 0
	})
	rows := make([]storedRow, len(items))
	for i := range items {
		rows[i] = items[i].row
	}
	return rows
}

// topKRows consumes the already-materialized pre-sort sequence. Expression ORDER BY values have
// already been appended by the caller; collated sort keys are built for every input row, in input
// order, before any output is returned, preserving the full sort's error timing.
func topKRows(rows []storedRow, order []orderSlot, k int64) ([]storedRow, error) {
	collated := false
	for _, key := range order {
		if key.collation != nil {
			collated = true
			break
		}
	}
	t := newTopKKeeper(k, order, collated)
	for i, row := range rows {
		if err := t.push(row); err != nil {
			return nil, err
		}
		rows[i] = nil // release discarded rows as the consumed input backing slice is walked
	}
	return t.finish(), nil
}
