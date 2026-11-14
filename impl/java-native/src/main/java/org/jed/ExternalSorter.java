package org.jed;

import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.HashSet;
import java.util.List;
import java.util.PriorityQueue;
import java.util.Set;

/** Stable external sort with binary adjacent-run compaction and at most 64 merge sources. */
final class ExternalSorter implements AutoCloseable {
    private record Run(ScratchFile path, long count, long start) {}
    private final List<SqlType> types;
    private final long workMem;
    private final Path scratch;
    private final MemoryAccount account;
    private final Comparator<List<SqlValue>> comparator;
    private final java.util.function.ToLongFunction<List<SqlValue>> measure;
    private final List<List<SqlValue>> buffer = new ArrayList<>();
    private final Run[] levels = new Run[63];
    private final Set<ScratchFile> files = new HashSet<>();
    private final Set<Source> sources = new HashSet<>();
    private long count, resident, charge, nextRun;
    private boolean finished, closed;
    ExternalSorter(List<SqlType> types, long workMem, Path scratch, MemoryAccount account, Comparator<List<SqlValue>> comparator) {
        this(types, workMem, scratch, account, comparator, MemoryAccount::row);
    }
    ExternalSorter(List<SqlType> types, long workMem, Path scratch, MemoryAccount account, Comparator<List<SqlValue>> comparator, java.util.function.ToLongFunction<List<SqlValue>> measure) {
        this.measure = measure;
        if (workMem < 0) throw new IllegalArgumentException("negative work_mem");
        this.types = List.copyOf(types); this.workMem = workMem; this.scratch = scratch;
        this.account = account; this.comparator = comparator;
    }
    long size() { return count; }
    long residentBytes() { return resident; }
    int runCount() { int count = 0; for (Run run : levels) if (run != null) count++; return count; }
    void append(List<SqlValue> row) {
        ensureOpen();
        if (finished) throw new IllegalStateException("sorter already finished");
        if (row.size() != types.size()) throw new IllegalArgumentException("sort row width");
        try {
            long bytes = measure.applyAsLong(row);
            boolean admitted = account == null || account.tryReserve(bytes);
            boolean canSpill = scratch != null && workMem != 0;
            if (!admitted && !canSpill) throw MemoryAccount.exceeded();
            if (admitted && account != null) charge = MemoryAccount.add(charge, bytes);
            buffer.add(List.copyOf(row)); resident = MemoryAccount.add(resident, bytes);
            count = Math.incrementExact(count);
            if (canSpill && (!admitted || resident > workMem)) spill();
        } catch (IOException failure) { fail(SpillRows.io(failure)); }
        catch (RuntimeException failure) { fail(failure); }
    }
    private ScratchFile create() throws IOException { ScratchFile path = new ScratchFile(scratch, account); files.add(path); return path; }
    private void delete(ScratchFile path) throws IOException { path.close(); files.remove(path); }
    private void spill() throws IOException {
        if (account != null) account.beforeSpill();
        buffer.sort(comparator);
        ScratchFile path = create();
        try (DataOutputStream output = path.output()) {
            for (List<SqlValue> row : buffer) SpillRows.write(output, row);
        }
        Run run = new Run(path, buffer.size(), nextRun++);
        buffer.clear(); resident = 0; release();
        int level = 0;
        while (levels[level] != null) {
            Run left = levels[level]; levels[level] = null;
            run = merge(left, run);
            level++;
        }
        levels[level] = run;
    }
    private Run merge(Run left, Run right) throws IOException {
        ScratchFile path = create();
        try (Source a = new Source(left); Source b = new Source(right); DataOutputStream output = path.output()) {
            a.advance(); b.advance();
            while (a.row != null || b.row != null) {
                Source first = b.row == null || a.row != null && comparator.compare(a.row, b.row) <= 0 ? a : b;
                SpillRows.write(output, first.row); first.advance();
            }
        }
        return new Run(path, Math.addExact(left.count, right.count), left.start);
    }
    RowSpool.Cursor finish() {
        ensureOpen();
        if (finished) throw new IllegalStateException("sorter already finished");
        finished = true;
        try {
            buffer.sort(comparator);
            PriorityQueue<Source> heap = new PriorityQueue<>((a, b) -> {
                int order = comparator.compare(a.row, b.row);
                return order != 0 ? order : Long.compare(a.start, b.start);
            });
            for (Run run : levels) if (run != null) {
                Source source = new Source(run); source.advance();
                if (source.row != null) heap.add(source);
            }
            if (!buffer.isEmpty()) { Source source = new Source(); source.advance(); heap.add(source); }
            return new RowSpool.Cursor() {
                private boolean done;
                @Override public List<SqlValue> next() {
                    if (done || closed) return null;
                    if (heap.isEmpty()) { close(); return null; }
                    try {
                        Source first = heap.remove(); List<SqlValue> result = first.row;
                        first.advance();
                        if (first.row != null) heap.add(first);
                        return result;
                    } catch (IOException failure) { fail(SpillRows.io(failure)); throw new AssertionError(); }
                    catch (RuntimeException failure) { fail(failure); throw new AssertionError(); }
                }
                @Override public void close() { if (!done) { done = true; heap.clear(); ExternalSorter.this.close(); } }
            };
        } catch (IOException failure) { fail(SpillRows.io(failure)); throw new AssertionError(); }
        catch (RuntimeException failure) { fail(failure); throw new AssertionError(); }
    }
    private final class Source implements AutoCloseable {
        private final Run run;
        private final long start;
        private DataInputStream input;
        private long at, headCharge;
        private List<SqlValue> row;
        private boolean done;
        Source() { run = null; start = nextRun; sources.add(this); }
        Source(Run run) throws IOException {
            this.run = run; start = run.start; input = run.path.input(); sources.add(this);
        }
        void advance() throws IOException {
            if (done) return;
            List<SqlValue> next = at == (run == null ? buffer.size() : run.count) ? null
                : run == null ? buffer.get(Math.toIntExact(at)) : SpillRows.read(input, types.size());
            long reserved = 0;
            if (next != null && run != null && account != null) {
                long bytes = MemoryAccount.row(next);
                if (account.tryReserve(bytes)) reserved = bytes;
            }
            if (account != null) account.release(headCharge);
            headCharge = reserved; row = next; at++;
            if (next == null) close();
        }
        @Override public void close() throws IOException {
            if (done) return;
            done = true; sources.remove(this); row = null;
            if (account != null) account.release(headCharge);
            headCharge = 0;
            if (input != null) {
                try { input.close(); } finally { input = null; delete(run.path); }
            }
        }
    }
    private void release() { if (account != null) account.release(charge); charge = 0; }
    private void ensureOpen() { if (closed) throw new IllegalStateException("sorter closed"); }
    private void fail(RuntimeException failure) {
        try { close(); } catch (RuntimeException cleanup) { failure.addSuppressed(cleanup); }
        throw failure;
    }
    @Override public void close() {
        if (closed) return;
        closed = true;
        IOException failure = null;
        for (Source source : List.copyOf(sources)) try { source.close(); } catch (IOException error) { failure = error; }
        buffer.clear(); resident = 0; release();
        for (ScratchFile path : List.copyOf(files)) try { delete(path); } catch (IOException error) { failure = error; }
        if (failure != null) throw SpillRows.io(failure);
    }
}
