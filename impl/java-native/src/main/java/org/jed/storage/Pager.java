package org.jed.storage;

import java.io.IOException;
import java.io.InterruptedIOException;
import java.nio.ByteBuffer;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;

/**
 * Host-independent page I/O and bounded immutable-page cache. Logical SQL page
 * cost belongs to tree traversal, never to cache misses or checksum work here.
 * Allocation, reader watermarks, and validated publication belong to the writer.
 */
public final class Pager implements AutoCloseable {
    public static final long DEFAULT_CACHE_BYTES = 256L * 1024 * 1024;
    private static final long MAX_PAGES = 0xffffffffL;
    private volatile BlockStore store;
    private final int pageSize;
    private final PageCodec codec;
    private final PageLayout layout;
    private long cachePages;
    private final Object cacheLock = new Object();
    private long cacheEpoch;
    private final Map<Long, Page> cache = new LinkedHashMap<>(16, 0.75f, true);
    private final Map<Long, CompletableFuture<Page>> loading = new HashMap<>();
    private volatile boolean closed;
    private boolean poisoned;
    private volatile boolean revoked;
    private long allocatedPages;

    /** pinnedMemory disables eviction for a pure in-memory database. */
    public Pager(BlockStore store, int pageSize, long cacheBytes, boolean pinnedMemory) throws IOException {
        this(store, pageSize, cacheBytes, pinnedMemory, PageCodec.PLAINTEXT);
    }
    public Pager(BlockStore store, int pageSize, long cacheBytes, boolean pinnedMemory, PageCodec codec) throws IOException {
        if (!PageFormat.validPageSize(pageSize)) throw new IllegalArgumentException("invalid page size");
        if (cacheBytes < 0) throw new IllegalArgumentException("negative cache budget");
        this.store = store;
        this.pageSize = pageSize;
        this.codec = java.util.Objects.requireNonNull(codec); layout = new PageLayout(pageSize, codec.encrypted());
        this.cachePages = pinnedMemory ? Long.MAX_VALUE : Math.max(1, cacheBytes / pageSize);
        this.allocatedPages = store.size() / pageSize;
    }

    /** Shared immutable resident bytes. Every buffer view has independent position/limit. */
    public static final class Page {
        private final byte[] bytes;
        private final PageFormat.BodyHeader header;
        private final byte[] tag;
        private record DecodedLeaf(LeafCodec codec, java.util.List<PersistentTree.Entry> rows) { }
        private volatile DecodedLeaf decodedLeaf;
        private Page(byte[] bytes, PageFormat.BodyHeader header, byte[] tag) {
            this.bytes = bytes;
            this.header = header;
            this.tag = tag;
        }
        public ByteBuffer buffer() { return ByteBuffer.wrap(bytes).asReadOnlyBuffer(); }
        public byte[] copyBytes() { return bytes.clone(); }
        public PageFormat.BodyHeader header() { return header; }
        public byte[] tag() { return tag.clone(); }
        // One schema/context interpretation follows the page's ordinary eviction
        // lifetime. Decoder identity includes its pager and snapshot high-water;
        // a different schema or snapshot must validate the bytes independently.
        java.util.List<PersistentTree.Entry> decodedLeaf(LeafCodec decoder) {
            DecodedLeaf present = decodedLeaf;
            return present != null && present.codec == decoder ? present.rows : null;
        }
        void rememberLeaf(LeafCodec decoder, java.util.List<PersistentTree.Entry> rows) {
            decodedLeaf = new DecodedLeaf(decoder, java.util.List.copyOf(rows));
        }
    }

    public int pageSize() { return pageSize; }
    public PageLayout layout() { return layout; }
    public PageCodec codec() { return codec; }
    public boolean readAheadEnabled() { return store.maxReadPages() > 0; }
    /** Hints are best effort, bounded by a quarter of this pool and decoded by the ordinary codec. */
    public void prefetch(java.util.List<PagePointer> candidates) {
        int cap = (int)Math.min(store.maxReadPages(), Math.max(1, cachePages / 4));
        if (cap == 0) return;
        var requested = new java.util.ArrayList<PagePointer>();
        long epoch;
        synchronized (cacheLock) {
            if (closed || revoked) return;
            epoch = cacheEpoch;
            for (var pointer : candidates) if (!cache.containsKey(pointer.page()) && !loading.containsKey(pointer.page())) { requested.add(pointer); if (requested.size() == cap) break; }
        }
        if (requested.isEmpty()) return;
        try {
            var pages = store.readPages(requested.stream().mapToLong(PagePointer::page).toArray(), pageSize);
            for (int i = 0; i < pages.size(); i++) {
                var pointer = requested.get(i); byte[] stored = pages.get(i);
                byte[] decoded = codec.openBody(pointer.page(), stored, pointer.tag());
                Page page = new Page(decoded, PageFormat.parseBody(decoded, !codec.encrypted()), codec.bodyTag(stored));
                synchronized (cacheLock) {
                    if (cacheEpoch == epoch && !closed && !revoked && !cache.containsKey(pointer.page()) && !loading.containsKey(pointer.page())) {
                        cache.put(pointer.page(), page); while (cache.size() > cachePages) cache.remove(cache.keySet().iterator().next());
                    }
                }
            }
        } catch (IOException | RuntimeException ignored) { /* The following demand read reports any actual fault. */ }
    }

    public Page readBody(long index) throws IOException {
        return readBody(index, null);
    }
    public Page readBody(long index, byte[] expectedTag) throws IOException {
        checkPageIndex(index);
        if (index < layout.firstBody()) throw new CorruptStorageException("reserved page referenced as body page");
        CompletableFuture<Page> future;
        boolean ownLoad = false;
        synchronized (cacheLock) {
            ensureOpen();
            Page present = cache.get(index);
            if (present != null) { codec.checkTag(present.tag, expectedTag); return present; }
            future = loading.get(index);
            if (future == null) {
                future = new CompletableFuture<>();
                loading.put(index, future);
                ownLoad = true;
            }
        }
        if (ownLoad) {
            try {
                byte[] bytes = store.readAt(index * pageSize, pageSize);
                byte[] decoded = codec.openBody(index, bytes, expectedTag);
                Page page = new Page(decoded, PageFormat.parseBody(decoded, !codec.encrypted()), codec.bodyTag(bytes));
                synchronized (cacheLock) {
                    // Invalidation detaches this load. Existing waiters keep their
                    // immutable result; it cannot repopulate the new generation.
                    if (loading.get(index) == future) {
                        loading.remove(index);
                        if (!closed) {
                            cache.put(index, page);
                            while (cache.size() > cachePages) cache.remove(cache.keySet().iterator().next());
                        }
                    }
                }
                future.complete(page);
            } catch (Throwable failure) {
                synchronized (cacheLock) {
                    if (loading.get(index) == future) loading.remove(index);
                }
                future.completeExceptionally(failure);
            }
        }
        try {
            Page page = future.get(); codec.checkTag(page.tag, expectedTag); return page;
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new InterruptedIOException("interrupted while waiting for a page");
        } catch (ExecutionException e) {
            Throwable failure = e.getCause();
            if (failure instanceof IOException io) throw io;
            if (failure instanceof RuntimeException runtime) throw runtime;
            if (failure instanceof Error error) throw error;
            throw new IOException("page load failed", failure);
        }
    }

    /** Uncached physical read for meta/recovery; codecs sit above the raw byte seam. */
    public byte[] readRaw(long index) throws IOException {
        checkPageIndex(index);
        ensureOpen();
        return store.readAt(index * pageSize, pageSize);
    }

    public void invalidate(long index) {
        synchronized (cacheLock) {
            cacheEpoch++;
            cache.remove(index);
            loading.remove(index);
        }
    }

    /** Client incarnation changes detach all resident and in-flight entries. */
    public void invalidateAll() { synchronized (cacheLock) { cacheEpoch++; cache.clear(); loading.clear(); } }

    public long cacheBytes() { synchronized (cacheLock) { return cachePages == Long.MAX_VALUE ? Long.MAX_VALUE : cachePages * pageSize; } }

    public long residentPages() {
        synchronized (cacheLock) { return cache.size(); }
    }

    /** Session-private migration; callers serialize all reads and mutations of this pager. */
    public synchronized void migrateStore(BlockStore replacement, long cacheBytes) throws IOException {
        ensureWritable();
        synchronized (cacheLock) { if (!loading.isEmpty()) throw new IOException("cannot migrate a pager with active reads"); }
        if (replacement.size() != 0) throw new IOException("migration destination must be empty");
        long length = store.size(); replacement.setSize(length);
        for (long at = 0; at < length;) {
            int count = (int) Math.min(65536, length - at);
            replacement.writeAt(at, store.readAt(at, count)); at += count;
        }
        BlockStore previous = store; store = replacement; setCacheBytes(cacheBytes); previous.close();
    }
    public void setCacheBytes(long bytes) {
        if (bytes < 0) throw new IllegalArgumentException("negative cache budget");
        synchronized (cacheLock) {
            cachePages = Math.max(1, bytes / pageSize);
            while (cache.size() > cachePages) cache.remove(cache.keySet().iterator().next());
        }
    }

    public synchronized long allocatedPages() { return allocatedPages; }
    public synchronized boolean poisoned() { return poisoned; }

    /** The publication coordinator uses this when a host stage outside byte I/O fails. */
    public synchronized void poison() { poisoned = true; }
    public synchronized void revoke() { poisoned = true; revoked = true; }
    public boolean revoked() { return revoked; }

    /** Geometric durable growth, bounded to a 1 MiB step and floored at 16 KiB. */
    public synchronized void reserve(long minimumPages) throws IOException {
        ensureWritable();
        if (minimumPages < 0 || minimumPages > MAX_PAGES) throw new IOException("page-count overflow");
        if (minimumPages <= allocatedPages) return;
        long target = allocatedPages;
        while (target < minimumPages) {
            target = Math.min(MAX_PAGES, target + growthStep(target));
        }
        try {
            store.setSize(target * pageSize);
            allocatedPages = target;
        } catch (IOException e) { poisoned = true; throw e; }
    }

    /**
     * Writer-only low-level operation, after planning has proved reuse safety.
     * Does not publish a generation. Any I/O failure permanently poisons writes.
     */
    public synchronized void writePage(long index, byte[] bytes) throws IOException {
        ensureWritable();
        checkPageIndex(index);
        if (bytes.length != pageSize || index >= allocatedPages) {
            throw new IOException("page write outside reserved storage");
        }
        invalidate(index);
        try { store.writeAt(index * pageSize, bytes); }
        catch (IOException e) { poisoned = true; throw e; }
    }

    public synchronized void sync() throws IOException {
        ensureWritable();
        try { store.sync(); }
        catch (IOException e) { poisoned = true; throw e; }
    }

    /**
     * Caller must first durably publish the smaller high-water, with reader reuse
     * permission. Failure is harmless: the already committed file merely stays long.
     */
    public synchronized void trimSlackAfterBarrier(long committedPages) throws IOException {
        ensureWritable();
        if (committedPages < 0 || committedPages > MAX_PAGES) throw new IOException("page-count overflow");
        long target = Math.min(MAX_PAGES, committedPages + growthStep(committedPages));
        if (allocatedPages <= target) return;
        try {
            store.setSize(target * pageSize);
            allocatedPages = target;
            synchronized (cacheLock) {
                cache.keySet().removeIf(index -> index >= target);
                loading.keySet().removeIf(index -> index >= target);
            }
        } catch (IOException ignored) { /* Post-barrier shrink is best effort. */ }
    }

    private long growthStep(long pages) {
        return Math.max(Math.max(1, 16384 / pageSize), Math.min(pages, Math.max(1, 1048576 / pageSize)));
    }

    private static void checkPageIndex(long index) throws IOException {
        if (index < 0 || index >= MAX_PAGES) throw new IOException("page index overflow");
    }

    private void ensureOpen() throws IOException {
        if (closed) throw new IOException("pager is closed");
        if (revoked) throw new IOException("database storage was revoked after a failed barrier");
    }

    private void ensureWritable() throws IOException {
        ensureOpen();
        if (poisoned) throw new IOException("database writer requires reopen after storage failure");
    }

    @Override public synchronized void close() throws IOException {
        if (closed) return;
        closed = true;
        synchronized (cacheLock) { cache.clear(); loading.clear(); }
        store.close();
    }
}
