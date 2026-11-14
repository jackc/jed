package org.jed.storage;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

/** Format-v53 catalog page packing and continued-entry transport; entry grammar lives above it. */
public final class CatalogChain {
    private CatalogChain() { }
    private record Part(int flags, int count, byte[] payload) { }

    public record Prepared(long root, PageAllocator allocator, List<ManifestEncoder.Write> writes, Set<Long> retiredPages, byte[] rootTag) {
        public Prepared { allocator = allocator.copy(); writes = List.copyOf(writes); retiredPages = Set.copyOf(retiredPages); rootTag = rootTag.clone(); }
        @Override public PageAllocator allocator() { return allocator.copy(); }
        @Override public byte[] rootTag() { return rootTag.clone(); }
    }

    /** Caller supplies entries in canonical kind/name order. This routine never reorders entries. */
    public static Prepared prepare(int pageSize, List<byte[]> entries, PageAllocator input, long txid,
                                   Pager oldPager, long oldRoot, long oldPageCount) throws IOException {
        return prepare(pageSize, entries, input, txid, oldPager, oldRoot, oldPageCount, PageCodec.PLAINTEXT, new byte[0]);
    }
    public static Prepared prepare(int pageSize, List<byte[]> entries, PageAllocator input, long txid,
                                   Pager oldPager, long oldRoot, long oldPageCount, PageCodec codec, byte[] oldRootTag) throws IOException {
        return prepare(pageSize, entries, input, txid, oldPager, oldRoot, oldPageCount, codec, oldRootTag, false);
    }
    public static Prepared prepare(int pageSize, List<byte[]> entries, PageAllocator input, long txid,
                                   Pager oldPager, long oldRoot, long oldPageCount, PageCodec codec, byte[] oldRootTag, boolean force) throws IOException {
        PageLayout layout = new PageLayout(pageSize, codec.encrypted()); int capacity = layout.chainCapacity();
        List<Part> parts = new ArrayList<>();
        ByteArrayOutputStream pending = new ByteArrayOutputStream(capacity);
        int count = 0;
        for (byte[] entry : entries) {
            if (entry.length == 0) throw new IllegalArgumentException("empty catalog entry");
            if (entry.length > capacity) {
                if (count != 0) { parts.add(new Part(0, count, pending.toByteArray())); pending.reset(); count = 0; }
                for (int at = 0; at < entry.length;) {
                    int length = Math.min(capacity, entry.length - at);
                    parts.add(new Part(at + length < entry.length ? 1 : 0, at == 0 ? 1 : 0,
                        Arrays.copyOfRange(entry, at, at + length)));
                    at += length;
                }
            } else {
                if (pending.size() + entry.length > capacity) {
                    parts.add(new Part(0, count, pending.toByteArray())); pending.reset(); count = 0;
                }
                pending.writeBytes(entry);
                count++;
            }
        }
        if (count != 0 || parts.isEmpty()) parts.add(new Part(0, count, pending.toByteArray()));

        Set<Long> oldPages = new HashSet<>();
        boolean identical = oldRoot != 0;
        long old = oldRoot; byte[] oldTag = oldRootTag.clone();
        int ordinal = 0;
        while (old != 0) {
            if (oldPager == null || oldPager.pageSize() != pageSize || old < layout.firstBody() || old >= oldPageCount || !oldPages.add(old)) {
                throw new CorruptStorageException("invalid old catalog chain");
            }
            Pager.Page page = oldPager.readBody(old, oldTag);
            var header = page.header();
            if (header.type() != 1) throw new CorruptStorageException("catalog pointer names another page type");
            if (ordinal >= parts.size()) identical = false;
            else {
                Part part = parts.get(ordinal);
                if (part.flags != header.flags() || part.count != header.count()) identical = false;
                ByteBuffer bytes = page.buffer();
                for (int i = 0; i < capacity; i++) {
                    byte expected = i < part.payload.length ? part.payload[i] : 0;
                    if (bytes.get(16 + layout.tagBytes() + i) != expected) { identical = false; break; }
                }
            }
            ordinal++;
            old = header.next(); oldTag = new byte[layout.tagBytes()]; page.buffer().get(16, oldTag);
            if (old == 0) for (byte value : oldTag) if (value != 0) throw new CorruptStorageException("nonzero terminal catalog tag");
        }
        if (!force && identical && ordinal == parts.size()) return new Prepared(oldRoot, input, List.of(), Set.of(), oldRootTag);

        PageAllocator allocator = input.copy();
        long[] ids = new long[parts.size()];
        for (int i = 0; i < ids.length; i++) ids[i] = allocator.allocate();
        List<ManifestEncoder.Write> writes = new ArrayList<>();
        byte[] nextTag = new byte[layout.tagBytes()];
        for (int i = parts.size() - 1; i >= 0; i--) {
            Part part = parts.get(i);
            byte[] page = new byte[pageSize];
            page[0] = 1; page[1] = (byte) part.flags;
            PageFormat.put16(page, 2, (int) (txid & 0xffff));
            PageFormat.put32(page, 4, part.count);
            PageFormat.put32(page, 8, i + 1 == ids.length ? 0 : ids[i + 1]);
            System.arraycopy(nextTag, 0, page, 16, nextTag.length);
            System.arraycopy(part.payload, 0, page, 16 + nextTag.length, part.payload.length);
            byte[] sealed = codec.sealBody(ids[i], page); nextTag = codec.bodyTag(sealed);
            writes.add(new ManifestEncoder.Write(ids[i], sealed));
        }
        if (!codec.encrypted()) java.util.Collections.reverse(writes);
        return new Prepared(ids[0], allocator, writes, oldPages, nextTag);
    }

    public static Cursor cursor(Pager pager, long root, long pageCount) { return new Cursor(pager, root, pageCount, null); }

    public static Cursor cursor(Pager pager, long root, long pageCount, byte[] rootTag) { return new Cursor(pager, root, pageCount, rootTag); }

    /** One ordinary page's entries, or one reconstructed oversized entry including its final zero tail. */
    public record Chunk(int itemCount, ByteBuffer payload) {
        public Chunk { payload = payload.asReadOnlyBuffer(); }
        @Override public ByteBuffer payload() { return payload.asReadOnlyBuffer(); }
    }

    public static final class Cursor implements AutoCloseable {
        private final Pager pager;
        private final long pageCount;
        private final Set<Long> visited = new HashSet<>();
        private final List<Long> pages = new ArrayList<>();
        private long next;
        private boolean closed;
        private byte[] tag;

        private Cursor(Pager pager, long root, long pageCount, byte[] tag) { this.pager = pager; this.next = root; this.pageCount = pageCount; this.tag = tag == null ? null : tag.clone(); }
        public List<Long> visitedPages() { return List.copyOf(pages); }

        public Chunk next() throws IOException {
            if (closed || next == 0) return null;
            Pager.Page page = readNext();
            var header = page.header();
            if (header.count() > pager.layout().chainCapacity()) throw new CorruptStorageException("catalog entry count exceeds page");
            ByteBuffer bytes = page.buffer().slice(16 + pager.layout().tagBytes(), pager.layout().chainCapacity());
            if (header.flags() == 0) return new Chunk((int) header.count(), bytes);
            if (header.count() != 1) throw new CorruptStorageException("continued catalog start has invalid count");
            ByteArrayOutputStream entry = new ByteArrayOutputStream();
            while (true) {
                byte[] part = new byte[bytes.remaining()]; bytes.get(part);
                if (entry.size() > Integer.MAX_VALUE - 8 - part.length) throw new CorruptStorageException("catalog entry exceeds Java array capacity");
                entry.writeBytes(part);
                if (header.flags() == 0) break;
                if (next == 0) throw new CorruptStorageException("unterminated catalog continuation");
                page = readNext(); header = page.header();
                if (header.count() != 0) throw new CorruptStorageException("catalog continuation has entries");
                bytes = page.buffer().slice(16 + pager.layout().tagBytes(), pager.layout().chainCapacity());
            }
            return new Chunk(1, ByteBuffer.wrap(entry.toByteArray()));
        }

        private Pager.Page readNext() throws IOException {
            if (next < pager.layout().firstBody() || next >= pageCount || !visited.add(next)) throw new CorruptStorageException("invalid catalog chain");
            long id = next;
            Pager.Page page = pager.readBody(id, tag);
            if (page.header().type() != 1) throw new CorruptStorageException("catalog pointer names another page type");
            pages.add(id);
            next = page.header().next(); tag = new byte[pager.layout().tagBytes()]; page.buffer().get(16, tag);
            if (next == 0) for (byte value : tag) if (value != 0) throw new CorruptStorageException("nonzero terminal catalog tag");
            return page;
        }

        @Override public void close() { closed = true; }
    }
}
