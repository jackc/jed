package org.jed.storage;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;

/** Schema-independent overflow slabs. Cursors retain one page and validate the exact declared length. */
public final class OverflowChain {
    private OverflowChain() {}
    public record Prepared(long firstPage, PageAllocator allocator, List<ManifestEncoder.Write> writes, byte[] tag) {
        public Prepared { allocator = allocator.copy(); writes = List.copyOf(writes); tag = tag.clone(); }
        @Override public PageAllocator allocator() { return allocator.copy(); }
        @Override public byte[] tag() { return tag.clone(); }
    }
    public static long pageCount(int pageSize, long storedLength) {
        return pageCount(new PageLayout(pageSize, false), storedLength);
    }
    public static long pageCount(PageLayout layout, long storedLength) {
        if (storedLength <= 0 || storedLength > 0xffffffffL) throw new IllegalArgumentException("invalid overflow length");
        int capacity = layout.chainCapacity();
        return (storedLength + capacity - 1) / capacity;
    }
    public static Prepared prepare(int pageSize, byte[] stored, PageAllocator input, long txid) throws IOException {
        return prepare(pageSize, stored, input, txid, PageCodec.PLAINTEXT);
    }
    public static Prepared prepare(int pageSize, byte[] stored, PageAllocator input, long txid, PageCodec codec) throws IOException {
        PageLayout layout = new PageLayout(pageSize, codec.encrypted());
        int count = (int) pageCount(layout, stored.length), capacity = layout.chainCapacity();
        PageAllocator allocator = input.copy(); long[] pages = new long[count];
        for (int i = 0; i < count; i++) pages[i] = allocator.allocate();
        List<ManifestEncoder.Write> writes = new ArrayList<>(count);
        byte[] nextTag = new byte[layout.tagBytes()];
        for (int i = count - 1; i >= 0; i--) {
            int offset = i * capacity;
            int length = Math.min(capacity, stored.length - offset);
            byte[] page = new byte[pageSize]; page[0] = 4;
            PageFormat.put16(page, 2, (int) (txid & 0xffff)); PageFormat.put32(page, 4, length);
            PageFormat.put32(page, 8, i + 1 == count ? 0 : pages[i + 1]);
            System.arraycopy(nextTag, 0, page, 16, nextTag.length);
            System.arraycopy(stored, offset, page, 16 + nextTag.length, length);
            byte[] sealed = codec.sealBody(pages[i], page); nextTag = codec.bodyTag(sealed);
            writes.add(new ManifestEncoder.Write(pages[i], sealed));
        }
        if (!codec.encrypted()) java.util.Collections.reverse(writes);
        return new Prepared(pages[0], allocator, writes, nextTag);
    }
    public static Cursor cursor(Pager pager, long firstPage, long storedLength, long highWater) throws IOException {
        return cursor(pager, firstPage, storedLength, highWater, null);
    }
    public static Cursor cursor(Pager pager, long firstPage, long storedLength, long highWater, byte[] tag) throws IOException {
        if (storedLength <= 0 || storedLength > 0xffffffffL || firstPage < pager.layout().firstBody() || firstPage >= highWater
            || pageCount(pager.layout(), storedLength) > highWater - pager.layout().firstBody()) throw new CorruptStorageException("invalid overflow descriptor");
        return new Cursor(pager, firstPage, storedLength, highWater, tag);
    }
    public static final class Cursor implements AutoCloseable {
        private final Pager pager;
        private final long highWater;
        private long next, remaining, lastPage;
        private boolean closed;
        private byte[] tag;
        private Cursor(Pager pager, long first, long length, long highWater, byte[] tag) { this.pager = pager; next = first; remaining = length; this.highWater = highWater; this.tag = tag == null ? null : tag.clone(); }
        public long lastPage() { return lastPage; }
        public ByteBuffer next() throws IOException {
            if (closed || remaining == 0) return null;
            if (next < pager.layout().firstBody() || next >= highWater) throw new CorruptStorageException("overflow pointer out of range");
            Pager.Page page = pager.readBody(next, tag); lastPage = next;
            var header = page.header();
            int count = (int) Math.min(remaining, pager.layout().chainCapacity());
            if (header.type() != 4 || header.count() != count || (remaining == count) != (header.next() == 0)) throw new CorruptStorageException("invalid overflow slab shape");
            ByteBuffer bytes = page.buffer();
            tag = new byte[pager.layout().tagBytes()]; bytes.get(16, tag);
            if (header.next() == 0) for (byte value : tag) if (value != 0) throw new CorruptStorageException("nonzero terminal chain tag");
            for (int i = 16 + tag.length + count; i < pager.layout().bodyEnd(); i++) if (bytes.get(i) != 0) throw new CorruptStorageException("nonzero overflow tail");
            remaining -= count; next = header.next();
            return bytes.slice(16 + tag.length, count).asReadOnlyBuffer();
        }
        @Override public void close() { closed = true; }
    }
}
