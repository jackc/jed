package org.jed.storage;

import java.io.ByteArrayOutputStream;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

/** Shared overflow slabs, exact rewrite, and bounded cycle/length rejection. */
public final class OverflowChainTest {
    public static void main(String[] args) throws Exception {
        Path directory = Path.of(args.length == 0 ? "spec/fileformat/fixtures" : args[0]);
        int chains = 0;
        for (String name : List.of("overflow_table", "compressed_table")) {
            try (var store = FileBlockStore.open(directory.resolve(name + ".jed"), false)) {
                var meta = PlaintextRecovery.recover(store).meta();
                try (var pager = new Pager(store, meta.pageSize(), meta.pageSize(), false)) {
                    Set<Long> slabs = new HashSet<>(), successors = new HashSet<>();
                    for (long id = 2; id < meta.pageCount(); id++) {
                        var header = pager.readBody(id).header();
                        if (header.type() == 4) { slabs.add(id); if (header.next() != 0) successors.add(header.next()); }
                    }
                    for (long first : slabs.stream().filter(id -> !successors.contains(id)).sorted().toList()) {
                        long length = 0; List<Long> ids = new ArrayList<>();
                        for (long id = first; id != 0;) { var header = pager.readBody(id).header(); length += header.count(); ids.add(id); id = header.next(); }
                        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
                        try (var cursor = OverflowChain.cursor(pager, first, length, meta.pageCount())) {
                            java.nio.ByteBuffer part;
                            while ((part = cursor.next()) != null) { byte[] slab = new byte[part.remaining()]; part.get(slab); bytes.writeBytes(slab); }
                        }
                        var planned = OverflowChain.prepare(meta.pageSize(), bytes.toByteArray(), new PageAllocator(meta.pageCount(), ids.stream().mapToLong(Long::longValue).toArray()), meta.txid());
                        for (var write : planned.writes()) check(Arrays.equals(write.bytes(), pager.readBody(write.page()).copyBytes()), "shared overflow bytes differ");
                        chains++;
                    }
                }
            }
        }
        check(chains > 0, "no shared overflow chains found");
        try (var store = new MemoryBlockStore(); var pager = new Pager(store, 256, 256, false)) {
            var prepared = OverflowChain.prepare(256, new byte[500], new PageAllocator(3, new long[0]), 1);
            store.setSize(prepared.allocator().highWater() * 256);
            for (var write : prepared.writes()) store.writeAt(write.page() * 256, write.bytes());
            byte[] bad = prepared.writes().getFirst().bytes(); PageFormat.put32(bad, 8, prepared.firstPage()); PageFormat.put32(bad, 12, PageFormat.bodyCrc(bad));
            store.writeAt(prepared.firstPage() * 256, bad);
            try (var cursor = OverflowChain.cursor(pager, prepared.firstPage(), 500, prepared.allocator().highWater())) {
                try { while (cursor.next() != null) { } throw new AssertionError("accepted overflow cycle"); }
                catch (CorruptStorageException expected) { }
            }
        }
        System.out.println("OverflowChainTest: passed " + chains + " shared chains and bounded cycle rejection");
    }
    private static void check(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
}
