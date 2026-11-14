package org.jed;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.util.AbstractList;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import org.jed.storage.*;

/** Immutable stored cells retain dispositions and chain references independently of decoded SQL values. */
final class StoredRow implements PersistentTree.Record {
    final List<Cell> cells;
    private final boolean staged;
    StoredRow(List<Cell> cells) { this(cells, false); }
    StoredRow(List<Cell> cells, boolean staged) { this.cells = List.copyOf(cells); this.staged = staged; }
    @Override public boolean staged() { return staged; }
    static StoredRow of(PersistentTree.Entry entry, List<SqlType> types) {
        if (entry.record() instanceof StoredRow stored) return stored;
        return new StoredRow(Encoding.row(entry.value(), types).stream().map(Cell::plain).toList());
    }
    static PersistentTree.Entry plan(byte[] key, List<SqlValue> values, PersistentTree.Entry previous,
                                     boolean[] changed, int pageSize, CostMeter meter, int[] primaryKey) {
        return plan(key, values, previous, changed, new PageLayout(pageSize, false), meter, primaryKey);
    }
    static PersistentTree.Entry plan(byte[] key, List<SqlValue> values, PersistentTree.Entry previous,
                                     boolean[] changed, PageLayout layout, CostMeter meter, int[] primaryKey) {
        StoredRow old = previous != null && previous.record() instanceof StoredRow stored ? stored : null;
        List<Cell> cells = new ArrayList<>(values.size());
        for (int c = 0; c < values.size(); c++) {
            boolean keep = old != null && changed != null && !changed[c];
            cells.add(keep ? old.cells.get(c) : Cell.plain(values.get(c)));
        }
        boolean[] elided = new boolean[cells.size()];
        for (int column : primaryKey) {
            boolean derivable = switch (cells.get(column).type.kind()) {
                case I16, I32, I64, BOOLEAN, UUID, DATE, TIMESTAMP, TIMESTAMPTZ -> true;
                default -> false;
            };
            if (!derivable) break;
            elided[column] = true;
        }
        long[] sizes = new long[cells.size()];
        int[] forms = new int[cells.size()];
        byte[][] compressed = new byte[cells.size()][];
        long weight = key.length;
        for (int c = 0; c < cells.size(); c++) {
            Cell cell = cells.get(c);
            sizes[c] = elided[c] ? 0 : cell.plainWeight(); weight += sizes[c];
        }
        long target = Math.max(0, (layout.capacity() - 12L - 16L * cells.size()) / 2);
        List<Integer> candidates = new ArrayList<>();
        if (weight > target) {
            for (int c = 0; c < cells.size(); c++) if (!elided[c] && cells.get(c).spillable() && cells.get(c).rawLength >= 32) candidates.add(c);
            candidates.sort((a, b) -> { int order = Long.compare(sizes[b], sizes[a]); return order != 0 ? order : Integer.compare(a, b); });
            for (int c : candidates) {
                if (weight <= target) break;
                Cell cell = cells.get(c);
                if (meter != null) { meter.charge(slabsAt(cell.rawLength, layout.capacity()) * Costs.VALUE_COMPRESS); meter.guard(); }
                int compressedLength;
                if (cell.compressed()) { compressed[c] = cell.stored; compressedLength = cell.storedLength; }
                else if (cell.external()) compressedLength = -1; // Its stored form proves compression was rejected.
                else { compressed[c] = Lz4.compress(cell.content()); compressedLength = compressed[c].length; }
                if (compressedLength >= 0 && 7L + compressedLength < sizes[c]) {
                    weight += 7L + compressedLength - sizes[c]; sizes[c] = 7L + compressedLength; forms[c] = 3;
                }
            }
            candidates.clear();
            for (int c = 0; c < cells.size(); c++) if (!elided[c] && cells.get(c).spillable() && sizes[c] > (forms[c] == 3 ? 13 : 9) + layout.tagBytes()) candidates.add(c);
            candidates.sort((a, b) -> { int order = Long.compare(sizes[b], sizes[a]); return order != 0 ? order : Integer.compare(a, b); });
            for (int c : candidates) {
                if (weight <= target) break;
                int pointer = (forms[c] == 3 ? 13 : 9) + layout.tagBytes();
                weight += pointer - sizes[c]; sizes[c] = pointer; forms[c] = forms[c] == 3 ? 4 : 2;
            }
        }
        // Unchanged forms keep their bytes and immutable chain. A changed disposition resolves
        // only that cell, unmetered: the logical compression attempts were charged above.
        for (int c = 0; c < cells.size(); c++) {
            Cell cell = cells.get(c);
            if (cell.tag == forms[c]) continue;
            Cell replacement = cell.tag == 0 ? cell : Cell.plain(cell.value());
            if (forms[c] == 3 || forms[c] == 4) replacement = replacement.compressed(compressed[c] != null ? compressed[c] : Lz4.compress(replacement.content()));
            if (forms[c] == 2 || forms[c] == 4) replacement = replacement.externalized(layout.tagBytes());
            cells.set(c, replacement);
        }
        return new PersistentTree.Entry(key, new StoredRow(cells, true));
    }
    record ReadCost(long pages, long decompressions) { }
    ReadCost readCost(boolean[] touched, int pageSize) {
        return readCost(touched, new PageLayout(pageSize, false));
    }
    ReadCost readCost(boolean[] touched, PageLayout layout) {
        long pages = 0, decompressions = 0;
        for (int c = 0; c < cells.size(); c++) if (touched[c]) {
            Cell cell = cells.get(c);
            if (cell.external()) pages += slabsAt(cell.storedLength, layout.chainCapacity());
            if (cell.compressed()) decompressions += slabsAt(cell.rawLength, layout.capacity());
        }
        return new ReadCost(pages, decompressions);
    }
    private static long slabs(long length, int pageSize) { return (length + pageSize - 17L) / (pageSize - 16L); }
    private static long slabsAt(long length, int capacity) { return (length + capacity - 1L) / capacity; }
    List<SqlValue> values() { return new Values(cells); }
    static final class Values extends AbstractList<SqlValue> {
        final List<Cell> cells;
        final SqlValue[] materialized;
        Values(List<Cell> cells) { this.cells = cells; materialized = new SqlValue[cells.size()]; }
        public int size() { return cells.size(); }
        public SqlValue get(int index) {
            if (materialized[index] == null) materialized[index] = cells.get(index).value();
            return materialized[index];
        }
        List<SqlValue> residentView() {
            return new AbstractList<>() {
                public int size() { return cells.size(); }
                public SqlValue get(int index) {
                    SqlValue value = materialized[index] == null ? cells.get(index).immediate : materialized[index];
                    if (value == null) throw new HostBatch.Unfetched();
                    return value;
                }
            };
        }
    }
    @Override public byte[] bytes() { return Encoding.row(values()); }

    static final class Cell {
        final SqlType type;
        final SqlValue immediate;
        final int tag;
        final byte[] stored;
        final long firstPage;
        final int storedLength, rawLength;
        final Pager pager;
        final long highWater;
        final byte[] chainTag;
        private Cell(SqlType type, SqlValue value, int tag, byte[] stored, long firstPage, int storedLength, int rawLength, Pager pager, long highWater) {
            this(type, value, tag, stored, firstPage, storedLength, rawLength, pager, highWater, new byte[0]);
        }
        private Cell(SqlType type, SqlValue value, int tag, byte[] stored, long firstPage, int storedLength, int rawLength, Pager pager, long highWater, byte[] chainTag) {
            this.type = type; immediate = value; this.tag = tag; this.stored = stored; this.firstPage = firstPage;
            this.storedLength = storedLength; this.rawLength = rawLength; this.pager = pager; this.highWater = highWater;
            this.chainTag = chainTag.clone();
        }
        static Cell plain(SqlValue value) {
            if (value.isNull() || Encoding.fixedWidth(value.type()) != 0) return new Cell(value.type(), value, 0, null, 0, 0, 0, null, 0);
            byte[] content;
            if (value.type().kind() == SqlType.Kind.TEXT) content = SqlText.encode(value.asText());
            else if (value.type().kind() == SqlType.Kind.JSON) content = SqlText.encode(value.asJson());
            else if (value.type().kind() == SqlType.Kind.JSONPATH) content = SqlText.encode(value.asJsonPath().toString());
            else if (value.type().kind() == SqlType.Kind.BYTEA) content = value.asBytes();
            else { Encoding.Sink sink = new Encoding.Sink(); Encoding.body(sink, value); content = sink.bytes(); }
            return new Cell(value.type(), value, 0, content, 0, content.length, content.length, null, 0);
        }
        boolean spillable() { return Encoding.fixedWidth(type) == 0 && (immediate == null || !immediate.isNull()); }
        boolean compressed() { return tag == 3 || tag == 4; }
        boolean external() { return tag == 2 || tag == 4; }
        long plainWeight() {
            int width = Encoding.fixedWidth(type);
            if (width != 0) return width;
            return immediate != null && immediate.isNull() ? 0 : rawLength + 1L;
        }
        long weight() {
            int width = Encoding.fixedWidth(type);
            if (width != 0) return width;
            if (immediate != null && immediate.isNull()) return 0;
            if (tag == 2) return 9L + chainTag.length; if (tag == 4) return 13L + chainTag.length; if (tag == 3) return 7L + storedLength;
            return rawLength + 1L;
        }
        byte[] content() { return stored; }
        Cell compressed(byte[] bytes) { return new Cell(type, immediate, 3, bytes, 0, bytes.length, rawLength, null, 0); }
        Cell externalized(int tagBytes) { return new Cell(type, immediate, compressed() ? 4 : 2, stored, 0, storedLength, rawLength, null, 0, new byte[tagBytes]); }
        Cell allocated(long first, byte[] chainTag) { return new Cell(type, immediate, tag, stored, first, storedLength, rawLength, null, 0, chainTag); }
        Cell imageCopy() throws IOException {
            if (!external()) return this;
            byte[] bytes = stored;
            if (bytes == null) {
                if (pager == null) throw new IOException("external image value has no pager");
                ByteArrayOutputStream output = new ByteArrayOutputStream(Math.min(storedLength, 65536));
                try (var cursor = OverflowChain.cursor(pager, firstPage, storedLength, highWater, chainTag)) {
                    java.nio.ByteBuffer slab; while ((slab = cursor.next()) != null) { byte[] part = new byte[slab.remaining()]; slab.get(part); output.writeBytes(part); }
                }
                bytes = output.toByteArray();
            }
            return new Cell(type, immediate, tag, bytes, 0, storedLength, rawLength, null, 0, new byte[chainTag.length]);
        }
        byte[] leafBytes() {
            if (immediate != null && immediate.isNull()) return null;
            Encoding.Sink out = new Encoding.Sink();
            if (external()) {
                out.u8(tag); out.unsigned(firstPage, 4); out.unsigned(storedLength, 4); if (compressed()) out.unsigned(rawLength, 4); out.raw(chainTag);
            } else if (compressed()) { out.u8(3); out.unsigned(rawLength, 4); out.sized(stored); }
            else if (shortText(type)) { out.u8(5); out.raw(stored); }
            else { out.u8(0); out.raw(stored); }
            return out.bytes();
        }
        static Cell encoded(SqlType type, byte[] bytes, Pager pager, long highWater) {
            if (bytes.length == 0) return plain(SqlValue.nullValue(type));
            Encoding.Reader in = new Encoding.Reader(bytes); int tag = in.u8();
            if (tag == 2 || tag == 4) {
                long first = in.unsigned(4), length = in.unsigned(4), raw = tag == 4 ? in.unsigned(4) : length;
                byte[] boundTag = in.raw(pager == null ? 0 : pager.layout().tagBytes()); in.end();
                if (first < 2 || length == 0 || raw == 0 || length > Integer.MAX_VALUE - 8 || raw > Integer.MAX_VALUE - 8
                    || pager != null && (first < pager.layout().firstBody() || first >= highWater || OverflowChain.pageCount(pager.layout(), length) > highWater - pager.layout().firstBody())) throw Encoding.corrupt("invalid external value descriptor");
                return new Cell(type, null, tag, null, first, (int) length, (int) raw, pager, highWater, boundTag);
            }
            if (tag == 3) {
                long raw = in.unsigned(4); byte[] stored = in.sized(); in.end();
                if (raw == 0 || raw > Integer.MAX_VALUE - 8) throw Encoding.corrupt("invalid compressed value length");
                return new Cell(type, null, 3, stored, 0, stored.length, (int) raw, null, 0);
            }
            if (shortText(type)) {
                if (tag != 5) throw Encoding.corrupt("invalid text storage tag");
                byte[] content = in.raw(in.remaining());
                return new Cell(type, null, 0, content, 0, content.length, content.length, null, 0);
            }
            if (tag != 0) throw Encoding.corrupt("invalid variable value tag");
            byte[] content = in.raw(in.remaining());
            return new Cell(type, null, 0, content, 0, content.length, content.length, null, 0);
        }
        SqlValue value() {
            if (immediate != null) return immediate;
            byte[] bytes = stored;
            try {
                if (external() && bytes == null) {
                    if (pager == null) throw new SqlException("0A000", "external value has no pager context");
                    ByteArrayOutputStream out = new ByteArrayOutputStream(Math.min(storedLength, 65536));
                    try (var cursor = OverflowChain.cursor(pager, firstPage, storedLength, highWater, chainTag)) {
                        java.nio.ByteBuffer slab;
                        while ((slab = cursor.next()) != null) { byte[] part = new byte[slab.remaining()]; slab.get(part); out.writeBytes(part); }
                    }
                    bytes = out.toByteArray();
                }
                if (compressed()) bytes = Lz4.decompress(bytes, rawLength, rawLength);
                if (type.kind() == SqlType.Kind.TEXT) return SqlValue.ofText(type, SqlText.decode(bytes));
                if (type.kind() == SqlType.Kind.JSON) return SqlValue.ofJson(SqlText.decode(bytes));
                if (type.kind() == SqlType.Kind.JSONPATH) return SqlValue.ofJsonPath(JsonPath.compile(SqlText.decode(bytes)));
                if (type.kind() == SqlType.Kind.BYTEA) return SqlValue.ofBytes(bytes);
                Encoding.Reader input = new Encoding.Reader(bytes); SqlValue result = Encoding.readBody(input, type); input.end(); return result;
            } catch (IOException failure) { throw Engine.storageError(failure); }
            catch (SqlException failure) {
                if (failure.sqlState().equals("0A000") || failure.sqlState().equals("XX001")) throw failure;
                throw Encoding.corrupt("invalid " + type + " stored body");
            }
        }
        Set<Long> referencedPages() throws IOException {
            if (!external() || pager == null || firstPage == 0) return Set.of();
            Set<Long> pages = new HashSet<>();
            try (var cursor = OverflowChain.cursor(pager, firstPage, storedLength, highWater, chainTag)) {
                while (cursor.next() != null) if (!pages.add(cursor.lastPage())) throw new CorruptStorageException("repeated external chain page");
            }
            return pages;
        }
    }
    private static boolean shortText(SqlType type) { return type.kind() == SqlType.Kind.TEXT || type.kind() == SqlType.Kind.BYTEA || type.kind() == SqlType.Kind.JSON || type.kind() == SqlType.Kind.JSONPATH; }
}
