package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.function.Consumer;
import org.jed.storage.*;

/** Canonical resident GiST bounding tree, as specified by gist.md §4.1. */
final class GistTree {
    private static final int FANOUT = 4;
    record Component(SqlRange range, byte[] minimum, byte[] maximum) {}
    record Entry(List<Component> bound, byte[] key, Node child) {}
    private static final class Node {
        final boolean leaf; final List<Entry> entries = new ArrayList<>();
        Node(boolean leaf) { this.leaf = leaf; }
    }
    record Loaded(PersistentTree entries, GistTree tree) {}
    record Prepared(PageAllocator allocator, List<ManifestEncoder.Write> writes, PagePointer root, Set<Long> retired) {}
    private final List<SqlType> types;
    private final Node root;
    private final PagePointer physicalRoot;
    private final Set<Long> physicalPages;
    private final boolean dirty;
    private GistTree(List<SqlType> types, Node root, PagePointer pointer, Set<Long> pages, boolean dirty) {
        this.types = List.copyOf(types); this.root = root; physicalRoot = pointer; physicalPages = Set.copyOf(pages); this.dirty = dirty;
    }
    PagePointer pointer() { return physicalRoot; }
    Set<Long> physicalPages() { return physicalPages; }
    boolean dirty() { return dirty; }
    PersistentTree.StructuralFacts structuralFacts() {
        if (root == null || root.entries.isEmpty()) return new PersistentTree.StructuralFacts(0, 0, 0);
        record Pending(Node node, int depth) {}
        var pending = new java.util.ArrayDeque<Pending>(); pending.push(new Pending(root, 1));
        long nodes = 0, leaves = 0; int height = 0;
        while (!pending.isEmpty()) {
            var item = pending.pop(); nodes++; height = Math.max(height, item.depth());
            if (item.node().leaf) leaves++;
            else for (Entry entry : item.node().entries) pending.push(new Pending(entry.child(), item.depth() + 1));
        }
        return new PersistentTree.StructuralFacts(nodes, leaves, height);
    }
    static byte[] leafKey(List<SqlValue> values, byte[] storageKey) {
        Encoding.Sink out = new Encoding.Sink();
        for (SqlValue value : values) {
            if (value.isNull()) return null;
            if (value.type().kind() == SqlType.Kind.RANGE) Encoding.body(out, value);
            else { byte[] key = Encoding.key(value); out.u16(key.length); out.raw(key); out.u16(key.length); out.raw(key); }
        }
        out.raw(storageKey); return out.bytes();
    }
    static GistTree build(List<SqlType> types, PersistentTree store, GistTree previous) throws IOException {
        var entries = new ArrayList<Entry>();
        try (var cursor = store.cursor()) {
            PersistentTree.Entry entry;
            while ((entry = cursor.next()) != null) {
                Encoding.Reader in = new Encoding.Reader(entry.key()); List<Component> bound = decode(types, in);
                byte[] key = in.raw(in.remaining()); if (key.length == 0) throw corrupt("empty GiST storage key");
                entries.add(new Entry(bound, key, null));
            }
        }
        entries.sort(GistTree::compare);
        Node root = new Node(true);
        for (Entry entry : entries) {
            Node sibling = insert(types, root, entry);
            if (sibling != null) { Node next = new Node(false); next.entries.add(branch(root)); next.entries.add(branch(sibling)); next.entries.sort(GistTree::compare); root = next; }
        }
        return new GistTree(types, root, previous == null ? new PagePointer(0, new byte[store.layout().tagBytes()]) : previous.physicalRoot,
            previous == null ? Set.of() : previous.physicalPages, true);
    }
    private static Node insert(List<SqlType> types, Node node, Entry entry) {
        if (node.leaf) node.entries.add(entry);
        else {
            int best = 0; byte[] bestBytes = null;
            for (int i = 0; i < node.entries.size(); i++) {
                byte[] merged = encode(types, union(node.entries.get(i).bound, entry.bound));
                if (bestBytes == null || Arrays.compareUnsigned(merged, bestBytes) < 0) { best = i; bestBytes = merged; }
            }
            Node child = node.entries.get(best).child, sibling = insert(types, child, entry);
            node.entries.set(best, branch(child)); if (sibling != null) node.entries.add(branch(sibling));
        }
        node.entries.sort(GistTree::compare);
        if (node.entries.size() <= FANOUT) return null;
        int middle = (node.entries.size() + 1) / 2; Node right = new Node(node.leaf);
        right.entries.addAll(node.entries.subList(middle, node.entries.size())); node.entries.subList(middle, node.entries.size()).clear(); return right;
    }
    private static Entry branch(Node child) {
        List<Component> bound = child.entries.getFirst().bound; byte[] minimum = child.entries.getFirst().key;
        for (int i = 1; i < child.entries.size(); i++) { Entry entry = child.entries.get(i); bound = union(bound, entry.bound); if (Arrays.compareUnsigned(entry.key, minimum) < 0) minimum = entry.key; }
        return new Entry(bound, minimum, child);
    }
    private static int compare(Entry a, Entry b) { int c = compareBounds(a.bound, b.bound); return c != 0 ? c : Arrays.compareUnsigned(a.key, b.key); }
    private static int compareBounds(List<Component> a, List<Component> b) {
        for (int i = 0; i < a.size(); i++) {
            Component x = a.get(i), y = b.get(i); int c;
            if (x.range != null) c = x.range.compare(y.range);
            else { c = Arrays.compareUnsigned(x.minimum, y.minimum); if (c == 0) c = Arrays.compareUnsigned(x.maximum, y.maximum); }
            if (c != 0) return c;
        }
        return 0;
    }
    private static List<Component> union(List<Component> a, List<Component> b) {
        List<Component> result = new ArrayList<>(a.size());
        for (int i = 0; i < a.size(); i++) { Component x = a.get(i), y = b.get(i);
            result.add(x.range != null ? new Component(x.range.union(y.range, false), null, null)
                : new Component(null, Arrays.compareUnsigned(x.minimum, y.minimum) <= 0 ? x.minimum : y.minimum,
                    Arrays.compareUnsigned(x.maximum, y.maximum) >= 0 ? x.maximum : y.maximum)); }
        return List.copyOf(result);
    }
    private static byte[] encode(List<SqlType> types, List<Component> bound) {
        Encoding.Sink out = new Encoding.Sink();
        for (int i = 0; i < types.size(); i++) { Component c = bound.get(i);
            if (c.range != null) Encoding.body(out, SqlValue.ofRange(types.get(i), c.range));
            else { out.u16(c.minimum.length); out.raw(c.minimum); out.u16(c.maximum.length); out.raw(c.maximum); } }
        return out.bytes();
    }
    private static List<Component> decode(List<SqlType> types, Encoding.Reader in) {
        List<Component> result = new ArrayList<>(types.size());
        for (SqlType type : types) {
            if (type.kind() == SqlType.Kind.RANGE) result.add(new Component(Encoding.readBody(in, type).asRange(), null, null));
            else { byte[] min = in.raw(in.u16()), max = in.raw(in.u16());
                if (min.length == 0 || max.length != min.length || Arrays.compareUnsigned(min, max) > 0) throw corrupt("invalid scalar GiST bound");
                result.add(new Component(null, min, max)); }
        }
        return List.copyOf(result);
    }
    /** Charges the complete conservative descent before yielding storage-ordered candidates. */
    void search(List<SqlValue> query, List<String> strategies, CostMeter meter, Consumer<byte[]> candidate) {
        if (root.entries.isEmpty()) return;
        search(root, query, strategies, meter, candidate);
    }
    private static void search(Node node, List<SqlValue> query, List<String> strategies, CostMeter meter, Consumer<byte[]> candidate) {
        if (meter != null) { meter.charge(Costs.PAGE_READ + (node.leaf ? 0 : Costs.GIST_DESCENT)); meter.guard(); }
        for (Entry entry : node.entries) {
            boolean matches = true;
            for (int i = 0; i < query.size(); i++) { Component bound = entry.bound.get(i); SqlValue value = query.get(i); String strategy = strategies.get(i);
                if (value.isNull()) { matches = false; break; }
                if (bound.range != null) {
                    SqlRange range = value.asRange(); matches = node.leaf && strategy.equals("@>") ? bound.range.contains(range) : bound.range.overlaps(range);
                } else { byte[] key = Encoding.key(value); matches = node.leaf ? Arrays.equals(bound.minimum, key) : Arrays.compareUnsigned(bound.minimum, key) <= 0 && Arrays.compareUnsigned(key, bound.maximum) <= 0; }
                if (!matches) break;
            }
            if (matches) { if (node.leaf) candidate.accept(entry.key); else search(entry.child, query, strategies, meter, candidate); }
        }
    }
    boolean conflicts(byte[] leafKey, List<String> strategies) { return conflicts(leafKey, strategies, true); }
    boolean conflicts(byte[] leafKey, List<String> strategies, boolean ignoreSelf) {
        Encoding.Reader in = new Encoding.Reader(leafKey); List<Component> query = decode(types, in); byte[] ignored = in.raw(in.remaining());
        return conflicts(root, query, strategies, ignoreSelf ? ignored : null);
    }
    private static boolean conflicts(Node node, List<Component> query, List<String> strategies, byte[] ignored) {
        for (Entry entry : node.entries) {
            boolean matches = true;
            for (int i = 0; i < query.size(); i++) { Component bound = entry.bound.get(i), value = query.get(i);
                if (bound.range != null) matches = bound.range.overlaps(value.range);
                else matches = node.leaf ? Arrays.equals(bound.minimum, value.minimum) : Arrays.compareUnsigned(bound.minimum, value.minimum) <= 0 && Arrays.compareUnsigned(value.minimum, bound.maximum) <= 0;
                if (!matches) break;
            }
            if (matches && (node.leaf ? !Arrays.equals(entry.key, ignored) : conflicts(entry.child, query, strategies, ignored))) return true;
        }
        return false;
    }
    Prepared prepare(PageAllocator input, long txid, PageLayout layout, PageCodec codec) throws IOException {
        if (!dirty) return new Prepared(input, List.of(), physicalRoot, Set.of());
        PageAllocator allocator = input.copy(); List<ManifestEncoder.Write> writes = new ArrayList<>();
        PagePointer pointer = root.entries.isEmpty() ? new PagePointer(0, new byte[layout.tagBytes()]) : serialize(root, allocator, txid, layout, codec, writes);
        return new Prepared(allocator, List.copyOf(writes), pointer, physicalPages);
    }
    Prepared writeImage(PageAllocator input, long txid, PageLayout layout, PageCodec codec, PersistentTree.ImageSink sink) throws IOException {
        PageAllocator allocator = input.copy();
        PagePointer pointer = root.entries.isEmpty() ? new PagePointer(0, new byte[layout.tagBytes()]) : serialize(root, allocator, txid, layout, codec, sink);
        return new Prepared(allocator, List.of(), pointer, Set.of());
    }
    private PagePointer serialize(Node node, PageAllocator allocator, long txid, PageLayout layout, PageCodec codec, List<ManifestEncoder.Write> writes) throws IOException {
        return serialize(node, allocator, txid, layout, codec, (page, bytes) -> writes.add(new ManifestEncoder.Write(page, bytes)));
    }
    private PagePointer serialize(Node node, PageAllocator allocator, long txid, PageLayout layout, PageCodec codec, PersistentTree.ImageSink sink) throws IOException {
        List<PagePointer> children = new ArrayList<>();
        if (!node.leaf) for (Entry entry : node.entries) children.add(serialize(entry.child, allocator, txid, layout, codec, sink));
        Encoding.Sink out = new Encoding.Sink(); int at = 0;
        for (Entry entry : node.entries) { byte[] bound = encode(types, entry.bound); out.u16(bound.length); out.raw(bound);
            if (node.leaf) { out.u16(entry.key.length); out.raw(entry.key); }
            else { PagePointer pointer = children.get(at++); out.unsigned(pointer.page(), 4); out.raw(pointer.tag()); } }
        byte[] payload = out.bytes(); if (payload.length > layout.capacity()) throw corrupt("GiST node exceeds page capacity");
        long page = allocator.allocate(); byte[] bytes = new byte[layout.pageSize()]; bytes[0] = (byte)(node.leaf ? 5 : 6);
        PageFormat.put16(bytes, 2, (int)(txid & 65535)); PageFormat.put32(bytes, 4, node.entries.size()); System.arraycopy(payload, 0, bytes, 16, payload.length);
        bytes = codec.sealBody(page, bytes); sink.write(page, bytes); return new PagePointer(page, codec.bodyTag(bytes));
    }
    static Loaded open(Pager pager, PagePointer root, long pageCount, List<SqlType> types) throws IOException {
        Set<Long> pages = new HashSet<>(); List<byte[]> keys = new ArrayList<>();
        if (root.page() != 0) read(pager, root, pageCount, types, pages, keys, 0);
        PersistentTree store = PersistentTree.empty(pager.pageSize(), TableLeafCodec.index(), pager.codec());
        for (byte[] key : keys) { if (store.getEntry(key) != null) throw corrupt("duplicate GiST leaf entry"); store = store.put(new PersistentTree.Entry(key, new StoredRow(List.of()))); }
        GistTree rebuilt = build(types, store, null);
        return new Loaded(store.committed(), new GistTree(types, rebuilt.root, root, pages, false));
    }
    private static void read(Pager pager, PagePointer pointer, long pageCount, List<SqlType> types, Set<Long> pages, List<byte[]> keys, int depth) throws IOException {
        if (depth >= 64 || pointer.page() < pager.layout().firstBody() || pointer.page() >= pageCount || !pages.add(pointer.page())) throw corrupt("invalid GiST child graph");
        var page = pager.readBody(pointer.page(), pointer.tag()); int kind = page.header().type(); long count = page.header().count();
        if ((kind != 5 && kind != 6) || count < 1 || count > FANOUT || page.header().next() != 0) throw corrupt("invalid GiST node header");
        byte[] payload = new byte[pager.layout().capacity()]; page.buffer().slice(16, payload.length).get(payload); Encoding.Reader in = new Encoding.Reader(payload);
        List<Component> previous = null; byte[] previousKey = null;
        for (int i = 0; i < count; i++) { byte[] boundBytes = in.raw(in.u16()); Encoding.Reader bounds = new Encoding.Reader(boundBytes); List<Component> bound = decode(types, bounds);
            if (bounds.remaining() != 0 || previous != null && compareBounds(previous, bound) > 0) throw corrupt("invalid GiST node bound order");
            if (kind == 5) { byte[] key = in.raw(in.u16()); if (key.length == 0 || previous != null && compareBounds(previous, bound) == 0 && Arrays.compareUnsigned(previousKey, key) >= 0) throw corrupt("invalid GiST leaf order");
                Encoding.Sink encoded = new Encoding.Sink(); encoded.raw(boundBytes); encoded.raw(key); keys.add(encoded.bytes()); previousKey = key;
            } else { long child = in.unsigned(4); byte[] tag = in.raw(pager.layout().tagBytes()); read(pager, new PagePointer(child, tag), pageCount, types, pages, keys, depth + 1); }
            previous = bound;
        }
        while (in.remaining() != 0) if (in.u8() != 0) throw corrupt("nonzero GiST node padding");
    }
    private static SqlException corrupt(String message) { return new SqlException("XX001", message); }
}
