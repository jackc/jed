package org.jed;

import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

/** Repeatable, input-ordered row storage. A null scratch target keeps memory databases resident. */
final class RowSpool implements AutoCloseable {
    interface Cursor extends AutoCloseable {
        List<SqlValue> next();
        @Override void close();
    }
    private final List<SqlType> types;
    private final long workMem;
    private final Path scratch;
    private final MemoryAccount account;
    private final List<List<SqlValue>> rows = new ArrayList<>();
    private final Set<Reader> readers = new HashSet<>();
    private ScratchFile file;
    private DataOutputStream output;
    private long count, resident, charge;
    private boolean frozen, closed;
    RowSpool(List<SqlType> types, long workMem, Path scratch, MemoryAccount account) {
        if (workMem < 0) throw new IllegalArgumentException("negative work_mem");
        this.types = List.copyOf(types); this.workMem = workMem; this.scratch = scratch; this.account = account;
    }
    RowSpool(List<SqlType> types, long workMem) { this(types, workMem, null, null); }
    List<SqlType> types() { return types; }
    long size() { return count; }
    long residentBytes() { return resident; }
    boolean spilled() { return file != null; }
    void append(List<SqlValue> row) {
        ensureOpen();
        if (frozen) throw new IllegalStateException("spool is already being read");
        if (row.size() != types.size()) throw new IllegalArgumentException("spool row width");
        try {
            if (output != null) SpillRows.write(output, row);
            else {
                long bytes = MemoryAccount.row(row);
                boolean admitted = account == null || account.tryReserve(bytes);
                if (!admitted && !canSpill()) throw MemoryAccount.exceeded();
                if (admitted && account != null) charge = MemoryAccount.add(charge, bytes);
                rows.add(List.copyOf(row)); resident = MemoryAccount.add(resident, bytes);
                if (canSpill() && (!admitted || resident > workMem)) spill();
            }
            count = Math.incrementExact(count);
        } catch (IOException failure) { fail(SpillRows.io(failure)); }
        catch (RuntimeException failure) { fail(failure); }
    }
    void releaseRow(List<SqlValue> row) {
        ensureOpen();
        if (file != null) return;
        if (account != null) { long bytes = MemoryAccount.row(row); account.release(bytes); charge -= bytes; }
    }
    void transferTo(List<SqlValue> row, RowSpool target) {
        ensureOpen();
        if (file != null || account != target.account) throw new IllegalStateException("row transfer requires a shared resident account");
        long bytes = MemoryAccount.row(row);
        if (account != null) { account.release(bytes); charge -= bytes; }
        target.append(row);
    }
    private boolean canSpill() { return scratch != null && workMem != 0; }
    private void spill() throws IOException {
        if (account != null) account.beforeSpill();
        file = new ScratchFile(scratch, account);
        output = file.output();
        for (List<SqlValue> row : rows) SpillRows.write(output, row);
        rows.clear(); resident = 0; release();
    }
    Cursor cursor() {
        ensureOpen();
        try {
            if (output != null) { output.close(); output = null; }
            frozen = true;
            Reader reader = new Reader(file == null ? null : file.input());
            readers.add(reader);
            return reader;
        } catch (IOException failure) { fail(SpillRows.io(failure)); throw new AssertionError(); }
    }
    private final class Reader implements Cursor {
        private DataInputStream input;
        private long at;
        private boolean done;
        Reader(DataInputStream input) { this.input = input; }
        @Override public List<SqlValue> next() {
            if (done) return null;
            if (at == count) { close(); return null; }
            try {
                List<SqlValue> row = input == null ? rows.get(Math.toIntExact(at)) : SpillRows.read(input, types.size());
                at++;
                return row;
            } catch (IOException failure) { fail(SpillRows.io(failure)); throw new AssertionError(); }
        }
        @Override public void close() {
            if (done) return;
            done = true; readers.remove(this);
            if (input != null) try { input.close(); } catch (IOException failure) { throw SpillRows.io(failure); } finally { input = null; }
        }
    }
    private void ensureOpen() { if (closed) throw new IllegalStateException("spool closed"); }
    private void release() { if (account != null) account.release(charge); charge = 0; }
    private void fail(RuntimeException failure) {
        try { close(); } catch (RuntimeException cleanup) { failure.addSuppressed(cleanup); }
        throw failure;
    }
    @Override public void close() {
        if (closed) return;
        closed = true;
        RuntimeException failure = null;
        for (Reader reader : List.copyOf(readers)) try { reader.close(); } catch (RuntimeException error) { failure = error; }
        if (output != null) try { output.close(); } catch (IOException error) { failure = SpillRows.io(error); } finally { output = null; }
        rows.clear(); resident = 0; release();
        if (file != null) try { file.close(); } catch (IOException error) { failure = SpillRows.io(error); }
        if (failure != null) throw failure;
    }
}
