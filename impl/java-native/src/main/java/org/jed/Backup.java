package org.jed;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import org.jed.host.LinuxNative;
import org.jed.storage.*;

/** Test-only host backup forms from one pinned committed generation. */
final class Backup {
    private Backup() { }
    static void write(Path destination, Engine.Snapshot snapshot, Pager pager, PlaintextRecovery.Selected selected,
                      int pageSize, PageCodec codec, byte[] keyBlock, boolean pageIdentical) throws IOException {
        write(FileHost.NATIVE, destination.toAbsolutePath().normalize(), snapshot, pager, selected, pageSize, codec, keyBlock, pageIdentical);
    }
    static void write(FileHost host, Path target, Engine.Snapshot snapshot, Pager pager, PlaintextRecovery.Selected selected,
                      int pageSize, PageCodec codec, byte[] keyBlock, boolean pageIdentical) throws IOException {
        Path temporary = Path.of(target + ".jedtmp");
        if (host.exists(target)) throw new SqlException("58P02", "backup destination already exists");
        if (pageIdentical && selected == null) throw new SqlException("0A000", "in-memory backup requires compacted form");
        boolean created = false, published = false;
        try {
            try (BlockStore output = host.create(temporary)) {
                created = true;
                if (pageIdentical) identical(snapshot, pager, selected, keyBlock, output);
                else {
                    var image = ImageWriter.write(snapshot, pageSize, snapshot.version() + 1, codec, keyBlock,
                        (page, bytes) -> output.writeAt(page * pageSize, bytes));
                    output.setSize(image.pageCount() * pageSize);
                }
                output.sync();
            }
            host.rename(temporary, target, false); published = true;
            host.syncDirectory(target.getParent());
        } catch (LinuxNative.NativeIOException failure) {
            if (failure.errno() == 17) throw new SqlException("58P02", "backup destination already exists");
            throw failure;
        } catch (java.nio.file.FileAlreadyExistsException failure) {
            throw new SqlException("58P02", "backup destination already exists");
        } finally {
            if (created && !published) host.delete(temporary);
        }
    }
    private static void identical(Engine.Snapshot snapshot, Pager pager, PlaintextRecovery.Selected selected,
                                  byte[] keyBlock, BlockStore output) throws IOException {
        var meta = selected.meta(); Set<Long> pages = new HashSet<>();
        try (var catalog = CatalogChain.cursor(pager, meta.root(), meta.pageCount(), meta.rootTag())) {
            while (catalog.next() != null) { }
            pages.addAll(catalog.visitedPages());
        }
        for (var table : snapshot.tables().values()) {
            add(pages, table.rows().physicalPages());
            // Verify every leaf's bound tag as well as its CRC, including fixed-only tables.
            try (var cursor = table.rows().cursor()) { while (cursor.next() != null) { } }
            for (var index : table.indexes()) {
                add(pages, index.physicalPages());
                if (index.kind() != 2) try (var cursor = index.entries().cursor()) { while (cursor.next() != null) { } }
            }
        }
        if (pages.size() != meta.livePages()) throw new CorruptStorageException("backup live-page count differs from reachability");
        for (long page : FreeList.read(pager, selected).chain()) if (!pages.add(page)) throw new CorruptStorageException("backup free-list overlaps live pages");
        output.setSize(meta.pageCount() * meta.pageSize());
        for (long page : pages.stream().mapToLong(Long::longValue).sorted().toArray()) {
            byte[] bytes = pager.readRaw(page);
            pager.codec().openBody(page, bytes, pager.codec().bodyTag(bytes));
            output.writeAt(page * meta.pageSize(), bytes);
        }
        byte[] bootstrap = ManifestEncoder.encode(meta.layout(), meta.txid(), meta.root(), meta.rootTag(), meta.pageCount(),
            meta.freeListHead(), meta.freeTag(), meta.livePages(), List.of(), new long[0], meta.remaps()).meta();
        if (pager.codec() instanceof EncryptedPageCodec encryption) {
            output.writeAt(2L * meta.pageSize(), keyBlock); output.writeAt(3L * meta.pageSize(), keyBlock);
            output.writeAt(0, encryption.sealMeta(0, bootstrap, List.of()));
            output.writeAt(meta.pageSize(), encryption.sealMeta(1, bootstrap, List.of()));
        } else { output.writeAt(0, bootstrap); output.writeAt(meta.pageSize(), bootstrap); }
    }
    private static void add(Set<Long> target, Set<Long> pages) throws CorruptStorageException {
        for (long page : pages) if (!target.add(page)) throw new CorruptStorageException("backup trees overlap");
    }
}
