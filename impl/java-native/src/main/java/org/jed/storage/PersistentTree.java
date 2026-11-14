package org.jed.storage;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;

/**
 * Immutable COW B+tree with schema-measured leaves and demand-loaded committed
 * pages. Raw unsigned key order is the sole comparator. No operation loads an
 * entire database, and cursors retain only their current descent path and leaf.
 */
public final class PersistentTree {
    public interface Record { byte[] bytes(); default boolean staged() { return true; } }
    private static final class RawRecord implements Record {
        private final byte[] bytes;
        RawRecord(byte[] bytes) { this.bytes = bytes.clone(); }
        public byte[] bytes() { return bytes.clone(); }
    }
    public static final class Entry {
        private final byte[] key;
        private final Record record;
        public Entry(byte[] key, byte[] value) { this(key, new RawRecord(value)); }
        public Entry(byte[] key, Record record) { this.key = key.clone(); this.record = java.util.Objects.requireNonNull(record); }
        public byte[] key() { return key.clone(); }
        public byte[] value() { return record.bytes(); }
        public Record record() { return record; }
    }

    private static final class Node {
        final List<Entry> rows;
        final List<byte[]> keys;
        final List<Ref> children;
        final Ref origin;
        final boolean sameKeys;
        volatile long encodedSize = -1;
        Node(List<Entry> rows) { this.rows = List.copyOf(rows); keys = null; children = null; origin = null; sameKeys = false; }
        Node(List<byte[]> keys, List<Ref> children) {
            this(keys, children, null);
        }
        Node(List<byte[]> keys, List<Ref> children, Ref origin) {
            this(keys, children, origin, false);
        }
        Node(List<byte[]> keys, List<Ref> children, Ref origin, boolean sameKeys) {
            this.keys = List.copyOf(keys); this.children = List.copyOf(children); rows = null; this.origin = origin; this.sameKeys = sameKeys;
        }
        boolean leaf() { return rows != null; }
        int count() { return leaf() ? rows.size() : keys.size(); }
    }

    private static final class Ref {
        final long page;
        final Node dirty;
        final byte[] tag;
        // Only interior structure is retained. Leaf residency belongs to Pager.
        volatile Node interior;
        Node residentRootLeaf;
        volatile int height;
        Ref(long page, Node dirty) { this(page, dirty, new byte[0]); }
        Ref(long page, Node dirty, byte[] tag) { this.page = page; this.dirty = dirty; this.tag = tag.clone(); }
        PagePointer pointer() { return new PagePointer(page, tag); }
    }

    private record PathStep(Ref ref, Node node, int child) { }
    private record Split(List<Ref> nodes, List<byte[]> separators) { }
    private record Point(int chosen, int largestLeft) { }

    public record Prepared(long root, PageAllocator allocator, List<ManifestEncoder.Write> writes,
                           List<PlaintextRecovery.Remap> remaps, Set<Long> retiredPages, byte[] rootTag) {
        public Prepared {
            allocator = allocator.copy(); writes = List.copyOf(writes);
            remaps = List.copyOf(remaps); retiredPages = Set.copyOf(retiredPages); rootTag = rootTag.clone();
        }
        @Override public PageAllocator allocator() { return allocator.copy(); }
        @Override public byte[] rootTag() { return rootTag.clone(); }
    }

    private record RemapKey(long parent, int slot) implements Comparable<RemapKey> {
        @Override public int compareTo(RemapKey other) {
            int order = Long.compare(parent, other.parent);
            return order != 0 ? order : Integer.compare(slot, other.slot);
        }
    }
    private final int pageSize;
    private final int capacity;
    private final PageLayout layout;
    private final PageCodec pageCodec;
    private final LeafCodec codec;
    private final Pager pager;
    private final long pageCount;
    private final List<PlaintextRecovery.Remap> remaps;
    private final Ref root;
    private final long size;
    private final Object lineage;
    private final Map<Long, Ref> retired;
    private final PendingBytes pending;

    private PersistentTree(int pageSize, LeafCodec codec, Pager pager, long pageCount,
                           List<PlaintextRecovery.Remap> remaps, Ref root, long size, Map<Long, Ref> retired, PageCodec pageCodec) {
        this(pageSize, codec, pager, pageCount, remaps, root, size, retired, pageCodec, new Object());
    }
    private PersistentTree(int pageSize, LeafCodec codec, Pager pager, long pageCount,
                           List<PlaintextRecovery.Remap> remaps, Ref root, long size, Map<Long, Ref> retired, PageCodec pageCodec, Object lineage) {
        this(pageSize, codec, pager, pageCount, remaps, root, size, retired, pageCodec, lineage, PendingBytes.EMPTY);
    }
    private PersistentTree(int pageSize, LeafCodec codec, Pager pager, long pageCount, List<PlaintextRecovery.Remap> remaps,
                           Ref root, long size, Map<Long, Ref> retired, PageCodec pageCodec, Object lineage, PendingBytes pending) {
        this.pending = pending;
        this.lineage = lineage;
        this.pageSize = pageSize; this.layout = new PageLayout(pageSize, pageCodec.encrypted()); this.capacity = layout.capacity(); this.codec = codec; this.pageCodec = pageCodec;
        this.pager = pager; this.pageCount = pageCount; this.remaps = List.copyOf(remaps);
        this.root = root; this.size = size; this.retired = Map.copyOf(retired);
    }

    public static PersistentTree empty(int pageSize, LeafCodec codec) {
        return empty(pageSize, codec, PageCodec.PLAINTEXT);
    }
    public static PersistentTree empty(int pageSize, LeafCodec codec, PageCodec pageCodec) {
        return new PersistentTree(pageSize, codec, null, 0, List.of(), null, 0, Map.of(), pageCodec);
    }

    /** Catalog row count is trusted after its checksummed, bounded decode. */
    public static PersistentTree open(Pager pager, long root, long rowCount, long pageCount,
                                     List<PlaintextRecovery.Remap> remaps, LeafCodec codec) throws IOException {
        return open(pager, root, rowCount, pageCount, remaps, codec, new byte[0]);
    }
    public static PersistentTree open(Pager pager, long root, long rowCount, long pageCount,
                                     List<PlaintextRecovery.Remap> remaps, LeafCodec codec, byte[] rootTag) throws IOException {
        if (rowCount < 0 || (root == 0) != (rowCount == 0) || (root != 0 && (root < pager.layout().firstBody() || root >= pageCount))) {
            throw new CorruptStorageException("invalid tree root or row count");
        }
        PersistentTree tree = new PersistentTree(pager.pageSize(), codec, pager, pageCount, remaps,
            root == 0 ? null : new Ref(root, null, rootTag), rowCount, Map.of(), pager.codec());
        tree.height(); return tree;
    }

    /** Index cardinality is absent from the catalog and stays unknown without scanning leaves. */
    public static PersistentTree openIndex(Pager pager, long root, long pageCount, List<PlaintextRecovery.Remap> remaps, LeafCodec codec, byte[] rootTag) throws IOException {
        if (root != 0 && (root < pager.layout().firstBody() || root >= pageCount)) throw new CorruptStorageException("invalid index root");
        PersistentTree tree = new PersistentTree(pager.pageSize(), codec, pager, pageCount, remaps, root == 0 ? null : new Ref(root, null, rootTag), root == 0 ? 0 : -1, Map.of(), pager.codec());
        tree.height(); return tree;
    }
    /** B4 keeps only an already-resident single root leaf after publication; every other leaf demotes. */
    public PersistentTree retainCommittedRoot(PersistentTree before) {
        if (root != null && before.root != null && root.height == 1) {
            Node resident = before.root.dirty != null && before.root.dirty.leaf() ? before.root.dirty : before.root.residentRootLeaf;
            if (resident != null) root.residentRootLeaf = new Node(codec.committedRecords(resident.rows));
        }
        return this;
    }
    /** Adopt a fully loaded interior skeleton onto the durable pager without any later publication-time read. */
    public PersistentTree publishedOn(Pager durable) throws IOException {
        structuralFacts();
        return new PersistentTree(pageSize, codec, durable, pageCount, remaps, root, size, Map.of(), durable.codec());
    }
    public java.util.OptionalLong knownSize() { return size < 0 ? java.util.OptionalLong.empty() : java.util.OptionalLong.of(size); }
    public boolean isEmpty() { return root == null; }
    public long size() { if (size < 0) throw new IllegalStateException("index cardinality is not persisted"); return size; }
    public int pageSize() { return pageSize; }
    public PageLayout layout() { return layout; }
    public PageCodec pageCodec() { return pageCodec; }
    public byte[] rootTag() { return root == null ? new byte[layout.tagBytes()] : root.tag.clone(); }

    /** Height is established during open; planning subsequently needs no leaf I/O. */
    public int height() throws IOException {
        if (root == null) return 0;
        List<Ref> path = new ArrayList<>(); Set<Long> seen = new HashSet<>(); Ref ref = root;
        while (ref.height == 0) {
            checkCycle(ref, seen);
            if (kind(ref) == 2) { ref.height = 1; break; }
            path.add(ref); ref = load(ref).children.getFirst();
        }
        int height = ref.height;
        for (int i = path.size() - 1; i >= 0; i--) {
            Ref parent = path.get(i); Node node = load(parent);
            for (Ref child : node.children) {
                if (child.height != 0 && child.height != height) throw new CorruptStorageException("mixed tree child heights");
                child.height = height;
            }
            parent.height = ++height;
        }
        return root.height;
    }
    public record StructuralFacts(long nodeCount, long leafCount, int height) {}
    public StructuralFacts structuralFacts() throws IOException {
        int height = height(); if (height == 0) return new StructuralFacts(0, 0, 0);
        List<Ref> pending = new ArrayList<>(); pending.add(root); Set<Long> seen = new HashSet<>(); long nodes = 0, leaves = 0;
        while (!pending.isEmpty()) {
            Ref ref = pending.removeLast(); checkCycle(ref, seen); nodes++;
            if (ref.height == 1) { leaves++; continue; }
            pending.addAll(load(ref).children);
        }
        return new StructuralFacts(nodes, leaves, height);
    }

    public long pointPageCount(byte[] key) throws IOException {
        if (root == null) return 0;
        long count = 0;
        Set<Long> ancestors = new HashSet<>();
        Ref ref = root;
        while (true) {
            checkCycle(ref, ancestors); count++;
            if (kind(ref) == 2) return count;
            Node node = load(ref);
            ref = node.children.get(child(node.keys, key));
        }
    }

    /** Logical bounded-scan page count, charging each visited interior and selected leaf once. */
    public long rangePageCount(byte[] lower, boolean lowerInclusive, byte[] upper, boolean upperInclusive) throws IOException {
        if (root == null || emptyRange(lower, lowerInclusive, upper, upperInclusive)) return 0;
        height();
        record Pending(Ref ref, int hint) { }
        List<Pending> pending = new ArrayList<>(); pending.add(new Pending(root, 0));
        Set<Long> seen = new HashSet<>(); long count = 0;
        while (!pending.isEmpty()) {
            Pending item = pending.removeLast(); Ref ref = item.ref; checkCycle(ref, seen); count++;
            if (item.hint == 2 || kind(ref) == 2) continue;
            Node node = load(ref);
            int first = lower == null ? 0 : child(node.keys, lower);
            int last = upper == null ? node.children.size() - 1 : upperInclusive ? child(node.keys, upper) : separatorLowerBound(node.keys, upper);
            int childKind = kind(node.children.get(first));
            for (int i = last; i >= first; i--) pending.add(new Pending(node.children.get(i), childKind));
        }
        return count;
    }

    private static boolean emptyRange(byte[] lower, boolean lowerInclusive, byte[] upper, boolean upperInclusive) {
        if (lower == null || upper == null) return false;
        int compared = compare(lower, upper);
        return compared > 0 || compared == 0 && (!lowerInclusive || !upperInclusive);
    }

    /** Only newly staged versions in dirty leaves count; clean packed subtrees cannot contain any. */
    public boolean hasStagedRecords() {
        if (root == null) return false;
        var pending = new java.util.ArrayDeque<Ref>(); pending.push(root);
        while (!pending.isEmpty()) { Ref ref = pending.pop(); if (ref.dirty == null) continue;
            if (ref.dirty.leaf()) { for (Entry entry : ref.dirty.rows) if (entry.record.staged()) return true; }
            else for (Ref child : ref.dirty.children) pending.push(child);
        }
        return false;
    }

    /** Committed root, or zero for an empty or not-yet-serialized root. */
    public long rootPage() { return root == null || root.dirty != null ? 0 : root.page; }

    public boolean sharesTreeHistory(PersistentTree other) { return lineage == other.lineage; }
    public boolean sharesBackingStore(PersistentTree other) { return pager != null && pager == other.pager; }

    /** The largest key, descending one bounded path without scanning table rows. */
    public byte[] lastKey() throws IOException {
        if (root == null) return null;
        Ref ref = root;
        Set<Long> ancestors = new HashSet<>();
        while (true) {
            checkCycle(ref, ancestors);
            Node node = load(ref);
            if (node.leaf()) return node.rows.getLast().key();
            ref = node.children.getLast();
        }
    }

    /** Logical full-scan page count: interior structure plus leaf ids, without reading every leaf. */
    public long scanPageCount() throws IOException {
        if (root == null) return 0;
        List<Ref> pending = new ArrayList<>();
        Set<Long> seen = new HashSet<>();
        pending.add(root);
        long count = 0;
        while (!pending.isEmpty()) {
            Ref ref = pending.removeLast();
            checkCycle(ref, seen);
            count++;
            if (kind(ref) == 2) continue;
            Node node = load(ref);
            if (kind(node.children.getFirst()) == 2) count += node.children.size();
            else pending.addAll(node.children);
        }
        return count;
    }
    /** Old physical nodes orphaned by this transaction; value overflow chains are the codec's concern. */
    public Set<Long> retiredPages() { return retired.keySet(); }

    /** Physical identities for wholesale retirement; only interior pages need decoding. */
    public Set<Long> physicalPages() throws IOException {
        Set<Long> result = new HashSet<>();
        List<Ref> pending = new ArrayList<>();
        if (root != null) pending.add(root);
        while (!pending.isEmpty()) {
            Ref ref = pending.removeLast();
            if (ref.page != 0 && !result.add(ref.page)) throw new CorruptStorageException("repeated tree page");
            if (kind(ref) == 2) {
                Node stored = null;
                if (codec.mayReferencePages()) {
                    if (ref.residentRootLeaf != null) { var page = pager.readBody(ref.page, ref.tag); stored = new Node(codec.decode(page.buffer().slice(16, capacity), (int) page.header().count())); }
                    else stored = load(ref);
                }
                if (stored != null) for (long page : codec.referencedPages(stored.rows)) {
                    if (!result.add(page)) throw new CorruptStorageException("repeated external value page");
                }
                continue;
            }
            Node node = load(ref);
            if (!codec.mayReferencePages() && kind(node.children.getFirst()) == 2) {
                for (Ref child : node.children) if (child.page != 0 && !result.add(child.page)) throw new CorruptStorageException("repeated tree leaf");
            } else pending.addAll(node.children);
        }
        return Set.copyOf(result);
    }

    /**
     * Pure post-order serialization. The allocator is copied so a remap-capacity
     * fold can restart allocation without consuming speculative page ids.
     * Retained interiors keep their physical page; only their child-slot remaps
     * change. A full remap region folds stale interiors and their ancestor paths.
     */
    public Prepared prepare(PageAllocator allocator, long txid) throws IOException {
        Prepared ordinary = prepare(allocator, txid, remaps, false);
        return ordinary.remaps.size() <= layout.remapCapacity()
            ? ordinary : prepare(allocator, txid, remaps, true);
    }

    /** Database-wide serializer supplies the evolving remap set and retries all trees when full. */
    public Prepared prepare(PageAllocator allocator, long txid, List<PlaintextRecovery.Remap> currentRemaps,
                            boolean fold) throws IOException {
        return prepare(allocator, txid, currentRemaps, fold, pageCodec);
    }
    public Prepared prepare(PageAllocator allocator, long txid, List<PlaintextRecovery.Remap> currentRemaps,
                            boolean fold, PageCodec encoder) throws IOException {
        return prepare(allocator, txid, currentRemaps, fold, encoder, 0);
    }
    public Prepared prepare(PageAllocator allocator, long txid, List<PlaintextRecovery.Remap> currentRemaps,
                            boolean fold, PageCodec encoder, long migratePage) throws IOException {
        if (encoder.encrypted() != layout.encrypted()) throw new IllegalArgumentException("tree encoder layout mismatch");
        Serializer serializer = new Serializer(allocator.copy(), txid, currentRemaps, fold, encoder);
        serializer.migratePage = fold ? 0 : migratePage;
        return serializer.run();
    }

    @FunctionalInterface public interface ImageSink { void write(long page, byte[] bytes) throws IOException; }
    /** Streams the same nodes in canonical post-order, relocating every page and overflow chain. */
    public Prepared writeImage(PageAllocator allocator, long txid, PageCodec encoder, ImageSink sink) throws IOException {
        if (encoder.encrypted() != layout.encrypted()) throw new IllegalArgumentException("tree encoder layout mismatch");
        return new Serializer(allocator.copy(), txid, List.of(), true, encoder, true, sink).run();
    }

    private final class Serializer {
        PageAllocator allocator;
        final long txid;
        long migratePage;
        final boolean fold;
        final boolean fresh;
        final ImageSink sink;
        final PageCodec encoder;
        final List<ManifestEncoder.Write> writes = new ArrayList<>();
        final TreeMap<RemapKey, PagePointer> mapping = new TreeMap<>();
        final Set<RemapKey> originalMappings = new HashSet<>();
        final Map<Long, Ref> dropped = new java.util.HashMap<>(retired);
        final Set<Long> keptExternal = new HashSet<>();

        Serializer(PageAllocator allocator, long txid, List<PlaintextRecovery.Remap> currentRemaps, boolean fold, PageCodec encoder) {
            this(allocator, txid, currentRemaps, fold, encoder, false, null);
        }
        Serializer(PageAllocator allocator, long txid, List<PlaintextRecovery.Remap> currentRemaps, boolean fold, PageCodec encoder, boolean fresh, ImageSink sink) {
            this.allocator = allocator; this.txid = txid; this.fold = fold; this.encoder = encoder; this.fresh = fresh; this.sink = sink;
            for (var remap : currentRemaps) {
                originalMappings.add(new RemapKey(remap.parent(), remap.slot()));
                if (!retired.containsKey(remap.parent())) mapping.put(new RemapKey(remap.parent(), remap.slot()), new PagePointer(remap.page(), remap.tag()));
            }
        }

        final class Frame {
            final Ref ref;
            final Node node;
            final PagePointer[] children;
            final int childKind;
            int next;
            Frame(Ref ref, Node node) throws IOException {
                this.ref = ref; this.node = node; children = new PagePointer[node.children.size()];
                childKind = kind(node.children.getFirst());
            }
        }

        Prepared run() throws IOException {
            if (root == null) return result(new PagePointer(0, new byte[layout.tagBytes()]));
            List<Frame> stack = new ArrayList<>();
            Set<Long> ancestors = new HashSet<>();
            Ref pending = root;
            int hint = pending.dirty != null ? (pending.dirty.leaf() ? 2 : 3) : 0;
            while (true) {
                PagePointer id;
                if (!fresh && pending.dirty == null && (!fold || hint == 2)) {
                    id = pending.pointer();
                } else {
                    Node node = load(pending);
                    if (node.leaf()) {
                        if (!fresh && pending.dirty == null) id = pending.pointer();
                        else id = writeLeaf(node);
                    } else {
                        checkCycle(pending, ancestors);
                        Frame frame = new Frame(pending, node);
                        stack.add(frame);
                        pending = node.children.getFirst(); hint = frame.childKind;
                        continue;
                    }
                }
                while (true) {
                    if (stack.isEmpty()) return result(id);
                    Frame parent = stack.getLast();
                    parent.children[parent.next++] = id;
                    if (parent.next < parent.children.length) {
                        pending = parent.node.children.get(parent.next); hint = parent.childKind;
                        break;
                    }
                    stack.removeLast();
                    if (parent.ref.page != 0) ancestors.remove(parent.ref.page);
                    id = writeInterior(parent);
                }
            }
        }

        Prepared result(PagePointer rootPage) throws IOException {
            if (fresh) return new Prepared(rootPage.page(), allocator, writes, List.of(), Set.of(), rootPage.tag());
            if (codec.mayReferencePages() && pager != null) {
                Set<Long> orphaned = new HashSet<>();
                for (var retiredRef : dropped.values()) {
                    Pager.Page old = pager.readBody(retiredRef.page, retiredRef.tag);
                    if (old.header().type() == 2) {
                        var entries = codec.decode(old.buffer().slice(16, capacity), (int) old.header().count());
                        orphaned.addAll(codec.referencedPages(entries));
                    }
                }
                orphaned.removeAll(keptExternal);
                for (long page : orphaned) dropped.putIfAbsent(page, new Ref(page, null));
            }
            List<PlaintextRecovery.Remap> result = new ArrayList<>();
            for (var entry : mapping.entrySet()) result.add(new PlaintextRecovery.Remap(entry.getKey().parent,
                entry.getKey().slot, entry.getValue().page(), entry.getValue().tag()));
            return new Prepared(rootPage.page(), allocator, writes, result, dropped.keySet(), rootPage.tag());
        }

        PagePointer writeLeaf(Node node) throws IOException {
            // Fresh images reserve the leaf before its chains; incremental commits
            // draw fresh chains first, then the leaf from the shared free allocator.
            long id = fresh ? allocator.allocate() : 0;
            LeafCodec.Prepared prepared = fresh ? codec.prepareImage(node.rows, allocator, txid, pageSize, encoder) : codec.prepare(node.rows, allocator, txid, pageSize, encoder);
            allocator = prepared.allocator(); for (var write : prepared.writes()) emit(write.page(), write.bytes());
            if (!fresh) id = allocator.allocate();
            if (!fresh) keptExternal.addAll(codec.referencedPages(node.rows));
            byte[] payload = prepared.payload();
            if (node.rows.isEmpty() || payload.length != codec.payloadSize(node.rows) || payload.length > capacity) {
                throw new TreeCapacityException("leaf codec size does not fit its planned page");
            }
            byte[] bytes = new byte[pageSize];
            bytes[0] = 2;
            PageFormat.put16(bytes, 2, (int) (txid & 0xffff));
            PageFormat.put32(bytes, 4, node.rows.size());
            System.arraycopy(payload, 0, bytes, 16, payload.length);
            byte[] sealed = encoder.sealBody(id, bytes);
            emit(id, sealed);
            return new PagePointer(id, encoder.bodyTag(sealed));
        }

        PagePointer writeInterior(Frame frame) throws IOException {
            Node node = frame.node;
            Ref original = fresh ? null : frame.ref.page != 0 ? frame.ref : node.origin;
            PagePointer[] rawChildren = null;
            boolean sameKeys = false;
            if (original != null) {
                Node old = load(original);
                // A separator edit invalidates the persisted slot identities for
                // this entire transaction, even if later edits restore its bytes.
                sameKeys = (frame.ref.page != 0 || node.sameKeys) && !old.leaf() && old.keys.size() == node.keys.size();
                if (sameKeys) for (int i = 0; i < node.keys.size(); i++) {
                    if (!Arrays.equals(old.keys.get(i), node.keys.get(i))) { sameKeys = false; break; }
                }
                if (sameKeys) {
                    rawChildren = new PagePointer[frame.children.length];
                    ByteBuffer bytes = pager.readBody(original.page, original.tag).buffer();
                    for (int i = 0; i < rawChildren.length; i++) rawChildren[i] = readPointer(bytes, 16 + i * layout.pointerBytes());
                }
            }
            if (sameKeys && original.page != migratePage && (!fold || !originalMappings.stream().anyMatch(key -> key.parent == original.page) && Arrays.equals(rawChildren, frame.children))) {
                for (int i = 0; i < frame.children.length; i++) {
                    RemapKey key = new RemapKey(original.page, i);
                    // A retained parent records every moved logical child. A reused
                    // page number can equal its stale on-disk pointer; that does not
                    // erase the edge's history or avoid a later canonical fold.
                    if (originalMappings.contains(key) || !rawChildren[i].equals(frame.children[i])) mapping.put(key, frame.children[i]);
                }
                dropped.remove(original.page);
                return original.pointer();
            }
            if (original != null) {
                dropped.put(original.page, original);
                long oldPage = original.page;
                mapping.keySet().removeIf(key -> key.parent == oldPage);
            }
            if (payload(node) > capacity) throw new TreeCapacityException("interior exceeds page capacity");
            byte[] bytes = new byte[pageSize];
            bytes[0] = 3;
            PageFormat.put16(bytes, 2, (int) (txid & 0xffff));
            PageFormat.put32(bytes, 4, node.keys.size());
            for (int i = 0; i < frame.children.length; i++) writePointer(bytes, 16 + i * layout.pointerBytes(), frame.children[i]);
            int directory = 16 + frame.children.length * layout.pointerBytes();
            int blob = directory + node.keys.size() * 4;
            int end = 0;
            for (int i = 0; i < node.keys.size(); i++) {
                byte[] key = node.keys.get(i);
                System.arraycopy(key, 0, bytes, blob + end, key.length);
                end += key.length;
                PageFormat.put32(bytes, directory + i * 4, end);
            }
            return append(bytes);
        }

        void emit(long page, byte[] bytes) throws IOException {
            if (sink == null) writes.add(new ManifestEncoder.Write(page, bytes)); else sink.write(page, bytes);
        }
        PagePointer append(byte[] bytes) throws IOException {
            long id = allocator.allocate();
            byte[] sealed = encoder.sealBody(id, bytes);
            emit(id, sealed);
            return new PagePointer(id, encoder.bodyTag(sealed));
        }
    }

    public byte[] get(byte[] key) throws IOException {
        Entry entry = getEntry(key);
        return entry == null ? null : entry.value();
    }

    public Entry getEntry(byte[] key) throws IOException {
        Ref ref = root;
        Set<Long> path = new HashSet<>();
        while (ref != null) {
            checkCycle(ref, path);
            Node node = load(ref);
            if (node.leaf()) {
                int at = lowerBound(node.rows, key);
                return at < node.rows.size() && compare(node.rows.get(at).key, key) == 0
                    ? node.rows.get(at) : null;
            }
            ref = node.children.get(child(node.keys, key));
        }
        return null;
    }

    /** Replaces an equal key; constraints are checked by the relational writer. */
    public PersistentTree put(byte[] key, byte[] value) throws IOException {
        return put(new Entry(key, value));
    }

    public PersistentTree put(Entry entry) throws IOException {
        byte[] key = entry.key;
        List<PathStep> path = new ArrayList<>();
        Ref leaf = descend(key, path);
        List<Entry> rows = leaf == null ? new ArrayList<>() : new ArrayList<>(load(leaf).rows);
        int edited = lowerBound(rows, key);
        boolean replace = edited < rows.size() && compare(rows.get(edited).key, key) == 0;
        if (!replace && size == Long.MAX_VALUE) throw new TreeCapacityException("row-count overflow");
        if (replace) rows.set(edited, entry); else rows.add(edited, entry);
        Node rebuilt = new Node(rows);
        Map<Long, Ref> dropped = new java.util.HashMap<>(retired);
        retire(leaf, dropped);
        for (int level = path.size() - 1; level >= 0; level--) {
            PathStep step = path.get(level);
            Node parent = step.node;
            retire(step.ref, dropped);
            Node even = rebuilt.leaf() && payload(rebuilt) > capacity
                ? evenWithSibling(parent, step.child, rebuilt, edited, dropped) : null;
            if (even != null) {
                rebuilt = withOrigin(even, step.ref);
                edited = -1;
                continue;
            }
            Split split = split(rebuilt, edited);
            List<Ref> children = new ArrayList<>(parent.children);
            children.remove(step.child);
            children.addAll(step.child, split.nodes);
            List<byte[]> keys = new ArrayList<>(parent.keys);
            keys.addAll(step.child, split.separators);
            rebuilt = new Node(keys, children, origin(step.ref), split.separators.isEmpty() && sameKeys(step.ref));
            edited = split.separators.isEmpty() ? -1 : step.child + split.separators.size() - 1;
        }
        while (payload(rebuilt) > capacity) {
            Split split = split(rebuilt, edited);
            rebuilt = new Node(split.separators, split.nodes);
            edited = -1; // Root growth has no edited position.
        }
        return with(new Ref(0, rebuilt), size < 0 ? -1 : replace ? size : size + 1, dropped).withPending(pending.write(entry.key, codec.recordBytes(entry)));
    }

    public PersistentTree remove(byte[] key) throws IOException {
        if (root == null) return this;
        List<PathStep> path = new ArrayList<>();
        Ref leaf = descend(key, path);
        Node original = load(leaf);
        int at = lowerBound(original.rows, key);
        if (at == original.rows.size() || compare(original.rows.get(at).key, key) != 0) return this;
        List<Entry> rows = new ArrayList<>(original.rows);
        rows.remove(at);
        Node rebuilt = new Node(rows);
        boolean empty = rows.isEmpty();
        Map<Long, Ref> dropped = new java.util.HashMap<>(retired);
        retire(leaf, dropped);
        for (int level = path.size() - 1; level >= 0; level--) {
            PathStep step = path.get(level);
            retire(step.ref, dropped);
            List<byte[]> keys = new ArrayList<>(step.node.keys);
            List<Ref> children = new ArrayList<>(step.node.children);
            children.set(step.child, new Ref(0, rebuilt));
            if (empty && !rebuilt.leaf() && children.size() > 1) {
                children.remove(step.child);
                keys.remove(Math.min(step.child, keys.size() - 1));
                empty = false;
                rebuilt = new Node(keys, children, origin(step.ref));
                continue;
            }
            Node parent = new Node(keys, children);
            boolean retainedKeys = sameKeys(step.ref);
            if (children.size() > 1 && payload(rebuilt) < capacity / 2) {
                int left = step.child + 1 < children.size() ? step.child : step.child - 1;
                Ref leftRef = children.get(left), rightRef = children.get(left + 1);
                Node a = load(leftRef), b = load(rightRef);
                Node merged = merge(a, keys.get(left), b);
                if (payload(merged) <= capacity) {
                    retire(leftRef, dropped); retire(rightRef, dropped);
                    children.set(left, new Ref(0, merged));
                    children.remove(left + 1);
                    keys.remove(left);
                    retainedKeys = false;
                } else {
                    Point point = point(merged, false);
                    if (point.chosen >= 0) {
                        Split pair = splitAt(merged, point.chosen);
                        byte[] separator = pair.separators.getFirst();
                        if (payload(parent) - keys.get(left).length + separator.length <= capacity) {
                            retire(leftRef, dropped); retire(rightRef, dropped);
                            children.set(left, pair.nodes.get(0));
                            children.set(left + 1, pair.nodes.get(1));
                            keys.set(left, separator);
                            retainedKeys = false;
                        }
                    }
                }
                empty = false;
            }
            rebuilt = new Node(keys, children, origin(step.ref), retainedKeys);
            if (children.size() > 1) empty = false;
        }
        Ref result = new Ref(0, rebuilt);
        while (!rebuilt.leaf() && rebuilt.keys.isEmpty()) {
            result = rebuilt.children.getFirst();
            rebuilt = load(result);
        }
        boolean emptyRoot = rebuilt.leaf() && rebuilt.rows.isEmpty();
        if (emptyRoot && size >= 0 && size != 1) throw new CorruptStorageException("empty tree disagrees with persisted row count");
        return with(emptyRoot ? null : result, emptyRoot ? 0 : size < 0 ? -1 : size - 1, dropped).withPending(pending.remove(key));
    }

    private Node evenWithSibling(Node parent, int index, Node leaf, int edited, Map<Long, Ref> dropped) throws IOException {
        boolean rightEdge = edited == leaf.rows.size() - 1;
        if (point(leaf, rightEdge).chosen < 0) return null;
        for (int candidate : new int[] {index + 1, rightEdge ? -1 : index - 1}) {
            if (candidate < 0 || candidate >= parent.children.size()) continue;
            Ref siblingRef = parent.children.get(candidate);
            Node sibling = load(siblingRef);
            if (!sibling.leaf()) throw new CorruptStorageException("mixed tree child heights");
            if (payload(sibling) > capacity - capacity / 8) continue;
            Node merged = candidate > index ? merge(leaf, null, sibling) : merge(sibling, null, leaf);
            Point point = point(merged, false);
            if (point.chosen < 0) continue;
            Split pair = splitAt(merged, point.chosen);
            int left = Math.min(index, candidate);
            byte[] separator = pair.separators.getFirst();
            if (payload(parent) - parent.keys.get(left).length + separator.length > capacity) continue;
            List<Ref> children = new ArrayList<>(parent.children);
            children.set(left, pair.nodes.get(0)); children.set(left + 1, pair.nodes.get(1));
            List<byte[]> keys = new ArrayList<>(parent.keys);
            keys.set(left, separator);
            retire(siblingRef, dropped);
            return new Node(keys, children);
        }
        return null;
    }

    private Split split(Node node, int edited) throws IOException {
        List<Ref> nodes = new ArrayList<>();
        List<byte[]> separators = new ArrayList<>();
        while (payload(node) > capacity) {
            Point point = point(node, edited == node.count() - 1);
            int at = point.chosen >= 0 ? point.chosen : point.largestLeft;
            if (at < 0) throw new TreeCapacityException("record or separator exceeds page capacity");
            Split halves = splitAt(node, at);
            Node left = halves.nodes.get(0).dirty;
            if (payload(left) > capacity) throw new TreeCapacityException("left split exceeds page capacity");
            nodes.add(halves.nodes.get(0));
            separators.add(halves.separators.getFirst());
            int removed = node.leaf() ? at : at + 1;
            edited = edited < removed ? -1 : edited - removed;
            node = halves.nodes.get(1).dirty;
        }
        if (node.leaf() && node.rows.isEmpty()) throw new TreeCapacityException("empty split leaf");
        nodes.add(new Ref(0, node));
        return new Split(nodes, separators);
    }

    private Point point(Node node, boolean rightEdge) throws IOException {
        int minimum = 1, maximum = node.leaf() ? node.count() - 1 : node.count() - 2;
        if (!node.leaf() && node.count() == 2) minimum = maximum = 1;
        int minRight = -1, maxLeft = -1, balanced = -1;
        long total = payload(node);
        if (!node.leaf() || codec.monotoneSliceSizes()) {
            minRight = boundary(node, minimum, maximum, capacity, false);
            int firstTooLarge = boundary(node, minimum, maximum, capacity + 1, true);
            maxLeft = firstTooLarge < 0 ? maximum : firstTooLarge - 1;
            if (maxLeft < minimum) maxLeft = -1;
            if (minRight < 0 || maxLeft < minRight) return new Point(-1, maxLeft);
            if (rightEdge) return new Point(maxLeft, maxLeft);
            balanced = boundary(node, minimum, maximum, (total + 1) / 2, true);
            if (balanced < 0) balanced = maximum;
            return new Point(Math.max(minRight, Math.min(balanced, maxLeft)), maxLeft);
        }
        for (int m = minimum; m <= maximum; m++) {
            Split pair = splitAt(node, m);
            long left = payload(pair.nodes.get(0).dirty), right = payload(pair.nodes.get(1).dirty);
            if (right <= capacity && minRight < 0) minRight = m;
            if (left <= capacity) maxLeft = m;
            if (left * 2 >= total && balanced < 0) balanced = m;
        }
        if (minRight < 0 || maxLeft < minRight) return new Point(-1, maxLeft);
        if (balanced < 0) balanced = maximum;
        return new Point(rightEdge ? maxLeft : Math.max(minRight, Math.min(balanced, maxLeft)), maxLeft);
    }

    /** First split whose left size reaches threshold, or whose right size falls below it. */
    private int boundary(Node node, int low, int high, long threshold, boolean left) throws IOException {
        int answer = -1;
        while (low <= high) {
            int middle = low + (high - low) / 2;
            Split pair = splitAt(node, middle);
            long size = payload(pair.nodes.get(left ? 0 : 1).dirty);
            if (left ? size >= threshold : size <= threshold) { answer = middle; high = middle - 1; }
            else low = middle + 1;
        }
        return answer;
    }

    private Split splitAt(Node node, int at) {
        if (node.leaf()) return new Split(List.of(
            new Ref(0, new Node(node.rows.subList(0, at))), new Ref(0, new Node(node.rows.subList(at, node.rows.size())))),
            List.of(node.rows.get(at).key));
        return new Split(List.of(
            new Ref(0, new Node(node.keys.subList(0, at), node.children.subList(0, at + 1))),
            new Ref(0, new Node(node.keys.subList(at + 1, node.keys.size()), node.children.subList(at + 1, node.children.size())))),
            List.of(node.keys.get(at)));
    }

    private static Node merge(Node left, byte[] separator, Node right) throws IOException {
        if (left.leaf() != right.leaf()) throw new CorruptStorageException("mixed tree child heights");
        if (left.leaf()) {
            List<Entry> rows = new ArrayList<>(left.rows); rows.addAll(right.rows); return new Node(rows);
        }
        List<byte[]> keys = new ArrayList<>(left.keys); keys.add(separator); keys.addAll(right.keys);
        List<Ref> children = new ArrayList<>(left.children); children.addAll(right.children);
        return new Node(keys, children);
    }

    private long payload(Node node) throws IOException {
        if (node.encodedSize >= 0) return node.encodedSize;
        if (node.leaf()) return node.encodedSize = node.rows.isEmpty() ? 0 : codec.payloadSize(node.rows);
        long size = (4L + layout.pointerBytes()) * node.keys.size() + layout.pointerBytes();
        for (byte[] key : node.keys) size += key.length;
        return node.encodedSize = size;
    }

    private PagePointer readPointer(ByteBuffer bytes, int offset) {
        long page = Integer.toUnsignedLong(bytes.getInt(offset)); byte[] tag = new byte[layout.tagBytes()]; bytes.get(offset + 4, tag);
        return new PagePointer(page, tag);
    }
    private void writePointer(byte[] bytes, int offset, PagePointer pointer) {
        PageFormat.put32(bytes, offset, pointer.page()); byte[] tag = pointer.tag();
        if (tag.length != layout.tagBytes()) throw new IllegalArgumentException("wrong pointer tag width");
        System.arraycopy(tag, 0, bytes, offset + 4, tag.length);
    }

    private Ref descend(byte[] key, List<PathStep> path) throws IOException {
        Ref ref = root;
        Set<Long> visited = new HashSet<>();
        while (ref != null) {
            checkCycle(ref, visited);
            Node node = load(ref);
            if (node.leaf()) return ref;
            int index = child(node.keys, key);
            path.add(new PathStep(ref, node, index));
            ref = node.children.get(index);
        }
        return null;
    }

    private Node load(Ref ref) throws IOException {
        if (ref.dirty != null) return hinted(ref, ref.dirty);
        if (ref.residentRootLeaf != null) return hinted(ref, ref.residentRootLeaf);
        if (ref.interior != null) return hinted(ref, ref.interior);
        Pager.Page page = pager.readBody(ref.page, ref.tag);
        var header = page.header();
        if (ref.height != 0 && header.type() != (ref.height == 1 ? 2 : 3)) throw new CorruptStorageException("tree height disagrees with page kind");
        if (header.next() != 0) throw new CorruptStorageException("tree page has chain link");
        ByteBuffer bytes = page.buffer();
        if (header.type() == 2) {
            if (header.count() == 0 || header.count() > Integer.MAX_VALUE) throw new CorruptStorageException("invalid leaf count");
            List<Entry> cached = page.decodedLeaf(codec);
            if (cached != null) return new Node(cached);
            List<Entry> rows = codec.decode(bytes.slice(16, capacity), (int) header.count());
            if (rows.size() != header.count()) throw new CorruptStorageException("leaf count mismatch");
            for (int i = 1; i < rows.size(); i++) {
                if (compare(rows.get(i - 1).key, rows.get(i).key) >= 0) throw new CorruptStorageException("non-increasing leaf keys");
            }
            page.rememberLeaf(codec, rows);
            return new Node(rows);
        }
        if (header.type() != 3 || header.count() > (capacity - layout.pointerBytes()) / (4 + layout.pointerBytes())) {
            throw new CorruptStorageException("invalid interior page");
        }
        int count = (int) header.count();
        List<Ref> children = new ArrayList<>();
        for (int i = 0; i <= count; i++) {
            PagePointer pointer = readPointer(bytes, 16 + i * layout.pointerBytes());
            long target = pointer.page(); byte[] targetTag = pointer.tag();
            for (var remap : remaps) {
                if (remap.parent() != ref.page) continue;
                if (remap.slot() > count) throw new CorruptStorageException("remap slot outside interior");
                if (remap.slot() == i) { target = remap.page(); targetTag = remap.tag(); }
            }
            if (target < layout.firstBody() || target >= pageCount || target == ref.page) throw new CorruptStorageException("invalid interior child");
            Ref child = new Ref(target, null, targetTag);
            if (ref.height > 1) child.height = ref.height - 1;
            children.add(child);
        }
        int directory = 16 + (count + 1) * layout.pointerBytes();
        int blob = directory + count * 4;
        int end = 0;
        List<byte[]> keys = new ArrayList<>();
        for (int i = 0; i < count; i++) {
            long next = Integer.toUnsignedLong(bytes.getInt(directory + i * 4));
            if (next < end || next > layout.bodyEnd() - blob) throw new CorruptStorageException("invalid separator directory");
            byte[] key = new byte[(int) next - end];
            bytes.get(blob + end, key);
            if (i != 0 && compare(keys.getLast(), key) >= 0) throw new CorruptStorageException("non-increasing separator keys");
            keys.add(key); end = (int) next;
        }
        for (int i = blob + end; i < layout.bodyEnd(); i++) {
            if (bytes.get(i) != 0) throw new CorruptStorageException("nonzero interior padding");
        }
        Node node = new Node(keys, children);
        ref.interior = node;
        return node;
    }

    private static Node hinted(Ref ref, Node node) throws CorruptStorageException {
        if (ref.height != 0 && node.leaf() != (ref.height == 1)) throw new CorruptStorageException("tree height disagrees with node kind");
        if (ref.height > 1) for (Ref child : node.children) {
            if (child.height != 0 && child.height != ref.height - 1) throw new CorruptStorageException("mixed tree child heights");
            child.height = ref.height - 1;
        }
        return node;
    }

    private int kind(Ref ref) throws IOException {
        if (ref.height != 0) return ref.height == 1 ? 2 : 3;
        if (ref.dirty != null) return ref.dirty.leaf() ? 2 : 3;
        if (ref.interior != null) return 3;
        int kind = pager.readBody(ref.page, ref.tag).header().type();
        if (kind != 2 && kind != 3) throw new CorruptStorageException("invalid tree page type");
        return kind;
    }

    private static Ref origin(Ref ref) { return ref.page != 0 ? ref : ref.dirty.origin; }
    private static boolean sameKeys(Ref ref) { return ref.page != 0 || ref.dirty.sameKeys; }
    private static Node withOrigin(Node node, Ref ref) {
        return node.leaf() ? node : new Node(node.keys, node.children, origin(ref));
    }

    public Cursor cursor() { return new Cursor(null, true, null, true, false); }
    public Cursor cursor(byte[] lower, boolean lowerInclusive, byte[] upper, boolean upperInclusive) {
        return new Cursor(lower, lowerInclusive, upper, upperInclusive, false);
    }

    /** Descending traversal with the same ascending lower/upper bounds as cursor(). */
    public Cursor reverseCursor() { return new Cursor(null, true, null, true, true); }
    public Cursor reverseCursor(byte[] lower, boolean lowerInclusive, byte[] upper, boolean upperInclusive) {
        return new Cursor(lower, lowerInclusive, upper, upperInclusive, true);
    }

    public final class Cursor implements AutoCloseable {
        private final List<PathStep> path = new ArrayList<>();
        private final Set<Long> visited = new HashSet<>();
        private Node leaf;
        private int position;
        private boolean started;
        private int hintWindow = 1;
        private boolean closed;
        private final byte[] lower, upper;
        private final boolean lowerInclusive, upperInclusive, reverse;

        private Cursor(byte[] lower, boolean lowerInclusive, byte[] upper, boolean upperInclusive, boolean reverse) {
            this.lower = lower == null ? null : lower.clone(); this.upper = upper == null ? null : upper.clone();
            this.lowerInclusive = lowerInclusive; this.upperInclusive = upperInclusive; this.reverse = reverse;
            closed = emptyRange(lower, lowerInclusive, upper, upperInclusive);
        }

        /** Returns the next immutable entry, or null at end. */
        public Entry next() throws IOException {
            if (closed) return null;
            if (!started) { started = true; if (root != null) descendStart(root); }
            while (leaf != null && (position < 0 || position >= leaf.rows.size())) {
                leaf = null;
                while (!path.isEmpty()) {
                    PathStep last = path.removeLast();
                    int sibling = last.child + (reverse ? -1 : 1);
                    if (sibling >= 0 && sibling < last.node.children.size()) {
                        if (!reverse && beyondUpper(last.node.keys.get(last.child))) { close(); return null; }
                        // A separator is the inclusive minimum of the right subtree.
                        if (reverse && lower != null && compare(last.node.keys.get(sibling), lower) <= 0) { close(); return null; }
                        path.add(new PathStep(last.ref, last.node, sibling));
                        outermost(last.node.children.get(sibling));
                        break;
                    }
                    if (last.ref.page != 0) visited.remove(last.ref.page);
                }
            }
            if (leaf == null) { close(); return null; }
            byte[] key = leaf.rows.get(position).key;
            if (reverse ? belowLower(key) : beyondUpper(key)) { close(); return null; }
            Entry entry = leaf.rows.get(position);
            position += reverse ? -1 : 1;
            return entry;
        }

        private boolean beyondUpper(byte[] key) {
            if (upper == null) return false;
            int compared = compare(key, upper);
            return compared > 0 || compared == 0 && !upperInclusive;
        }

        private boolean belowLower(byte[] key) {
            if (lower == null) return false;
            int compared = compare(key, lower);
            return compared < 0 || compared == 0 && !lowerInclusive;
        }

        private void descendStart(Ref ref) throws IOException {
            while (true) {
                hint(ref);
                checkCycle(ref, visited); Node node = load(ref);
                if (node.leaf()) {
                    if (ref.page != 0) visited.remove(ref.page);
                    leaf = node;
                    if (reverse) {
                        position = upper == null ? node.rows.size() - 1 : lowerBound(node.rows, upper);
                        if (upper != null && (position == node.rows.size() || !upperInclusive || compare(node.rows.get(position).key, upper) != 0)) position--;
                    } else {
                        position = lower == null ? 0 : lowerBound(node.rows, lower);
                        if (lower != null && !lowerInclusive && position < node.rows.size() && compare(node.rows.get(position).key, lower) == 0) position++;
                    }
                    return;
                }
                int next = reverse
                    ? upper == null ? node.children.size() - 1 : upperInclusive ? child(node.keys, upper) : separatorLowerBound(node.keys, upper)
                    : lower == null ? 0 : child(node.keys, lower);
                path.add(new PathStep(ref, node, next)); ref = node.children.get(next);
            }
        }

        private void outermost(Ref ref) throws IOException {
            while (true) {
                hint(ref);
                checkCycle(ref, visited);
                Node node = load(ref);
                if (node.leaf()) {
                    // Leaf is no longer an ancestor once its contents are retained.
                    if (ref.page != 0) visited.remove(ref.page);
                    leaf = node; position = reverse ? node.rows.size() - 1 : 0; return;
                }
                int next = reverse ? node.children.size() - 1 : 0;
                path.add(new PathStep(ref, node, next));
                ref = node.children.get(next);
            }
        }
        private void hint(Ref ref) {
            if (pager == null || !pager.readAheadEnabled() || ref.page == 0 || ref.height != 1 || ref.residentRootLeaf != null) return;
            var pointers = new ArrayList<PagePointer>();
            if (path.isEmpty()) pointers.add(new PagePointer(ref.page, ref.tag));
            else {
                PathStep parent = path.getLast();
                for (int at = parent.child; at >= 0 && at < parent.node.children.size() && pointers.size() < hintWindow; at += reverse ? -1 : 1) {
                    if (at != parent.child && (!reverse && beyondUpper(parent.node.keys.get(at - 1)) || reverse && lower != null && compare(parent.node.keys.get(at), lower) <= 0)) break;
                    Ref leafRef = parent.node.children.get(at); if (leafRef.page != 0) pointers.add(new PagePointer(leafRef.page, leafRef.tag));
                }
            }
            pager.prefetch(pointers); hintWindow = Math.min(64, hintWindow * 2);
        }

        @Override public void close() { closed = true; leaf = null; path.clear(); visited.clear(); }
    }

    private PersistentTree with(Ref root, long count, Map<Long, Ref> retired) {
        return new PersistentTree(pageSize, codec, pager, pageCount, remaps, root, count, retired, pageCodec, lineage, pending);
    }
    public long pendingBytes() { return pending.bytes; }
    public PersistentTree forgetPendingKeys() { return withPending(pending.forget()); }
    public PersistentTree committed() { return withPending(PendingBytes.EMPTY); }
    private PersistentTree withPending(PendingBytes value) {
        return value == pending ? this : new PersistentTree(pageSize, codec, pager, pageCount, remaps, root, size, retired, pageCodec, lineage, value);
    }
    private static void retire(Ref ref, Map<Long, Ref> retired) { if (ref != null && ref.page != 0) retired.put(ref.page, ref); }
    private static void checkCycle(Ref ref, Set<Long> seen) throws CorruptStorageException {
        if (ref.page != 0 && !seen.add(ref.page)) throw new CorruptStorageException("tree page cycle");
    }
    private static int compare(byte[] a, byte[] b) { return Arrays.compareUnsigned(a, b); }
    private static int lowerBound(List<Entry> rows, byte[] key) {
        int low = 0, high = rows.size();
        while (low < high) { int mid = (low + high) >>> 1; if (compare(rows.get(mid).key, key) < 0) low = mid + 1; else high = mid; }
        return low;
    }
    private static int child(List<byte[]> keys, byte[] key) {
        int low = 0, high = keys.size();
        while (low < high) { int mid = (low + high) >>> 1; if (compare(keys.get(mid), key) <= 0) low = mid + 1; else high = mid; }
        return low;
    }
    private static int separatorLowerBound(List<byte[]> keys, byte[] key) {
        int low = 0, high = keys.size();
        while (low < high) { int mid = (low + high) >>> 1; if (compare(keys.get(mid), key) < 0) low = mid + 1; else high = mid; }
        return low;
    }
}
