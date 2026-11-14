package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import org.jed.storage.*;

/** Canonical from-scratch image: node-preserving, table-by-table, bounded leaf traversal. */
final class ImageWriter {
    private ImageWriter() { }
    record Result(long pageCount, byte[] firstMeta, byte[] secondMeta) { }
    static Result write(Engine.Snapshot snapshot, int pageSize, long txid, PageCodec codec, byte[] keyBlock,
                        PersistentTree.ImageSink sink) throws IOException {
        if (txid == 0) throw new TreeCapacityException("transaction id exhausted");
        PageLayout layout = new PageLayout(pageSize, codec.encrypted());
        PageAllocator allocator = new PageAllocator(layout.firstBody(), new long[0]);
        Map<String,PagePointer> roots = new HashMap<>();
        for (var table : CatalogCodec.ordered(snapshot)) {
            var data = table.rows().writeImage(allocator, txid, codec, sink);
            allocator = data.allocator(); roots.put(table.name(), new PagePointer(data.root(), data.rootTag()));
            for (var index : CatalogCodec.orderedIndexes(table)) {
                if (index.kind() == 2) {
                    var tree = index.gist().writeImage(allocator, txid, layout, codec, sink);
                    allocator = tree.allocator(); roots.put(index.name(), tree.root());
                } else {
                    var tree = index.entries().writeImage(allocator, txid, codec, sink);
                    allocator = tree.allocator(); roots.put(index.name(), new PagePointer(tree.root(), tree.rootTag()));
                }
            }
        }
        List<byte[]> entries = new ArrayList<>();
        for (var type : CatalogCodec.orderedTypes(snapshot)) entries.add(CatalogCodec.definition(type));
        for (var sequence : CatalogCodec.orderedSequences(snapshot)) entries.add(CatalogCodec.sequence(sequence));
        for (var collation : CatalogCodec.orderedCollations(snapshot)) entries.add(CatalogCodec.collation(collation));
        for (var table : CatalogCodec.ordered(snapshot)) entries.add(CatalogCodec.table(table, roots.get(table.name()).page(), roots.get(table.name()).tag(), roots));
        entries.addAll(CatalogCodec.statistics(snapshot));
        for (var view : CatalogCodec.orderedViews(snapshot)) entries.add(CatalogCodec.view(view));
        var catalog = CatalogChain.prepare(pageSize, entries, allocator, txid, null, 0, 0, codec, new byte[layout.tagBytes()]);
        for (var page : catalog.writes()) sink.write(page.page(), page.bytes());
        long count = catalog.allocator().highWater();
        byte[] meta = ManifestEncoder.encode(layout, txid, catalog.root(), catalog.rootTag(), count, 0, new byte[layout.tagBytes()],
            count - layout.firstBody(), List.of(), new long[0], List.of()).meta();
        byte[] first = meta, second = meta;
        if (codec instanceof EncryptedPageCodec encryption) {
            if (keyBlock == null) throw new IOException("encrypted image has no key block");
            sink.write(2, keyBlock); sink.write(3, keyBlock);
            first = encryption.sealMeta(0, meta, List.of()); second = encryption.sealMeta(1, meta, List.of());
        }
        sink.write(0, first); sink.write(1, second);
        return new Result(count, first, second);
    }
}
