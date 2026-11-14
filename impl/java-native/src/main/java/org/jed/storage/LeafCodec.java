package org.jed.storage;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.List;
import java.util.Set;

/** Schema-aware format-v53 leaf payload; tree/page headers are owned by storage. */
public interface LeafCodec {
    record Prepared(byte[] payload, PageAllocator allocator, List<ManifestEncoder.Write> writes) {
        public Prepared { payload = payload.clone(); allocator = allocator.copy(); writes = List.copyOf(writes); }
        @Override public byte[] payload() { return payload.clone(); }
        @Override public PageAllocator allocator() { return allocator.copy(); }
    }
    default long recordBytes(PersistentTree.Entry entry) { return (long)entry.key().length + entry.value().length; }
    /** Exact encoded payload bytes, including all column/key directory and compression savings. */
    long payloadSize(List<PersistentTree.Entry> entries) throws IOException;
    /** True only when extending either end of a nonempty ordered slice never decreases its payload size. */
    default boolean monotoneSliceSizes() { return false; }
    /** Encodes only the used payload (no 16-byte header and no page tail). */
    byte[] encode(List<PersistentTree.Entry> entries) throws IOException;
    /** Reads the full page payload, including its zero tail; checks shape, count and padding. */
    List<PersistentTree.Entry> decode(ByteBuffer payload, int count) throws IOException;
    /** Fresh value chains are allocated in record/column order; the caller chooses the leaf's allocation point. */
    default Prepared prepare(List<PersistentTree.Entry> entries, PageAllocator allocator, long txid, int pageSize) throws IOException {
        return new Prepared(encode(entries), allocator, List.of());
    }
    default Prepared prepare(List<PersistentTree.Entry> entries, PageAllocator allocator, long txid, int pageSize, PageCodec codec) throws IOException {
        return prepare(entries, allocator, txid, pageSize);
    }
    /** From-scratch copies relocate kept external chains while preserving their stored bytes. */
    default Prepared prepareImage(List<PersistentTree.Entry> entries, PageAllocator allocator, long txid, int pageSize, PageCodec codec) throws IOException {
        return prepare(entries, allocator, txid, pageSize, codec);
    }
    /** Drop statement-local staging flags without changing retained resident value representations. */
    default List<PersistentTree.Entry> committedRecords(List<PersistentTree.Entry> entries) { return entries; }
    /** Existing external chain ids retained by these rows, without materializing their contents. */
    default Set<Long> referencedPages(List<PersistentTree.Entry> entries) throws IOException { return Set.of(); }
    default boolean mayReferencePages() { return false; }
}
