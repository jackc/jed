package org.jed;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import org.jed.storage.LeafCodec;
import org.jed.storage.PersistentTree;

/** Format-v53 PAX leaf payloads; internal row bytes use the canonical single-value codec. */
final class TableLeafCodec implements LeafCodec {
    private final List<SqlType> types;
    private final int[] keyOffsets;
    private final int fixedKeyWidth;
    private final org.jed.storage.Pager pager;
    private final long highWater;

    @Override public long recordBytes(PersistentTree.Entry entry) {
        long bytes = entry.key().length;
        StoredRow row = StoredRow.of(entry, types);
        for (int c = 0; c < types.size(); c++) if (keyOffsets[c] < 0) {
            StoredRow.Cell cell = row.cells.get(c); bytes = Math.addExact(bytes, cell.weight());
            if (cell.external()) bytes = Math.addExact(bytes, cell.storedLength);
        }
        return bytes;
    }
    TableLeafCodec(List<SqlType> types, int[] primaryKey) { this(types, primaryKey, null, 0); }
    TableLeafCodec(List<SqlType> types, int[] primaryKey, org.jed.storage.Pager pager, long highWater) { this(types, primaryKey, pager, highWater, false); }
    static TableLeafCodec index() { return new TableLeafCodec(List.of(), new int[0], null, 0, true); }
    private TableLeafCodec(List<SqlType> types, int[] primaryKey, org.jed.storage.Pager pager, long highWater, boolean index) {
        this.pager = pager; this.highWater = highWater;
        this.types = List.copyOf(types);
        keyOffsets = new int[types.size()]; Arrays.fill(keyOffsets, -1);
        int keyWidth = primaryKey.length == 0 ? 8 : 0;
        boolean fixed = true, eliding = true;
        for (int ordinal : primaryKey) {
            SqlType type = types.get(ordinal);
            boolean derivable = switch (type.kind()) {
                case I16, I32, I64, BOOLEAN, UUID, DATE, TIMESTAMP, TIMESTAMPTZ -> true;
                default -> false;
            };
            if (eliding && derivable) keyOffsets[ordinal] = keyWidth;
            else eliding = false;
            int width = type.kind() == SqlType.Kind.DECIMAL ? 0 : Encoding.fixedWidth(type);
            if (width == 0) fixed = false;
            keyWidth += width;
        }
        fixedKeyWidth = !index && fixed && keyWidth <= 65535 ? keyWidth : 0;
    }

    // Every candidate region size is monotone: adding records grows directories/bitmaps,
    // cannot shrink numeric spans or shared-key suffixes, and cannot remove dictionary
    // entries. Choosing the smallest eligible encoding preserves this property; adding
    // an ineligible record only removes a packed candidate. Elided regions stay zero.
    @Override public boolean monotoneSliceSizes() { return true; }
    @Override public long payloadSize(List<PersistentTree.Entry> records) { return layout(records, true).size(); }

    @Override public byte[] encode(List<PersistentTree.Entry> records) { return layout(records, false).bytes(); }

    @Override public LeafCodec.Prepared prepare(List<PersistentTree.Entry> entries, org.jed.storage.PageAllocator input, long txid, int pageSize) throws IOException {
        return prepare(entries, input, txid, pageSize, org.jed.storage.PageCodec.PLAINTEXT);
    }
    @Override public LeafCodec.Prepared prepare(List<PersistentTree.Entry> entries, org.jed.storage.PageAllocator input, long txid, int pageSize, org.jed.storage.PageCodec pageCodec) throws IOException {
        var allocator = input.copy();
        List<org.jed.storage.ManifestEncoder.Write> writes = new ArrayList<>();
        List<PersistentTree.Entry> encoded = new ArrayList<>(entries.size());
        for (var entry : entries) {
            StoredRow row = StoredRow.of(entry, types);
            List<StoredRow.Cell> cells = new ArrayList<>(row.cells);
            for (int c = 0; c < cells.size(); c++) {
                StoredRow.Cell cell = cells.get(c);
                if (cell.external() && cell.firstPage == 0) {
                    var chain = org.jed.storage.OverflowChain.prepare(pageSize, cell.stored, allocator, txid, pageCodec);
                    allocator = chain.allocator(); writes.addAll(chain.writes()); cells.set(c, cell.allocated(chain.firstPage(), chain.tag()));
                }
            }
            encoded.add(new PersistentTree.Entry(entry.key(), new StoredRow(cells)));
        }
        return new LeafCodec.Prepared(encode(encoded), allocator, writes);
    }

    @Override public LeafCodec.Prepared prepareImage(List<PersistentTree.Entry> entries, org.jed.storage.PageAllocator allocator, long txid, int pageSize, org.jed.storage.PageCodec codec) throws IOException {
        List<PersistentTree.Entry> copied = new ArrayList<>(entries.size());
        for (var entry : entries) {
            List<StoredRow.Cell> cells = new ArrayList<>();
            for (var cell : StoredRow.of(entry, types).cells) cells.add(cell.imageCopy());
            copied.add(new PersistentTree.Entry(entry.key(), new StoredRow(cells)));
        }
        return prepare(copied, allocator, txid, pageSize, codec);
    }

    @Override public List<PersistentTree.Entry> committedRecords(List<PersistentTree.Entry> entries) {
        return entries.stream().map(entry -> new PersistentTree.Entry(entry.key(), new StoredRow(StoredRow.of(entry, types).cells))).toList();
    }
    @Override public boolean mayReferencePages() { return types.stream().anyMatch(type -> Encoding.fixedWidth(type) == 0); }
    @Override public java.util.Set<Long> referencedPages(List<PersistentTree.Entry> entries) throws IOException {
        java.util.Set<Long> pages = new java.util.HashSet<>();
        for (var entry : entries) for (var cell : StoredRow.of(entry, types).cells) {
            for (long id : cell.referencedPages()) if (!pages.add(id)) throw new org.jed.storage.CorruptStorageException("shared overflow chain inside leaf");
        }
        return pages;
    }
    private record Region(byte[] bytes, int size) {
        static Region measured(int size) { return new Region(null, size); }
        static Region encoded(byte[] bytes) { return new Region(bytes, bytes.length); }
    }
    private Region layout(List<PersistentTree.Entry> records, boolean sizing) {
        int count = records.size();
        if (count == 0) throw new IllegalArgumentException("empty committed leaf");
        byte[][] keys = new byte[count][];
        List<StoredRow> rows = new ArrayList<>(count);
        for (int i = 0; i < count; i++) {
            keys[i] = records.get(i).key();
            if (keys[i].length == 0 || fixedKeyWidth != 0 && keys[i].length != fixedKeyWidth) throw new IllegalArgumentException("invalid key width");
            if (i > 0 && Arrays.compareUnsigned(keys[i - 1], keys[i]) >= 0) throw new IllegalArgumentException("unordered leaf");
            rows.add(StoredRow.of(records.get(i), types));
        }
        int prefix = 0;
        byte[] first = keys[0], last = keys[count - 1];
        while (prefix < Math.min(65535, Math.min(first.length, last.length)) && first[prefix] == last[prefix]) prefix++;
        if (count < 2 || (long) count * prefix <= prefix + 2L) prefix = 0;
        int headSize = 1 + (fixedKeyWidth == 0 ? 2 * count : 2) + (prefix == 0 ? 0 : prefix + 2);
        for (byte[] key : keys) headSize += key.length - prefix;
        Encoding.Sink head = sizing ? null : new Encoding.Sink(false);
        if (!sizing) {
            head.u8((fixedKeyWidth == 0 ? 0 : 1) | (prefix == 0 ? 0 : 2));
            if (fixedKeyWidth != 0) head.u16(fixedKeyWidth);
            if (prefix != 0) { head.u16(prefix); head.raw(Arrays.copyOf(first, prefix)); }
            if (fixedKeyWidth == 0) {
                int offset = 0;
                for (byte[] key : keys) { offset += key.length - prefix; head.u16(offset); }
            }
            for (byte[] key : keys) head.raw(Arrays.copyOfRange(key, prefix, key.length));
        }
        List<Region> regions = new ArrayList<>(types.size());
        for (int c = 0; c < types.size(); c++) {
            SqlType type = types.get(c);
            if (keyOffsets[c] >= 0) {
                for (int i = 0; i < count; i++) {
                    SqlValue value = rows.get(i).cells.get(c).value();
                    if (value.isNull() || !Arrays.equals(Encoding.key(value), Arrays.copyOfRange(keys[i], keyOffsets[c], keyOffsets[c] + Encoding.fixedWidth(type)))) {
                        throw new IllegalArgumentException("elided value differs from key");
                    }
                }
                regions.add(sizing ? Region.measured(0) : Region.encoded(new byte[0])); continue;
            }
            byte[][] cells = new byte[count][];
            int width = Encoding.fixedWidth(type);
            for (int i = 0; i < count; i++) {
                StoredRow.Cell cell = rows.get(i).cells.get(c);
                if (width == 0) { cells[i] = cell.leafBytes(); continue; }
                SqlValue value = cell.value();
                if (value.isNull()) cells[i] = null;
                else if (width > 0) {
                    Encoding.Sink body = new Encoding.Sink();
                    if (type.kind() == SqlType.Kind.DECIMAL) {
                        Decimal decimal = value.asDecimal().constrain(type.precision(), type.scale());
                        long unscaled = Long.parseLong((decimal.negative() ? "-" : "") + decimal.coefficientDigits());
                        body.signed(unscaled, width);
                    } else Encoding.body(body, value);
                    cells[i] = body.bytes();
                } else if (shortText(type)) {
                    byte[] content = type.kind() == SqlType.Kind.BYTEA ? value.asBytes() : SqlText.encode(value.asText());
                    cells[i] = new byte[content.length + 1]; cells[i][0] = 5;
                    System.arraycopy(content, 0, cells[i], 1, content.length);
                } else cells[i] = Encoding.value(value);
            }
            regions.add(width > 0 ? fixedRegion(type, cells, width, sizing) : variableRegion(type, cells, sizing));
        }
        int offset = headSize + 2 * (types.size() + 1);
        for (Region region : regions) { if (!sizing) head.u16(offset); offset += region.size(); }
        if (sizing) return Region.measured(offset);
        head.u16(offset);
        for (Region region : regions) head.raw(region.bytes());
        if (head.size() > 65535) throw new SqlException("0A000", "leaf payload exceeds maximum page");
        return Region.encoded(head.bytes());
    }

    private static Region fixedRegion(SqlType type, byte[][] cells, int width, boolean sizing) {
        int count = cells.length;
        byte[] bitmap = sizing ? null : new byte[(count + 7) / 8];
        long smallest = 0, largest = 0; boolean any = false;
        for (int i = 0; i < count; i++) {
            if (cells[i] == null) { if (!sizing) bitmap[i / 8] |= (byte) (128 >>> (i % 8)); }
            else if (width <= 8) {
                long value = new Encoding.Reader(cells[i]).unsigned(width);
                if (!any || Long.compareUnsigned(value, smallest) < 0) smallest = value;
                if (!any || Long.compareUnsigned(value, largest) > 0) largest = value;
                any = true;
            }
        }
        boolean offsettable = switch (type.kind()) {
            case I16, I32, I64, DATE, TIMESTAMP, TIMESTAMPTZ, DECIMAL -> true;
            default -> false;
        };
        long delta = largest - smallest;
        int offsetWidth = delta == 0 ? 0 : Long.compareUnsigned(delta, 255) <= 0 ? 1 : Long.compareUnsigned(delta, 65535) <= 0 ? 2 : Long.compareUnsigned(delta, 0xffffffffL) <= 0 ? 4 : 8;
        boolean compressed = offsettable && offsetWidth < width && (long) count * (width - offsetWidth) > width + 1;
        int size = 1 + (count + 7) / 8 + (compressed ? width + 1 + count * offsetWidth : count * width);
        if (sizing) return Region.measured(size);
        Encoding.Sink out = new Encoding.Sink(); out.u8(compressed ? 1 : 0); out.raw(bitmap);
        if (compressed) {
            out.unsigned(smallest, width); out.u8(offsetWidth);
            for (byte[] cell : cells) out.unsigned(cell == null ? 0 : new Encoding.Reader(cell).unsigned(width) - smallest, offsetWidth);
        } else for (byte[] cell : cells) out.raw(cell == null ? new byte[width] : cell);
        return Region.encoded(out.bytes());
    }

    private static Region variableRegion(SqlType type, byte[][] cells, boolean sizing) {
        int plainSize = 1 + 2 * cells.length;
        for (byte[] cell : cells) if (cell != null) plainSize += cell.length;
        if (type.kind() == SqlType.Kind.ARRAY && type.element().isInteger()) {
            Region packed = packedArrays(type.element(), cells, sizing);
            if (packed != null && packed.size() < plainSize) return packed;
            return plainRegion(cells, plainSize, sizing);
        }
        if (!shortText(type)) return plainRegion(cells, plainSize, sizing);
        List<byte[]> dictionary = new ArrayList<>();
        byte[] codes = sizing ? null : new byte[cells.length];
        for (int i = 0; i < cells.length; i++) {
            byte[] cell = cells[i] == null ? new byte[0] : cells[i];
            int code = cell.length == 0 || cell[0] == 5 ? 0 : dictionary.size();
            while (code < dictionary.size() && !Arrays.equals(dictionary.get(code), cell)) code++;
            if (code == dictionary.size()) {
                if (code == 256) return plainRegion(cells, plainSize, sizing);
                dictionary.add(cell);
            }
            if (!sizing) codes[i] = (byte) code;
        }
        int packedSize = 3 + 2 * dictionary.size() + cells.length;
        for (byte[] cell : dictionary) packedSize += cell.length;
        if (packedSize >= plainSize) return plainRegion(cells, plainSize, sizing);
        if (sizing) return Region.measured(packedSize);
        Encoding.Sink packed = new Encoding.Sink(false); packed.u8(2); packed.u16(dictionary.size()); int offset = 0;
        for (byte[] cell : dictionary) { offset += cell.length; packed.u16(offset); }
        for (byte[] cell : dictionary) packed.raw(cell);
        packed.raw(codes);
        return Region.encoded(packed.bytes());
    }

    private static Region plainRegion(byte[][] cells, int size, boolean sizing) {
        if (sizing) return Region.measured(size);
        Encoding.Sink plain = new Encoding.Sink(false); plain.u8(0); int offset = 0;
        for (byte[] cell : cells) { if (cell != null) offset += cell.length; plain.u16(offset); }
        for (byte[] cell : cells) if (cell != null) plain.raw(cell);
        return Region.encoded(plain.bytes());
    }

    private static Region packedArrays(SqlType element, byte[][] cells, boolean sizing) {
        int width = Encoding.fixedWidth(element), total = 0;
        byte[] counts = new byte[cells.length]; long base = 0, largest = 0; boolean any = false;
        for (int i = 0; i < cells.length; i++) {
            byte[] cell = cells[i];
            if (cell == null || cell.length == 0) { counts[i] = (byte) 255; continue; }
            if (cell.length < 3 || cell[0] != 0 || cell[2] != 0) return null;
            if (cell[1] == 0) { if (cell.length != 3) return null; continue; }
            if (cell[1] != 1 || cell.length < 11) return null;
            Encoding.Reader in = new Encoding.Reader(Arrays.copyOfRange(cell, 3, cell.length));
            long count = in.unsigned(4), lower = in.unsigned(4);
            if (count < 1 || count > 254 || lower != 1 || in.remaining() != count * width) return null;
            counts[i] = (byte) count; total += (int) count;
            for (int j = 0; j < count; j++) {
                long value = in.unsigned(width);
                if (!any || Long.compareUnsigned(value, base) < 0) base = value;
                if (!any || Long.compareUnsigned(value, largest) > 0) largest = value;
                any = true;
            }
        }
        int bits = 64 - Long.numberOfLeadingZeros(largest - base);
        int packedSize = (int) (((long) total * bits + 7) / 8);
        if (sizing) return Region.measured(2 + cells.length + width + packedSize);
        byte[] packed = new byte[packedSize]; long bit = 0;
        for (int i = 0; i < cells.length; i++) {
            int count = counts[i] & 255; if (count == 0 || count == 255) continue;
            Encoding.Reader in = new Encoding.Reader(Arrays.copyOfRange(cells[i], 11, cells[i].length));
            for (int j = 0; j < count; j++) {
                long delta = in.unsigned(width) - base;
                for (int b = bits - 1; b >= 0; b--, bit++)
                    packed[(int) (bit / 8)] |= (byte) (((delta >>> b) & 1) << (7 - bit % 8));
            }
        }
        Encoding.Sink out = new Encoding.Sink(false); out.u8(4); out.raw(counts); out.unsigned(base, width); out.u8(bits); out.raw(packed);
        return Region.encoded(out.bytes());
    }

    @Override public List<PersistentTree.Entry> decode(ByteBuffer payload, int count) throws IOException {
        try {
            if (count <= 0 || count > payload.remaining()) throw Encoding.corrupt("invalid leaf count");
            byte[] bytes = new byte[payload.remaining()]; payload.duplicate().get(bytes);
            Encoding.Reader in = new Encoding.Reader(bytes);
            int flags = in.u8(); if ((flags & ~3) != 0) throw Encoding.corrupt("reserved leaf flags");
            int width = (flags & 1) != 0 ? in.u16() : 0;
            if (width != fixedKeyWidth) throw Encoding.corrupt("leaf key width disagrees with schema");
            if ((flags & 1) != 0 && width == 0) throw Encoding.corrupt("zero fixed key width");
            byte[] prefix = (flags & 2) != 0 ? in.raw(in.u16()) : new byte[0];
            if ((flags & 2) != 0 && prefix.length == 0 || width > 0 && prefix.length >= width) throw Encoding.corrupt("invalid key prefix");
            int[] ends = new int[count];
            for (int i = 0; i < count; i++) {
                long end = width > 0 ? (long) (i + 1) * (width - prefix.length) : in.u16();
                if (end > bytes.length || end <= (i == 0 ? 0 : ends[i - 1])) throw Encoding.corrupt("invalid key directory");
                ends[i] = (int) end;
            }
            byte[] blob = in.raw(ends[count - 1]);
            byte[][] keys = new byte[count][];
            for (int i = 0; i < count; i++) {
                int start = i == 0 ? 0 : ends[i - 1];
                keys[i] = Arrays.copyOf(prefix, prefix.length + ends[i] - start);
                System.arraycopy(blob, start, keys[i], prefix.length, ends[i] - start);
                if (i > 0 && Arrays.compareUnsigned(keys[i - 1], keys[i]) >= 0) throw Encoding.corrupt("unordered leaf keys");
            }
            int[] columns = new int[types.size() + 1];
            for (int c = 0; c < columns.length; c++) columns[c] = in.u16();
            if (columns[0] != in.position()) throw Encoding.corrupt("invalid first column offset");
            StoredRow.Cell[][] rows = new StoredRow.Cell[count][types.size()];
            for (int c = 0; c < types.size(); c++) {
                if (columns[c + 1] < columns[c] || columns[c + 1] > bytes.length) throw Encoding.corrupt("invalid column directory");
                byte[] region = Arrays.copyOfRange(bytes, columns[c], columns[c + 1]);
                if (keyOffsets[c] >= 0) {
                    if (region.length != 0) throw Encoding.corrupt("stored elided column");
                    for (int i = 0; i < count; i++) {
                        int end = keyOffsets[c] + Encoding.fixedWidth(types.get(c));
                        if (end > keys[i].length) throw Encoding.corrupt("truncated elided key");
                        rows[i][c] = StoredRow.Cell.plain(Encoding.readBody(new Encoding.Reader(Arrays.copyOfRange(keys[i], keyOffsets[c], end)), types.get(c)));
                    }
                } else decodeRegion(region, types.get(c), rows, c);
            }
            for (int i = columns[types.size()]; i < bytes.length; i++) if (bytes[i] != 0) throw Encoding.corrupt("nonzero leaf tail");
            List<PersistentTree.Entry> entries = new ArrayList<>(count);
            for (int i = 0; i < count; i++) entries.add(new PersistentTree.Entry(keys[i], new StoredRow(Arrays.asList(rows[i]))));
            return entries;
        } catch (SqlException failure) {
            if (failure.sqlState().equals("0A000")) throw failure;
            throw new org.jed.storage.CorruptStorageException(failure.getMessage());
        }
    }

    private void decodeRegion(byte[] region, SqlType type, StoredRow.Cell[][] rows, int column) {
        Encoding.Reader in = new Encoding.Reader(region);
        int kind = in.u8(), count = rows.length, width = Encoding.fixedWidth(type);
        if (width != 0) {
            boolean offsettable = switch (type.kind()) { case I16, I32, I64, DATE, TIMESTAMP, TIMESTAMPTZ, DECIMAL -> true; default -> false; };
            if (kind != 0 && !(kind == 1 && offsettable)) throw Encoding.corrupt("invalid fixed region kind");
            byte[] bitmap = in.raw((count + 7) / 8);
            long base = kind == 1 ? in.unsigned(width) : 0;
            int offsetWidth = kind == 1 ? in.u8() : width;
            if (kind == 1 && (offsetWidth != 0 && offsetWidth != 1 && offsetWidth != 2 && offsetWidth != 4 || offsetWidth >= width)) throw Encoding.corrupt("invalid base-offset width");
            for (int i = 0; i < count; i++) {
                byte[] body;
                if (kind == 0) body = in.raw(width);
                else {
                    long offset = in.unsigned(offsetWidth), value = base + offset;
                    if (Long.compareUnsigned(value, base) < 0 || width < 8 && value >>> (width * 8) != 0) throw Encoding.corrupt("base-offset overflow");
                    Encoding.Sink sink = new Encoding.Sink(); sink.unsigned(value, width); body = sink.bytes();
                }
                if ((bitmap[i / 8] & (128 >>> (i % 8))) != 0) rows[i][column] = StoredRow.Cell.plain(SqlValue.nullValue(type));
                else if (type.kind() == SqlType.Kind.DECIMAL) {
                    long coefficient = new Encoding.Reader(body).signed(width);
                    rows[i][column] = StoredRow.Cell.plain(SqlValue.ofDecimal(Decimal.parse(Long.toString(coefficient) + "e-" + type.scale())));
                } else rows[i][column] = StoredRow.Cell.plain(Encoding.readBody(new Encoding.Reader(body), type));
            }
        } else if (kind == 4 && type.kind() == SqlType.Kind.ARRAY && type.element().isInteger()) {
            byte[] counts = in.raw(count); int elementWidth = Encoding.fixedWidth(type.element());
            long base = in.unsigned(elementWidth); int bits = in.u8();
            if (bits > elementWidth * 8) throw Encoding.corrupt("invalid packed array bit width");
            long total = 0;
            for (byte size : counts) if ((size & 255) != 255) total += size & 255;
            long bitCount = total * bits;
            if ((bitCount + 7) / 8 != in.remaining()) throw Encoding.corrupt("invalid packed array extent");
            byte[] packed = in.raw(in.remaining());
            if (bitCount % 8 != 0 && (packed[packed.length - 1] & ((1 << (8 - bitCount % 8)) - 1)) != 0) throw Encoding.corrupt("nonzero packed array padding");
            long bit = 0;
            for (int i = 0; i < count; i++) {
                int size = counts[i] & 255;
                if (size == 255) { rows[i][column] = StoredRow.Cell.plain(SqlValue.nullValue(type)); continue; }
                Encoding.Sink cell = new Encoding.Sink(); cell.u8(0); cell.u8(size == 0 ? 0 : 1); cell.u8(0);
                if (size != 0) { cell.unsigned(size, 4); cell.unsigned(1, 4); }
                for (int j = 0; j < size; j++) {
                    long delta = 0;
                    for (int b = 0; b < bits; b++, bit++) delta = (delta << 1) | ((packed[(int) (bit / 8)] >>> (7 - bit % 8)) & 1);
                    long value = base + delta;
                    if (Long.compareUnsigned(value, base) < 0 || elementWidth < 8 && value >>> (elementWidth * 8) != 0) throw Encoding.corrupt("packed array element overflow");
                    cell.unsigned(value, elementWidth);
                }
                rows[i][column] = StoredRow.Cell.encoded(type, cell.bytes(), pager, highWater);
            }
        } else {
            if (kind != 0 && !(kind == 2 && shortText(type))) throw Encoding.corrupt("invalid variable region kind");
            int size = kind == 2 ? in.u16() : count;
            if (kind == 2 && (size == 0 || size > 256)) throw Encoding.corrupt("invalid dictionary size");
            int[] ends = new int[size];
            for (int i = 0; i < size; i++) {
                ends[i] = in.u16();
                if (i > 0 && ends[i] < ends[i - 1]) throw Encoding.corrupt("invalid value directory");
            }
            byte[] blob = in.raw(ends[size - 1]);
            StoredRow.Cell[] values = new StoredRow.Cell[size];
            for (int i = 0; i < size; i++) {
                int start = i == 0 ? 0 : ends[i - 1], end = ends[i];
                values[i] = StoredRow.Cell.encoded(type, Arrays.copyOfRange(blob, start, end), pager, highWater);
            }
            boolean[] used = new boolean[size];
            for (int i = 0; i < count; i++) {
                int code = kind == 2 ? in.u8() : i;
                if (code >= size) throw Encoding.corrupt("dictionary code out of range");
                if (used[code] && (values[code].compressed() || values[code].external())) throw Encoding.corrupt("shared compressed/external dictionary entry");
                used[code] = true;
                rows[i][column] = values[code];
            }
        }
        in.end();
    }
    private static boolean shortText(SqlType type) { return type.kind() == SqlType.Kind.TEXT || type.kind() == SqlType.Kind.BYTEA || type.kind() == SqlType.Kind.JSON || type.kind() == SqlType.Kind.JSONPATH; }
}
