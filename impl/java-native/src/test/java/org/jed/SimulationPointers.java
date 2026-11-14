package org.jed;

import java.io.IOException;
import java.util.*;
import org.jed.storage.*;

/** Classifies authenticated live pointers for the shared encrypted replay oracle. */
final class SimulationPointers {
    record Facts(Map<String,List<Long>> kinds, Set<Long> dependencies, long otherVersion) { }
    static Facts inspect(FileDatabase database) throws IOException {
        try (var lease = database.backupLease()) {
            Pager pager = lease.pager(); var meta = lease.selected().meta();
            Map<String,Set<Long>> kinds = new TreeMap<>();
            chain(kinds, pager, meta.root(), meta.rootTag(), "catalog", "catalog_next");
            chain(kinds, pager, meta.freeListHead(), meta.freeTag(), "free", "free_next");
            for (long page = meta.manifestHead(); page != 0;) { add(kinds, "manifest", page); page = PageFormat.u32(pager.readRaw(page), 8); }
            for (var table : lease.snapshot().tables().values()) {
                tree(kinds, pager, table.rows());
                try (var cursor = table.rows().cursor()) { for (PersistentTree.Entry entry; (entry = cursor.next()) != null;) {
                    for (var cell : StoredRow.of(entry, table.types()).cells) if (cell.external())
                        chain(kinds, pager, cell.firstPage, cell.chainTag, cell.compressed() ? "overflow_lz4" : "overflow", "overflow_next");
                } }
                for (var index : table.indexes()) {
                    if (index.kind() != 2) tree(kinds, pager, index.entries());
                    else { long root = index.gist().pointer().page(); for (long page : index.gist().physicalPages()) add(kinds, page == root ? "root" : "gist_child", page); }
                }
            }
            long other = 0;
            for (int slot = 0; slot < 2; slot++) try { var candidate = PlaintextRecovery.parseMeta(pager.readRaw(slot)); if (candidate.txid() < meta.txid()) other = Math.max(other, candidate.txid()); } catch (CorruptStorageException ignored) { }
            Map<String,List<Long>> sorted = new TreeMap<>(); kinds.forEach((kind, pages) -> sorted.put(kind, pages.stream().sorted().toList()));
            return new Facts(Map.copyOf(sorted), lease.selected().protectedPages(), other);
        }
    }
    private static void tree(Map<String,Set<Long>> kinds, Pager pager, PersistentTree tree) throws IOException {
        long root = tree.rootPage();
        for (long page : tree.physicalPages()) {
            byte[] bytes = pager.readRaw(page); var header = pager.readBody(page, pager.codec().bodyTag(bytes)).header();
            if (header.type() == 2 || header.type() == 3) add(kinds, page == root ? "root" : "child", page);
        }
    }
    private static void chain(Map<String,Set<Long>> kinds, Pager pager, long page, byte[] tag, String first, String next) throws IOException {
        Set<Long> visited = new HashSet<>(); String kind = first;
        while (page != 0) {
            if (!visited.add(page)) throw new CorruptStorageException("cycle in replay pointer chain");
            add(kinds, kind, page); var block = pager.readBody(page, tag); page = block.header().next();
            tag = new byte[pager.layout().tagBytes()]; block.buffer().get(16, tag); kind = next;
        }
    }
    private static void add(Map<String,Set<Long>> kinds, String kind, long page) { kinds.computeIfAbsent(kind, ignored -> new TreeSet<>()).add(page); }
}
