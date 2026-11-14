package org.jed.storage;

import java.util.Arrays;
import java.util.List;

public final class CatalogChainTest {
    public static void main(String[] args) throws Exception {
        byte[] large = new byte[481];
        for (int i = 0; i < large.length; i++) large[i] = (byte) (i % 253 + 1);
        List<byte[]> entries = List.of(new byte[] {1, 2, 3}, large, new byte[] {4, 5});
        var prepared = CatalogChain.prepare(256, entries, new PageAllocator(2, new long[0]), 1, null, 0, 0);
        check(prepared.writes().size() == 5, "long entry occupies fresh continuation pages");
        try (MemoryBlockStore memory = new MemoryBlockStore(256);
             Pager pager = new Pager(memory, 256, 256, false)) {
            pager.reserve(prepared.allocator().highWater());
            for (var write : prepared.writes()) pager.writePage(write.page(), write.bytes());
            try (var cursor = CatalogChain.cursor(pager, prepared.root(), prepared.allocator().highWater())) {
                var first = cursor.next();
                check(first.itemCount() == 1 && first.payload().get(0) == 1, "leading page");
                var continued = cursor.next();
                check(continued.itemCount() == 1 && continued.payload().remaining() == 720, "continued entry extent");
                byte[] decoded = new byte[large.length]; continued.payload().get(decoded);
                check(Arrays.equals(large, decoded), "continued bytes");
                var tail = cursor.next();
                check(tail.itemCount() == 1 && tail.payload().get(0) == 4, "entry after continuation starts fresh page");
                check(cursor.next() == null && cursor.visitedPages().size() == 5, "complete chain");
            }
            var same = CatalogChain.prepare(256, entries, prepared.allocator(), 2, pager,
                prepared.root(), prepared.allocator().highWater());
            check(same.root() == prepared.root() && same.writes().isEmpty() && same.retiredPages().isEmpty(), "unchanged catalog kept");
            large[1] = 99;
            var changed = CatalogChain.prepare(256, entries, prepared.allocator(), 2, pager,
                prepared.root(), prepared.allocator().highWater());
            check(changed.writes().size() == 5 && changed.retiredPages().size() == 5, "changed catalog COW chain");
            byte[] broken = prepared.writes().get(2).bytes();
            PageFormat.put32(broken, 4, 1);
            PageFormat.put32(broken, 12, PageFormat.bodyCrc(broken));
            pager.writePage(prepared.writes().get(2).page(), broken);
            try (var cursor = CatalogChain.cursor(pager, prepared.root(), prepared.allocator().highWater())) {
                cursor.next();
                try { cursor.next(); throw new AssertionError("continued count accepted"); }
                catch (CorruptStorageException expected) { }
            }
        }
        System.out.println("CatalogChainTest passed");
    }

    private static void check(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
}
