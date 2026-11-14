package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.HashSet;
import org.jed.storage.*;

/** Packed byte store with canonical high-water accounting and watermark-gated RAM reclamation. */
final class MemoryDomain implements AutoCloseable {
    private Pager pager;
    private final int pageSize;
    private final IdentityHashMap<Engine.Snapshot,Integer> pins = new IdentityHashMap<>();
    private Engine.Snapshot snapshot = Engine.Snapshot.empty();
    private long high = 2, catalogRoot, lastCompactedLive;
    private int lastCatalogPages;
    private boolean spilled;
    private long[] free = new long[0];
    private Set<Long> catalogPages = Set.of();

    MemoryDomain(int pageSize) throws IOException { this(pageSize, true); }
    MemoryDomain(int pageSize, boolean mainDomain) throws IOException {
        this.pageSize = pageSize;
        pager = new Pager(new MemoryBlockStore(pageSize), pageSize, 0, true);
        var initial = CatalogChain.prepare(pageSize, List.of(), new PageAllocator(2, new long[0]), 0, pager, 0, 2);
        high = initial.allocator().highWater(); catalogRoot = initial.root(); lastCompactedLive = mainDomain ? high : 0;
        catalogPages = initial.writes().stream().map(ManifestEncoder.Write::page).collect(java.util.stream.Collectors.toUnmodifiableSet());
        pager.reserve(high);
        for (var write : initial.writes()) pager.writePage(write.page(), write.bytes());
    }
    Engine.Snapshot compactImage() throws IOException {
        synchronized (pins) { if (!pins.isEmpty()) throw new SqlException("55006", "database is in use"); }
        MemoryBlockStore output = new MemoryBlockStore(pageSize); Pager replacement = null;
        try {
            var image = ImageWriter.write(snapshot, pageSize, snapshot.version() + 1, PageCodec.PLAINTEXT, null,
                (page, bytes) -> output.writeAt(page * pageSize, bytes));
            output.setSize(image.pageCount() * pageSize);
            replacement = new Pager(output, pageSize, 0, true);
            var meta = PlaintextRecovery.parseMeta(image.firstMeta());
            var loaded = CatalogCodec.read(replacement, meta);
            var pages = new HashSet<Long>();
            for (long page = meta.root(); page != 0;) { if (!pages.add(page)) throw new CorruptStorageException("cyclic image catalog"); page = replacement.readBody(page).header().next(); }
            pager.close(); pager = replacement; replacement = null; snapshot = loaded; high = image.pageCount();
            catalogRoot = meta.root(); catalogPages = pages; lastCatalogPages = pages.size(); free = new long[0]; lastCompactedLive = high;
            return snapshot;
        } catch (IOException | RuntimeException | Error failure) { if (replacement != null) replacement.close(); else if (pager != replacement) output.close(); throw failure; }
    }
    Engine.Snapshot snapshot() { return snapshot; }
    long storageBytes() { return high * pageSize; }
    int pageSize() { return pageSize; }
    AutoCloseable pin(Engine.Snapshot value) {
        synchronized (pins) { pins.merge(value, 1, Integer::sum); }
        return () -> { synchronized (pins) { int count = pins.get(value); if (count == 1) pins.remove(value); else pins.put(value, count - 1); } };
    }
    long oldestLive() { synchronized (pins) { long oldest = snapshot.version(); for (var pin : pins.keySet()) oldest = Math.min(oldest, pin.version()); return oldest; } }
    private boolean reclaimable(long version) {
        synchronized (pins) { for (var pin : pins.keySet()) if (pin.version() < version) return false; }
        return true;
    }
    private record Plan(PageAllocator allocator, List<ManifestEncoder.Write> writes,
                        Map<String,PagePointer> roots, CatalogChain.Prepared catalog) {}
    record Staged(Engine.Snapshot before, Engine.Snapshot after, Plan plan) {}

    Engine.Snapshot commit(Engine.Snapshot next, long limit) throws IOException {
        if (next == snapshot) return snapshot;
        return adopt(stage(next, limit));
    }
    void precheck(Engine.Snapshot next, long limit) throws IOException { if (next != snapshot) admittedPlan(next, limit); }
    private Plan admittedPlan(Engine.Snapshot next, long limit) throws IOException {
        Plan plan = plan(next);
        boolean repair = next.tables().values().stream().noneMatch(table -> table.rows().hasStagedRecords()
            || table.indexes().stream().anyMatch(index -> index.entries().hasStagedRecords()))
            && plan.catalog.writes().size() <= lastCatalogPages;
        if (!fits(plan, limit) && snapshot.version() != 0 && reclaimable(snapshot.version())) {
            int before = free.length;
            compact();
            if (free.length > before) plan = plan(next);
        }
        if (!fits(plan, limit) && !repair) throw StorageLimit.exceeded(limit);
        return plan;
    }
    /** Writes only free or fresh pages; adoption waits until every domain's durable work succeeds. */
    Staged stage(Engine.Snapshot next, long limit) throws IOException {
        if (next == snapshot) return new Staged(snapshot, snapshot, null);
        Plan plan = admittedPlan(next, limit);
        long txid = snapshot.version() + 1, newHigh = plan.allocator.highWater();
        pager.reserve(newHigh);
        for (var write : plan.writes) pager.writePage(write.page(), write.bytes());
        Map<String,Engine.Table> tables = new HashMap<>();
        for (var table : next.tables().values()) {
            PagePointer pointer = plan.roots.get(table.name());
            var rows = PersistentTree.open(pager, pointer.page(), table.rows().size(), newHigh, List.of(),
                new TableLeafCodec(table.types(), table.primaryKey(), pager, newHigh), pointer.tag()).retainCommittedRoot(table.rows());
            List<Engine.Index> indexes = new ArrayList<>();
            for (var index : table.indexes()) {
                pointer = plan.roots.get(index.name());
                if (index.kind() == 2) {
                    var loaded = GistTree.open(pager, pointer, newHigh, index.keys().stream().map(Engine.IndexKey::type).toList());
                    indexes.add(new Engine.Index(index.name(), index.keys(), index.unique(), index.constraint(), index.predicate(),
                        2, loaded.entries(), index.timezoneDependencies(), loaded.tree(), index.hostDependencies()));
                } else indexes.add(index.changed(PersistentTree.openIndex(pager, pointer.page(), newHigh, List.of(), TableLeafCodec.index(), pointer.tag())));
            }
            tables.put(table.name(), table.changed(rows, table.nextRowId()).withIndexes(indexes).withStatistics(table.statistics()));
        }
        var after = new Engine.Snapshot(tables, next.types(), next.views(), next.sequences(), txid, next.collations()).withTempActions(next.tempActions());
        return new Staged(snapshot, after, plan);
    }
    /** Scratch migration is staged before any domain publishes, so I/O failure commits nothing. */
    void prepareTempSpill(Staged staged, java.nio.file.Path directory, long buffers,
                          java.util.function.Consumer<byte[]> entropy) throws IOException {
        prepareTempSpill(staged, directory, buffers, entropy, ScratchHost.NATIVE);
    }
    void prepareTempSpill(Staged staged, java.nio.file.Path directory, long buffers,
                          java.util.function.Consumer<byte[]> entropy, ScratchHost host) throws IOException {
        if (spilled) { pager.setCacheBytes(buffers); return; }
        if (directory == null || buffers == 0 || staged.plan == null || staged.plan.allocator.highWater() <= buffers / pageSize) return;
        BlockStore device = null; boolean adopted = false;
        try {
            device = host.create(directory, true);
            if (entropy != null) device = new ScratchDevice(device, entropy);
            pager.migrateStore(device, buffers); adopted = true; spilled = true;
        } finally {
            if (!adopted && device != null) device.close();
        }
    }
    boolean spilled() { return spilled; }
    long residentPages() { return pager.residentPages(); }
    Engine.Snapshot adopt(Staged staged) {
        if (staged.before != snapshot) throw new IllegalStateException("staged memory snapshot is stale");
        if (staged.plan == null) return snapshot;
        Plan plan = staged.plan;
        snapshot = staged.after; high = plan.allocator.highWater(); free = plan.allocator.remainingPages(); catalogRoot = plan.catalog.root();
        catalogPages = plan.catalog.writes().stream().map(ManifestEncoder.Write::page).collect(java.util.stream.Collectors.toUnmodifiableSet());
        lastCatalogPages = plan.catalog.writes().size();
        if (reclaimable(snapshot.version()) && high - free.length > ReclamationSchedule.COMPACT_MIN_PAGES
            && high - free.length > ReclamationSchedule.COMPACT_GROWTH * lastCompactedLive) {
            // Publication succeeded. A failed maintenance walk cannot turn that into a failed commit.
            try { compact(); } catch (IOException ignored) { }
        }
        return snapshot;
    }
    private boolean fits(Plan plan, long limit) {
        long after = plan.allocator.highWater();
        return limit <= 0 || after <= high || after <= limit / pageSize;
    }
    private Plan plan(Engine.Snapshot next) throws IOException {
        PageAllocator allocator = new PageAllocator(high, free);
        List<ManifestEncoder.Write> writes = new ArrayList<>();
        Map<String,PagePointer> roots = new HashMap<>();
        long txid = snapshot.version() + 1;
        for (var table : CatalogCodec.ordered(next)) {
            var prepared = table.rows().prepare(allocator, txid, List.of(), true, PageCodec.PLAINTEXT);
            allocator = prepared.allocator(); writes.addAll(prepared.writes());
            roots.put(table.name(), new PagePointer(prepared.root(), prepared.rootTag()));
            for (var index : CatalogCodec.orderedIndexes(table)) {
                if (index.kind() == 2) {
                    var gist = GistTree.build(index.keys().stream().map(Engine.IndexKey::type).toList(), index.entries(), index.gist());
                    var tree = gist.prepare(allocator, txid, pager.layout(), PageCodec.PLAINTEXT);
                    allocator = tree.allocator(); writes.addAll(tree.writes()); roots.put(index.name(), tree.root());
                } else {
                    var tree = index.entries().prepare(allocator, txid, List.of(), true, PageCodec.PLAINTEXT);
                    allocator = tree.allocator(); writes.addAll(tree.writes());
                    roots.put(index.name(), new PagePointer(tree.root(), tree.rootTag()));
                }
            }
        }
        List<byte[]> entries = new ArrayList<>();
        for (var type : CatalogCodec.orderedTypes(next)) entries.add(CatalogCodec.definition(type));
        for (var sequence : CatalogCodec.orderedSequences(next)) entries.add(CatalogCodec.sequence(sequence));
        for (var collation : CatalogCodec.orderedCollations(next)) entries.add(CatalogCodec.collation(collation));
        for (var table : CatalogCodec.ordered(next)) entries.add(CatalogCodec.table(table, roots.get(table.name()).page(), roots.get(table.name()).tag(), roots));
        entries.addAll(CatalogCodec.statistics(next));
        for (var view : CatalogCodec.orderedViews(next)) entries.add(CatalogCodec.view(view));
        var catalog = CatalogChain.prepare(pageSize, entries, allocator, txid, pager, catalogRoot, high, PageCodec.PLAINTEXT, new byte[0], true);
        writes.addAll(catalog.writes());
        return new Plan(catalog.allocator(), List.copyOf(writes), roots, catalog);
    }
    private void compact() throws IOException {
        Set<Long> live = new HashSet<>(catalogPages);
        for (var table : snapshot.tables().values()) {
            live.addAll(table.rows().physicalPages());
            for (var index : table.indexes()) live.addAll(index.physicalPages());
        }
        long[] pages = new long[Math.toIntExact(high - 2 - live.size())]; int count = 0;
        for (long page = 2; page < high; page++) if (!live.contains(page)) pages[count++] = page;
        free = pages; lastCompactedLive = live.size();
    }
    public void close() throws IOException { pager.close(); }
}
