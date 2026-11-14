package org.jed;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import org.jed.storage.*;

/** Target-layout rebuild with bounded page residency and handle-private scratch storage. */
final class RekeyScratch implements AutoCloseable {
    private final Pager pager;
    private final long cacheBytes;
    private final java.util.function.Consumer<byte[]> entropy;
    private long high;
    private boolean spilled;

    RekeyScratch(int pageSize, boolean encrypted, long cacheBytes, java.util.function.Consumer<byte[]> entropy) throws IOException {
        this.cacheBytes = cacheBytes; this.entropy = entropy;
        PageCodec codec = PageCodec.PLAINTEXT;
        if (encrypted) {
            byte[] key = new byte[32]; entropy.accept(key);
            codec = new EncryptedPageCodec(key, entropy); java.util.Arrays.fill(key, (byte) 0);
        }
        pager = new Pager(new MemoryBlockStore(pageSize), pageSize, cacheBytes, false, codec);
        high = pager.layout().firstBody(); pager.reserve(high);
    }

    Engine.Snapshot rebuild(Engine.Snapshot source) throws IOException {
        var tables = new HashMap<String,Engine.Table>();
        for (var table : CatalogCodec.ordered(source)) {
            PersistentTree rows = PersistentTree.empty(pager.pageSize(), new TableLeafCodec(table.types(), table.primaryKey()), pager.codec());
            try (var cursor = table.rows().cursor()) {
                PersistentTree.Entry entry;
                while ((entry = cursor.next()) != null) {
                    // Force every value under the old snapshot before choosing the new disposition.
                    List<SqlValue> values = new ArrayList<>(Encoding.row(entry, table.types()));
                    rows = rows.put(Encoding.entry(entry.key(), values, null, null, pager.layout(), null, table.primaryKey()));
                    rows = save(rows, table.types(), table.primaryKey());
                }
            }
            var indexes = new ArrayList<Engine.Index>();
            for (var index : CatalogCodec.orderedIndexes(table)) {
                PersistentTree tree = PersistentTree.empty(pager.pageSize(), TableLeafCodec.index(), pager.codec());
                try (var cursor = index.entries().cursor()) {
                    PersistentTree.Entry entry;
                    while ((entry = cursor.next()) != null) { tree = tree.put(entry); tree = save(tree, null, null); }
                }
                indexes.add(index.changed(tree));
            }
            tables.put(table.name(), table.changed(rows, table.nextRowId()).withIndexes(indexes).withStatistics(table.statistics()));
        }
        return new Engine.Snapshot(tables, source.types(), source.views(), source.sequences(), source.version(), source.collations());
    }

    private PersistentTree save(PersistentTree tree, List<SqlType> types, int[] key) throws IOException {
        var prepared = tree.prepare(new PageAllocator(high, new long[0]), 1, List.of(), true, pager.codec());
        high = prepared.allocator().highWater();
        if (!spilled && high * pager.pageSize() > cacheBytes) {
            Path path = SpillRows.create(Path.of(System.getProperty("java.io.tmpdir")));
            BlockStore device = null; boolean adopted = false;
            try {
                device = new ScratchDevice(FileBlockStore.openScratch(path), entropy); Files.delete(path);
                pager.migrateStore(device, cacheBytes); adopted = true; spilled = true;
            } finally { if (!adopted && device != null) device.close(); Files.deleteIfExists(path); }
        }
        pager.reserve(high);
        for (var write : prepared.writes()) pager.writePage(write.page(), write.bytes());
        LeafCodec codec = types == null ? TableLeafCodec.index() : new TableLeafCodec(types, key, pager, high);
        return PersistentTree.open(pager, prepared.root(), tree.size(), high, List.of(), codec, prepared.rootTag());
    }
    public void close() throws IOException { pager.close(); }
}
