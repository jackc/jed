package org.jed.storage;

import java.io.IOException;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Set;

/** Persisted free-list decoding after meta selection; a failure never permits fallback. */
public final class FreeList {
    private final long[] pages;
    private final long[] chain;
    private FreeList(long[] pages, long[] chain) { this.pages = pages; this.chain = chain; }
    public long[] pages() { return pages.clone(); }
    public long[] chain() { return chain.clone(); }
    public boolean contains(long page) { return Arrays.binarySearch(pages, page) >= 0; }
    public int size() { return pages.length; }

    public static FreeList read(Pager pager, PlaintextRecovery.Selected selected) throws IOException {
        var meta = selected.meta();
        if (meta.pageSize() != pager.pageSize()) throw new IllegalArgumentException("page-size mismatch");
        LongList pages = new LongList();
        LongList chain = new LongList();
        Set<Long> visited = new HashSet<>();
        PageLayout layout = meta.layout();
        if (!layout.equals(pager.layout())) throw new IllegalArgumentException("free-list page variant mismatch");
        byte[] expectedTag = meta.freeTag();
        long previous = layout.firstBody() - 1;
        for (long id = meta.freeListHead(); id != 0;) {
            if (id < layout.firstBody() || id >= meta.pageCount() || !visited.add(id)) {
                throw new CorruptStorageException("invalid free-list chain");
            }
            Pager.Page page = pager.readBody(id, expectedTag);
            var header = page.header();
            if (header.type() != 7 || header.count() > layout.chainCapacity() / 4) {
                throw new CorruptStorageException("invalid free-list page");
            }
            var bytes = page.buffer();
            for (long i = 0; i < header.count(); i++) {
                long entry = Integer.toUnsignedLong(bytes.getInt(16 + layout.tagBytes() + (int) i * 4));
                if (entry <= previous || entry >= meta.pageCount() || selected.protectedPages().contains(entry)) {
                    throw new CorruptStorageException("invalid or protected free-list entry");
                }
                pages.add(entry);
                previous = entry;
            }
            for (int i = 16 + layout.tagBytes() + (int) header.count() * 4; i < layout.bodyEnd(); i++) {
                if (bytes.get(i) != 0) throw new CorruptStorageException("nonzero free-list padding");
            }
            chain.add(id);
            expectedTag = new byte[layout.tagBytes()];
            for (int i = 0; i < expectedTag.length; i++) expectedTag[i] = bytes.get(16 + i);
            id = header.next();
            if (id == 0 && !PageFormat.zeros(expectedTag, 0, expectedTag.length)) throw new CorruptStorageException("nonzero terminal free-list tag");
        }
        long[] free = pages.array();
        long[] links = chain.array();
        for (long id : links) {
            if (Arrays.binarySearch(free, id) >= 0) throw new CorruptStorageException("free-list includes its own chain");
        }
        return new FreeList(free, links);
    }

    private static final class LongList {
        long[] values = new long[16];
        int size;
        void add(long value) throws IOException {
            if (size == values.length) {
                if (size > (Integer.MAX_VALUE - 8) / 2) throw new IOException("free-list exceeds Java array capacity");
                values = Arrays.copyOf(values, size * 2);
            }
            values[size++] = value;
        }
        long[] array() { return Arrays.copyOf(values, size); }
    }
}
