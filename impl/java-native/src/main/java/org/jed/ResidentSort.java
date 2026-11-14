package org.jed;

import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.PriorityQueue;
import java.util.function.ToLongFunction;

/** Resident sort state, with bounded top-k retention and stable equal-key ordering. */
final class ResidentSort implements AutoCloseable {
    private record Entry(List<SqlValue> row, long ordinal, long bytes) {}
    private final MemoryAccount account;
    private final ToLongFunction<List<SqlValue>> measure;
    private final Comparator<Entry> order;
    private final long capacity;
    private final ArrayList<Entry> all = new ArrayList<>();
    private final PriorityQueue<Entry> heap;
    private long charge, ordinal;
    private boolean closed;
    ResidentSort(MemoryAccount account, long capacity, Comparator<List<SqlValue>> compare, ToLongFunction<List<SqlValue>> measure) {
        this.account = account; this.capacity = capacity; this.measure = measure;
        order = (a, b) -> { int c = compare.compare(a.row, b.row); return c == 0 ? Long.compare(a.ordinal, b.ordinal) : c; };
        heap = capacity == Long.MAX_VALUE ? null : new PriorityQueue<>(order.reversed());
    }
    void append(List<SqlValue> row) {
        if (closed) throw new IllegalStateException("closed resident sort");
        Entry entry = new Entry(List.copyOf(row), ordinal++, measure.applyAsLong(row));
        if (capacity == 0 || heap != null && heap.size() >= capacity && order.compare(entry, heap.peek()) >= 0) return;
        account.reserve(entry.bytes); charge = MemoryAccount.add(charge, entry.bytes);
        if (heap == null) all.add(entry);
        else {
            if (heap.size() >= capacity) { Entry old = heap.remove(); account.release(old.bytes); charge -= old.bytes; }
            heap.add(entry);
        }
    }
    RowSpool.Cursor finish(ToLongFunction<List<SqlValue>> retainedMeasure) {
        if (heap != null) { all.addAll(heap); heap.clear(); }
        account.release(charge); charge = 0;
        for (Entry entry : all) { long bytes = retainedMeasure.applyAsLong(entry.row); account.reserve(bytes); charge = MemoryAccount.add(charge, bytes); }
        return finish();
    }
    RowSpool.Cursor finish() {
        if (heap != null) { all.addAll(heap); heap.clear(); }
        all.sort(order);
        return new RowSpool.Cursor() {
            private int at;
            public List<SqlValue> next() { if (closed || at == all.size()) { close(); return null; } return all.get(at++).row; }
            public void close() { ResidentSort.this.close(); }
        };
    }
    public void close() {
        if (closed) return;
        closed = true; all.clear(); if (heap != null) heap.clear(); account.release(charge); charge = 0;
    }
}
