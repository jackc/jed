package org.jed.storage;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Random;
import java.util.TreeMap;

/** Internal snapshot/order/rebalance invariants; format-specific leaf goldens test the real codec. */
public final class PersistentTreeTest {
    public static void main(String[] args) throws Exception {
        var decodes = new java.util.concurrent.atomic.AtomicInteger();
        LeafCodec measured = new LeafCodec() {
            public long payloadSize(List<PersistentTree.Entry> entries) {
                long size = 0;
                for (var e : entries) size += 4 + e.key().length + e.value().length;
                return size;
            }
            public byte[] encode(List<PersistentTree.Entry> entries) {
                ByteBuffer bytes = ByteBuffer.allocate((int) payloadSize(entries));
                for (var entry : entries) {
                    byte[] key = entry.key(), value = entry.value();
                    bytes.putShort((short) key.length).putShort((short) value.length).put(key).put(value);
                }
                return bytes.array();
            }
            public List<PersistentTree.Entry> decode(ByteBuffer bytes, int count) {
                decodes.incrementAndGet();
                List<PersistentTree.Entry> entries = new ArrayList<>();
                for (int i = 0; i < count; i++) {
                    byte[] key = new byte[Short.toUnsignedInt(bytes.getShort())];
                    byte[] value = new byte[Short.toUnsignedInt(bytes.getShort())];
                    bytes.get(key).get(value);
                    entries.add(new PersistentTree.Entry(key, value));
                }
                while (bytes.hasRemaining()) check(bytes.get() == 0, "test leaf tail");
                return entries;
            }
        };
        PersistentTree tree = PersistentTree.empty(256, measured);
        TreeMap<Integer, byte[]> model = new TreeMap<>();
        Random random = new Random(927461);
        List<PersistentTree> snapshots = new ArrayList<>();
        List<TreeMap<Integer, byte[]>> expected = new ArrayList<>();
        for (int i = 0; i < 3000; i++) {
            int id = random.nextInt(300);
            if (random.nextInt(4) == 0) {
                tree = tree.remove(key(id)); model.remove(id);
            } else {
                byte[] value = new byte[1 + random.nextInt(160)];
                random.nextBytes(value);
                tree = tree.put(key(id), value); model.put(id, value);
            }
            if (i % 300 == 0) { snapshots.add(tree); expected.add(new TreeMap<>(model)); }
            verify(tree, model);
        }
        for (int i = 0; i < snapshots.size(); i++) verify(snapshots.get(i), expected.get(i));
        for (int id : new ArrayList<>(model.keySet())) {
            tree = tree.remove(key(id)); model.remove(id); verify(tree, model);
        }
        check(tree.size() == 0 && tree.cursor().next() == null, "empty root collapse");
        // Ascending inserts select right-edge split points and exercise growing roots.
        for (int id = 0; id < 1500; id++) {
            byte[] value = new byte[30];
            tree = tree.put(key(id), value); model.put(id, value);
        }
        verify(tree, model);
        ranges(tree, model);
        try { tree.put(key(2000), new byte[400]); throw new AssertionError("oversized record accepted"); }
        catch (TreeCapacityException expectedFailure) { verify(tree, model); }
        try (MemoryBlockStore memory = new MemoryBlockStore(256);
             LeafGuard store = new LeafGuard(memory);
             Pager pager = new Pager(store, 256, 256, false)) {
            var prepared = tree.prepare(new PageAllocator(2, new long[0]), 1);
            check(prepared.writes().size() == tree.scanPageCount(), "bootstrap writes every logical page");
            install(pager, prepared);
            PageAllocator allocator = prepared.allocator();
            tree = PersistentTree.open(pager, prepared.root(), tree.size(), allocator.highWater(), prepared.remaps(), measured);
            pager.invalidateAll();
            int priorDecodes = decodes.get();
            tree.get(key(600)); tree.get(key(600));
            check(decodes.get() == priorDecodes + 1, "resident leaf decoded repeatedly");
            tree.get(key(1499)); tree.get(key(600));
            check(decodes.get() == priorDecodes + 3 && pager.residentPages() == 1,
                "decoded leaves must follow the bounded page pool");
            LeafCodec differentSchema = new LeafCodec() {
                public long payloadSize(List<PersistentTree.Entry> entries) throws IOException { return measured.payloadSize(entries); }
                public byte[] encode(List<PersistentTree.Entry> entries) throws IOException { return measured.encode(entries); }
                public List<PersistentTree.Entry> decode(ByteBuffer bytes, int count) throws IOException {
                    throw new CorruptStorageException("different schema rejects this leaf");
                }
            };
            PersistentTree incompatible = PersistentTree.open(pager, prepared.root(), tree.size(), allocator.highWater(), prepared.remaps(), differentSchema);
            tree.get(key(0));
            try { incompatible.get(key(0)); throw new AssertionError("decoded leaf bypassed schema validation"); }
            catch (CorruptStorageException expectedFailure) { }
            pager.invalidateAll(); store.forbidLeaves = true;
            try {
                check(tree.structuralFacts().nodeCount() == prepared.writes().size(), "skeleton node count");
                tree.rangePageCount(key(2), true, key(1200), true);
                tree.pointPageCount(key(600));
            } finally { store.forbidLeaves = false; }
            verify(tree, model);
            ranges(tree, model);
            var facts = tree.structuralFacts();
            check(facts.nodeCount() == tree.scanPageCount() && facts.leafCount() > 0 && facts.height() > 1, "structural facts");
            long count = tree.scanPageCount();
            boolean folded = false;
            for (int update = 0; update < 30; update++) {
                int id = update * 47;
                byte[] value = new byte[30]; Arrays.fill(value, (byte) (update + 1));
                PersistentTree old = tree;
                byte[] before = old.get(key(id));
                tree = tree.put(key(id), value); model.put(id, value);
                prepared = tree.prepare(allocator, update + 2);
                if (update == 0) check(prepared.writes().size() == 1 && prepared.remaps().size() == 1,
                    "only changed leaf written with deferred interior");
                if (prepared.remaps().isEmpty()) { folded = true; check(prepared.writes().size() > 1, "fold writes stale interiors"); }
                install(pager, prepared);
                allocator = prepared.allocator();
                tree = PersistentTree.open(pager, prepared.root(), tree.size(), allocator.highWater(), prepared.remaps(), measured);
                check(Arrays.equals(old.get(key(id)), before), "old disk snapshot preserved");
                check(tree.scanPageCount() == count, "same-shape update page cost");
                verify(tree, model);
            }
            check(folded, "remap region capacity triggers fold");
        }
        System.out.println("PersistentTreeTest passed (3000 mixed mutations, retained snapshots, 1500 ascending inserts)");
    }

    private static void ranges(PersistentTree tree, TreeMap<Integer, byte[]> model) throws Exception {
        Random random = new Random(239);
        for (int n = 0; n < 100; n++) {
            int lower = random.nextInt(1700), upper = random.nextInt(1700);
            boolean includeLower = random.nextBoolean(), includeUpper = random.nextBoolean();
            List<Integer> expected = model.keySet().stream().filter(id -> (id > lower || includeLower && id == lower)
                && (id < upper || includeUpper && id == upper)).toList();
            List<Integer> actual = new ArrayList<>();
            try (var cursor = tree.cursor(key(lower), includeLower, key(upper), includeUpper)) {
                PersistentTree.Entry entry;
                while ((entry = cursor.next()) != null) actual.add(ByteBuffer.wrap(entry.key()).getInt());
            }
            check(actual.equals(expected), "bounded cursor differs");
            actual.clear();
            try (var cursor = tree.reverseCursor(key(lower), includeLower, key(upper), includeUpper)) {
                PersistentTree.Entry entry;
                while ((entry = cursor.next()) != null) actual.add(ByteBuffer.wrap(entry.key()).getInt());
            }
            check(actual.equals(expected.reversed()), "bounded reverse cursor differs");
            long pages = tree.rangePageCount(key(lower), includeLower, key(upper), includeUpper);
            check(pages <= tree.scanPageCount(), "bounded page count exceeds full scan");
            if (lower > upper || lower == upper && (!includeLower || !includeUpper)) check(pages == 0, "empty range costs pages");
        }
        for (int id : new int[]{0, 10, 9999}) check(tree.pointPageCount(key(id)) == tree.rangePageCount(key(id), true, key(id), true), "point and singleton range page counts differ");
    }

    private static final class LeafGuard implements BlockStore {
        private final BlockStore delegate; boolean forbidLeaves;
        LeafGuard(BlockStore delegate) { this.delegate = delegate; }
        public byte[] readAt(long offset, int length) throws IOException {
            byte[] bytes = delegate.readAt(offset, length);
            if (forbidLeaves && PageFormat.parseBody(bytes).type() == 2) throw new AssertionError("planning read a leaf page");
            return bytes;
        }
        public void writeAt(long offset, byte[] bytes) throws IOException { delegate.writeAt(offset, bytes); }
        public void sync() throws IOException { delegate.sync(); }
        public long size() throws IOException { return delegate.size(); }
        public void setSize(long size) throws IOException { delegate.setSize(size); }
        public void close() { }
    }
    private static byte[] key(int value) { return ByteBuffer.allocate(4).putInt(value).array(); }
    private static void install(Pager pager, PersistentTree.Prepared prepared) throws IOException {
        pager.reserve(prepared.allocator().highWater());
        for (var write : prepared.writes()) pager.writePage(write.page(), write.bytes());
    }
    private static void verify(PersistentTree tree, TreeMap<Integer, byte[]> model) throws IOException {
        check(tree.size() == model.size(), "row count");
        try (var cursor = tree.reverseCursor()) {
            for (var expected : model.descendingMap().entrySet()) {
                var actual = cursor.next();
                check(actual != null && Arrays.equals(actual.key(), key(expected.getKey()))
                    && Arrays.equals(actual.value(), expected.getValue()), "reverse cursor");
            }
            check(cursor.next() == null, "reverse cursor end");
        }
        try (var cursor = tree.cursor()) {
            for (var expected : model.entrySet()) {
                var actual = cursor.next();
                check(actual != null && Arrays.equals(actual.key(), key(expected.getKey()))
                    && Arrays.equals(actual.value(), expected.getValue()), "ordered cursor");
                check(Arrays.equals(tree.get(key(expected.getKey())), expected.getValue()), "lookup");
            }
            check(cursor.next() == null, "cursor end");
        }
    }
    private static void check(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
}
