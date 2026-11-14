package org.jed.storage;

import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.function.LongPredicate;

/**
 * Validated publication of an already allocated/serialized delta. Tree allocation
 * and orphan discovery precede this class; their output is checked against the
 * current free list and reader-watermark reuse grant before any storage work.
 * Durable commits finish with a barrier. Relaxed commits retain a cumulative
 * manifest and durable floor until an explicit or capacity-triggered flush.
 */
public final class DurableCommitter {
    @FunctionalInterface public interface HostBarrier { void run() throws IOException; }
    /** Authenticates the selected meta against its plaintext manifest chain. */
    @FunctionalInterface public interface MetaAuthentication {
        void authenticate(int slot, byte[] meta, List<ManifestEncoder.Write> overflowInChainOrder) throws IOException;
    }

    private final Pager pager;
    private final HostBarrier directoryBarrier;
    private final MetaAuthentication authentication;
    private PlaintextRecovery.Selected current;
    private FreeList free;
    private boolean firstWrite = true;
    private PlaintextRecovery.Meta floor;
    private final java.util.TreeMap<Long, Long> pending = new java.util.TreeMap<>();
    private Set<Long> floorFree = Set.of();
    private boolean pendingTruncation;

    public DurableCommitter(Pager pager, PlaintextRecovery.Selected selected,
                            HostBarrier directoryBarrier) throws IOException {
        this(pager, selected, directoryBarrier, null);
    }

    public DurableCommitter(Pager pager, PlaintextRecovery.Selected selected,
                            HostBarrier directoryBarrier, MetaAuthentication authentication) throws IOException {
        if (pager.layout().encrypted() && authentication == null) {
            throw new IllegalArgumentException("encrypted publication requires meta authentication");
        }
        this.authentication = authentication;
        this.pager = pager;
        this.current = selected;
        this.floor = selected.meta();
        this.directoryBarrier = directoryBarrier;
        this.free = FreeList.read(pager, selected);
    }

    public synchronized PlaintextRecovery.Selected current() { return current; }
    public synchronized long durableGeneration() { return floor == null ? 0 : floor.txid(); }
    public synchronized boolean hasPending() { return !pending.isEmpty(); }
    public synchronized List<ManifestEncoder.Entry> pendingEntries() { return pending.entrySet().stream().map(e -> new ManifestEncoder.Entry(e.getKey(), e.getValue())).toList(); }
    public synchronized long[] floorFreePages() { return floorFree.stream().mapToLong(Long::longValue).sorted().toArray(); }
    public synchronized long[] allocatablePages() { return java.util.Arrays.stream(freePages()).filter(id -> pending.isEmpty() || floorFree.contains(id) && !pending.containsKey(id)).toArray(); }
    /** A remote writer plans against the owner's bounded pending set. */
    public synchronized void adoptBatch(List<org.jed.host.PageProtocol.Digest> entries, long[] reusable) {
        pending.clear(); for (var entry : entries) pending.put(entry.index(), entry.digest());
        var allowed = new java.util.HashSet<Long>(); for (long page : reusable) allowed.add(page); floorFree = Set.copyOf(allowed);
        if (!pending.isEmpty()) floor = null;
    }
    public synchronized void flush(boolean shrink) throws IOException {
        if (pager.poisoned()) throw new IOException("database requires reopen after failed barrier");
        if (pending.isEmpty()) return;
        try { pager.sync(); }
        catch (IOException | RuntimeException | Error failure) { pager.revoke(); throw failure; }
        coveredByBarrier();
        if (shrink) shrinkAfterBarrier();
    }
    public synchronized void coveredByBarrier() {
        // A barrier without a pending commit does not change the alternate-slot
        // choice established when both initial/compacted metas have the same txid.
        int floorSlot = pending.isEmpty() ? current.floorSlot() : current.slot();
        floor = current.meta(); current = new PlaintextRecovery.Selected(current.meta(), current.slot(), floorSlot, current.protectedPages());
        pending.clear(); floorFree = Set.of();
    }
    private void beginBatch() { if (pending.isEmpty()) { var pages = new java.util.HashSet<Long>(); for (long page : freePages()) pages.add(page); floorFree = Set.copyOf(pages); } }
    private void shrinkAfterBarrier() throws IOException {
        if (!pendingTruncation) return;
        pendingTruncation = false;
        pager.trimSlackAfterBarrier(current.meta().pageCount());
    }
    private void published(PlaintextRecovery.Selected next, FreeList nextFree, List<ManifestEncoder.Entry> identities, boolean durable, boolean local) throws IOException {
        if (local && next.meta().pageCount() < current.meta().pageCount()) pendingTruncation = true;
        current = next; free = nextFree; residentFree = Set.of(); firstWrite = false;
        pending.clear(); for (var entry : identities) pending.put(entry.page(), entry.crc());
        if (durable) { floor = next.meta(); pending.clear(); floorFree = Set.of(); if (local) shrinkAfterBarrier(); }
        else if (pending.size() >= pager.layout().inlineManifestCapacity()) {
            // Local batch-cap barriers settle any deferred shrink. A page-client
            // barrier leaves it to the owner's next local publication/flush.
            try { flush(local); } catch (IOException ignored) { /* Published commit remains successful; the revoked handle reports the failed flush next. */ }
        }
    }
    private Set<Long> residentFree = Set.of();
    /** Proven dead residue too small to persist a free-list page; lost harmlessly on reopen. */
    public synchronized void residentFree(Set<Long> pages) {
        for (long page : pages) if (page < pager.layout().firstBody() || page >= current.meta().pageCount()
            || page == current.meta().root() || current.protectedPages().contains(page)) throw new IllegalArgumentException("invalid resident free residue");
        residentFree = Set.copyOf(pages);
    }
    public synchronized long[] freePages() {
        var pages = new java.util.TreeSet<Long>(); for (long page : free.pages()) pages.add(page); pages.addAll(residentFree);
        return pages.stream().mapToLong(Long::longValue).toArray();
    }
    private boolean free(long page) { return free.contains(page) || residentFree.contains(page); }

    /** Before the first streamed external body write, stabilize every recovered dependency. */
    public synchronized void prepareExternalWrites() throws IOException {
        if (pager.poisoned()) throw new IOException("database writer requires reopen after storage failure");
        if (!firstWrite) return;
        try {
            if (directoryBarrier != null) directoryBarrier.run();
            if (current.needsStabilization()) pager.sync();
            firstWrite = false;
        } catch (IOException | RuntimeException | Error failure) { pager.poison(); throw failure; }
    }

    /** Checked streaming delta has arrived. No SQL/catalog interpretation is part of publication. */
    public synchronized PlaintextRecovery.Selected publishExternal(byte[] encodedMeta, Set<Long> dependencies) throws IOException {
        return publishExternal(encodedMeta, dependencies, List.of(), true);
    }
    public synchronized PlaintextRecovery.Selected publishExternal(byte[] encodedMeta, Set<Long> dependencies, List<ManifestEncoder.Entry> identities, boolean durable) throws IOException {
        var next = PlaintextRecovery.parseMeta(encodedMeta);
        if (current.meta().txid() == -1L || next.txid() != current.meta().txid() + 1 || !next.layout().equals(pager.layout()))
            throw new IllegalArgumentException("external publication generation");
        int slot = current.floorSlot() ^ 1;
        var published = new PlaintextRecovery.Selected(next, slot, durable ? slot : current.floorSlot(), dependencies);
        // Structural free-list validation is pre-publication and reads no stored SQL.
        FreeList nextFree = FreeList.read(pager, published);
        for (long page : nextFree.pages()) if (!free(page)) throw new CorruptStorageException("client added an orphan to the free list");
        if (nextFree.contains(next.root())) throw new CorruptStorageException("catalog root appears in free list");
        for (var remap : next.remaps()) if (nextFree.contains(remap.parent()) || nextFree.contains(remap.page()))
            throw new CorruptStorageException("remap page appears in free list");
        beginBatch();
        try { pager.writePage(slot, encodedMeta); if (durable) pager.sync(); }
        catch (IOException | RuntimeException | Error failure) { pager.poison(); throw failure; }
        published(published, nextFree, identities, durable, false);
        return published;
    }

    /**
     * The owner holds its writer gate throughout planning and this call. safeReuse
     * is its generation-aware reader-watermark grant, never a user SQL callback.
     * No-op domains bypass this method, including recovery/directory barriers.
     */
    public synchronized PlaintextRecovery.Selected commit(byte[] encodedMeta,
            List<ManifestEncoder.Write> writes, LongPredicate safeReuse) throws IOException {
        return commit(encodedMeta, writes, safeReuse, true);
    }
    public synchronized PlaintextRecovery.Selected commit(byte[] encodedMeta,
            List<ManifestEncoder.Write> writes, LongPredicate safeReuse, boolean durable) throws IOException {
        if (pager.poisoned()) throw new IOException("database writer requires reopen after storage failure");
        var next = PlaintextRecovery.parseMeta(encodedMeta);
        var before = current.meta();
        // A changed catalog domain can publish a new meta while all retained body
        // pages are unchanged. Its empty manifest is valid and checked below.
        if (before.txid() == -1L || next.txid() != before.txid() + 1
            || !next.layout().equals(pager.layout())) {
            throw new IllegalArgumentException("invalid durable commit generation");
        }
        Map<Long, byte[]> staged = new HashMap<>();
        List<ManifestEncoder.Write> body = new ArrayList<>(), manifest = new ArrayList<>();
        for (var write : writes) {
            long id = write.page();
            byte[] bytes = write.bytes();
            if (id < next.layout().firstBody() || id >= next.pageCount() || bytes.length != pager.pageSize()
                || staged.putIfAbsent(id, bytes) != null) {
                throw new IllegalArgumentException("invalid or duplicate staged page");
            }
            if (pending.containsKey(id) || id < before.pageCount() && (!free(id) || !safeReuse.test(id) || !pending.isEmpty() && !floorFree.contains(id))) {
                throw new IllegalArgumentException("staged page is not safe to reuse");
            }
        }
        // Manifest pages are plaintext in both variants. Identify them through
        // the descriptor, never by inspecting an encrypted body's first byte.
        Set<Long> manifestIds = new java.util.HashSet<>();
        long manifestId = next.manifestHead();
        for (long ordinal = 0; ordinal < next.manifestPages(); ordinal++) {
            byte[] bytes = staged.get(manifestId);
            if (bytes == null || !manifestIds.add(manifestId)) {
                throw new IllegalArgumentException("missing or cyclic staged manifest chain");
            }
            var header = PageFormat.parseBody(bytes);
            if (header.type() != 8) throw new IllegalArgumentException("manifest pointer targets body page");
            manifest.add(new ManifestEncoder.Write(manifestId, bytes));
            manifestId = header.next();
        }
        if (manifestId != 0) throw new IllegalArgumentException("manifest chain exceeds descriptor");
        for (var write : writes) {
            boolean isManifest = manifestIds.contains(write.page());
            byte[] bytes = staged.get(write.page());
            byte[] decoded = isManifest ? bytes : pager.codec().openBody(write.page(), bytes, pager.codec().bodyTag(bytes));
            var header = PageFormat.parseBody(decoded, isManifest || !next.encrypted());
            if (header.stamp() != (int) (next.txid() & 0xffff) || !isManifest && header.type() == 8) {
                throw new IllegalArgumentException("staged page has wrong type or transaction stamp");
            }
            if (!isManifest) body.add(write);
        }
        // Truncation must retire only pages safe at the fallback and at all pins.
        for (long id = next.pageCount(); id < before.pageCount(); id++) {
            if (!free(id) || !safeReuse.test(id)) throw new IllegalArgumentException("unsafe high-water reduction");
        }
        BlockStore overlay = overlay(next.pageCount(), staged);
        Set<Long> dependencies = PlaintextRecovery.validateManifest(overlay, next, floor);
        Set<Long> expected = new java.util.HashSet<>(staged.keySet()); expected.addAll(pending.keySet());
        if (!dependencies.equals(expected) || body.size() + pending.size() != next.manifestEntries()
            || manifest.size() != next.manifestPages()) {
            throw new IllegalArgumentException("commit manifest does not cover exactly the staged writes");
        }
        int slot = current.floorSlot() ^ 1;
        if (authentication != null) authentication.authenticate(slot, next.encoded(), List.copyOf(manifest));
        var published = new PlaintextRecovery.Selected(next, slot, durable ? slot : current.floorSlot(), dependencies);
        FreeList nextFree;
        try (Pager plannedPager = new Pager(overlay, pager.pageSize(), pager.pageSize(), false, pager.codec())) {
            nextFree = FreeList.read(plannedPager, published);
        }
        if (nextFree.contains(next.root())) throw new CorruptStorageException("catalog root appears in free list");
        for (var remap : next.remaps()) {
            if (nextFree.contains(remap.parent()) || nextFree.contains(remap.page())) {
                throw new CorruptStorageException("remap page appears in free list");
            }
        }

        // Every failure above is a serialization/planning error: no poisoning and
        // no host writes. From this point a failure requires reopening/recovery.
        List<ManifestEncoder.Entry> identities = new ArrayList<>(pendingEntries());
        for (var write : body) {
            long digest;
            if (next.encrypted()) { var crc = new java.util.zip.CRC32(); crc.update(write.bytes()); digest = crc.getValue(); }
            else digest = PageFormat.bodyCrc(write.bytes());
            identities.add(new ManifestEncoder.Entry(write.page(), digest));
        }
        beginBatch();
        try {
            if (firstWrite) {
                if (directoryBarrier != null) directoryBarrier.run();
                if (current.needsStabilization()) pager.sync();
            }
            pager.reserve(next.pageCount());
            for (var write : body) pager.writePage(write.page(), staged.get(write.page()));
            for (var write : manifest) pager.writePage(write.page(), staged.get(write.page()));
            pager.writePage(slot, next.encoded());
            if (durable) pager.sync();
        } catch (IOException | RuntimeException | Error failure) {
            pager.poison();
            throw failure;
        }
        published(published, nextFree, identities, durable, true);
        return published;
    }

    private BlockStore overlay(long pageCount, Map<Long, byte[]> staged) {
        return new BlockStore() {
            public byte[] readAt(long offset, int length) throws IOException {
                if (length != pager.pageSize() || offset % pager.pageSize() != 0
                    || offset < 0 || offset / pager.pageSize() >= pageCount) {
                    throw new IOException("invalid planned page read");
                }
                byte[] bytes = staged.get(offset / pager.pageSize());
                return bytes == null ? pager.readRaw(offset / pager.pageSize()) : bytes.clone();
            }
            public long size() { return pageCount * pager.pageSize(); }
            public void writeAt(long offset, byte[] bytes) { throw new UnsupportedOperationException(); }
            public void sync() { throw new UnsupportedOperationException(); }
            public void setSize(long size) { throw new UnsupportedOperationException(); }
            public void close() { }
        };
    }
}
