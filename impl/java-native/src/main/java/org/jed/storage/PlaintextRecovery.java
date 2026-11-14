package org.jed.storage;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.zip.CRC32;

import static org.jed.storage.PageFormat.*;

/**
 * Keyless v53 meta and manifest selection for both storage variants. Reads one page at a time; inherited
 * data pages are never scanned. Catalog/free-list semantic validation is a
 * separate step after selection and must not cause fallback to an older meta.
 */
public final class PlaintextRecovery {
    private PlaintextRecovery() { }

    public record Remap(long parent, int slot, long page, byte[] tag) {
        public Remap(long parent, int slot, long page) { this(parent, slot, page, new byte[0]); }
        public Remap { tag = tag.clone(); }
        @Override public byte[] tag() { return tag.clone(); }
        @Override public boolean equals(Object other) { return other instanceof Remap r && parent == r.parent && slot == r.slot && page == r.page && Arrays.equals(tag, r.tag); }
        @Override public int hashCode() { return java.util.Objects.hash(parent, slot, page, Arrays.hashCode(tag)); }
    }

    public record Meta(int pageSize, long txid, long root, long pageCount, long freeListHead,
                       long livePages, long manifestHead, long manifestEntries,
                       long manifestPages, long manifestCrc, List<Remap> remaps, byte[] encoded,
                       byte[] rootTag, byte[] freeTag, boolean encrypted) {
        public Meta(int pageSize, long txid, long root, long pageCount, long freeListHead, long livePages, long manifestHead, long manifestEntries,
                    long manifestPages, long manifestCrc, List<Remap> remaps, byte[] encoded) {
            this(pageSize, txid, root, pageCount, freeListHead, livePages, manifestHead, manifestEntries, manifestPages, manifestCrc, remaps, encoded, new byte[0], new byte[0], false);
        }
        public Meta {
            remaps = List.copyOf(remaps);
            encoded = encoded.clone(); rootTag = rootTag.clone(); freeTag = freeTag.clone();
        }
        @Override public byte[] encoded() { return encoded.clone(); }
        @Override public byte[] rootTag() { return rootTag.clone(); }
        @Override public byte[] freeTag() { return freeTag.clone(); }
        public PageLayout layout() { return new PageLayout(pageSize, encrypted); }
    }

    public record Selected(Meta meta, int slot, int floorSlot, Set<Long> protectedPages) {
        public Selected { protectedPages = Set.copyOf(protectedPages); }
        /** A writable handle must perform this barrier before its first storage mutation. */
        public boolean needsStabilization() { return meta.manifestEntries != 0; }
    }

    public static Meta parseMeta(byte[] bytes) throws CorruptStorageException {
        if (!validPageSize(bytes.length)) throw corrupt("invalid meta size");
        boolean encrypted = bytes[6] == 1 && bytes[7] == 40;
        if ((!encrypted && (bytes[6] != 0 || bytes[7] != 0)) || encrypted && bytes.length < 1024) throw corrupt("invalid meta suite or reserve");
        PageLayout layout = new PageLayout(bytes.length, encrypted);
        if (u32(bytes, 0) != 0x4a454442L || u16(bytes, 4) != VERSION
            || u32(bytes, 8) != bytes.length || u32(bytes, 32) != metaCrc(bytes)) {
            throw corrupt("invalid meta header or checksum");
        }
        long root = u32(bytes, 20), count = u32(bytes, 24), free = u32(bytes, 28);
        long head = u32(bytes, 36), entries = u32(bytes, 40), pages = u32(bytes, 44);
        long chainCrc = u64(bytes, 48), live = u32(bytes, 56), remapCount = u32(bytes, 60);
        if (count <= layout.firstBody() || !bodyIndex(root, count, layout) || (free != 0 && !bodyIndex(free, count, layout))
            || live > count - layout.firstBody() || entries + pages > count - layout.firstBody() || chainCrc < 0 || chainCrc > 0xffffffffL
            || (head == 0) != (pages == 0) || (head != 0 && !bodyIndex(head, count, layout))
            || (pages == 0 && chainCrc != 0) || (entries == 0 && pages != 0)) {
            throw corrupt("invalid meta page bounds");
        }
        int inline = (int) Math.min(entries, layout.inlineManifestCapacity());
        if (entries > inline + pages * layout.overflowManifestCapacity()
            || !zeros(bytes, layout.inlineManifestStart() + inline * 8, layout.remapStart())
            || remapCount > layout.remapCapacity()) {
            throw corrupt("invalid manifest descriptor");
        }
        List<Remap> remaps = new ArrayList<>();
        Set<Long> targets = new HashSet<>();
        long previousParent = 0;
        int previousSlot = -1;
        int at = layout.remapStart();
        for (long i = 0; i < remapCount; i++, at += 10 + layout.tagBytes()) {
            long parent = u32(bytes, at), page = u32(bytes, at + 6);
            int slot = u16(bytes, at + 4);
            if (!bodyIndex(parent, count, layout) || !bodyIndex(page, count, layout) || parent == page
                || parent < previousParent || (parent == previousParent && slot <= previousSlot)
                || !targets.add(page)) throw corrupt("invalid interior remap");
            remaps.add(new Remap(parent, slot, page, Arrays.copyOfRange(bytes, at + 10, at + 10 + layout.tagBytes())));
            previousParent = parent;
            previousSlot = slot;
        }
        if (!zeros(bytes, at, layout.bodyEnd())) throw corrupt("nonzero remap padding");
        if (encrypted && free == 0 && !zeros(bytes, 80, 96)) throw corrupt("nonzero absent free-list tag");
        return new Meta(bytes.length, u64(bytes, 12), root, count, free, live, head,
            entries, pages, chainCrc, remaps, bytes,
            encrypted ? Arrays.copyOfRange(bytes, 64, 80) : new byte[0], encrypted ? Arrays.copyOfRange(bytes, 80, 96) : new byte[0], encrypted);
    }

    /** Page size discovery is bounded before any allocation driven by file bytes. */
    public static int pageSize(BlockStore store) throws IOException {
        if (store.size() < 12) throw corrupt("short database header");
        long size = u32(store.readAt(0, 12), 8);
        if (size > Integer.MAX_VALUE || !validPageSize((int) size)) throw corrupt("invalid database page size");
        return (int) size;
    }

    public static Selected recover(BlockStore store) throws IOException {
        return recover(store, pageSize(store));
    }

    public static Selected recover(BlockStore store, int pageSize) throws IOException {
        if (!validPageSize(pageSize) || store.size() < 2L * pageSize) throw corrupt("short meta region");
        long physicalPages = store.size() / pageSize;
        Meta[] candidates = new Meta[2];
        for (int slot = 0; slot < 2; slot++) {
            byte[] bytes = store.readAt((long) slot * pageSize, pageSize);
            try { candidates[slot] = parseMeta(bytes); }
            catch (CorruptStorageException ignored) { /* A torn candidate permits fallback. */ }
        }
        int first = candidates[0] == null || (candidates[1] != null
            && Long.compareUnsigned(candidates[1].txid, candidates[0].txid) > 0) ? 1 : 0;
        for (int attempt = 0; attempt < 2; attempt++) {
            int slot = first ^ attempt;
            Meta candidate = candidates[slot], other = candidates[slot ^ 1];
            if (candidate == null || candidate.pageCount > physicalPages) continue;
            Set<Long> dependencies;
            try { dependencies = validateManifest(store, candidate, other); }
            catch (CorruptStorageException ignored) { continue; }
            int floorSlot = slot;
            if (other != null && other.txid == candidate.txid && other.pageCount <= physicalPages) {
                try {
                    validateManifest(store, other, null);
                    floorSlot = (int) (candidate.txid & 1);
                } catch (CorruptStorageException ignored) { /* A matching header is insufficient. */ }
            }
            return new Selected(candidate, slot, floorSlot, dependencies);
        }
        throw corrupt("no complete meta and commit manifest");
    }

    /** Validate one known candidate without fallback, also usable on a planned-write overlay. */
    public static Set<Long> validateManifest(BlockStore store, Meta meta, Meta other) throws IOException {
        ManifestCheck check = new ManifestCheck(store, meta, other);
        int inline = (int) Math.min(meta.manifestEntries, meta.layout().inlineManifestCapacity());
        check.entries(meta.encoded, meta.layout().inlineManifestStart(), inline);
        long id = meta.manifestHead;
        CRC32 chain = new CRC32();
        byte[] index = new byte[4];
        for (long ordinal = 0; ordinal < meta.manifestPages; ordinal++) {
            if (!bodyIndex(id, meta.pageCount, meta.layout()) || id == meta.root || id == meta.freeListHead
                || !check.dependencies.add(id)) throw corrupt("invalid manifest chain pointer");
            byte[] bytes = store.readAt(id * meta.pageSize, meta.pageSize);
            BodyHeader header = parseBody(bytes);
            int count = (int) Math.min(meta.manifestEntries - check.seen, overflowCapacity(meta.pageSize));
            if (header.type() != 8 || header.stamp() != (int) (meta.txid & 0xffff)
                || header.count() != count || u64(bytes, 16) != meta.txid || u32(bytes, 24) != ordinal
                || u32(bytes, 28) != 0 || !zeros(bytes, 32 + count * 8, bytes.length)) {
                throw corrupt("invalid manifest chain page");
            }
            put32(index, 0, id);
            chain.update(index);
            chain.update(bytes);
            check.entries(bytes, 32, count);
            id = header.next();
        }
        if (id != 0 || check.seen != meta.manifestEntries || chain.getValue() != meta.manifestCrc
            || (meta.manifestEntries != 0 && !check.freeHeadSeen)) {
            throw corrupt("incomplete manifest or chain checksum mismatch");
        }
        return check.dependencies;
    }

    private static final class ManifestCheck {
        final BlockStore store;
        final Meta meta;
        final Meta other;
        final Set<Long> dependencies = new HashSet<>();
        long previous;
        long seen;
        boolean freeHeadSeen;

        ManifestCheck(BlockStore store, Meta meta, Meta other) {
            this.store = store;
            this.meta = meta;
            this.other = other;
            this.freeHeadSeen = meta.freeListHead == 0; previous = meta.layout().firstBody() - 1;
        }

        void entries(byte[] bytes, int offset, int count) throws IOException {
            for (int i = 0; i < count; i++, offset += 8) {
                long id = u32(bytes, offset), expectedCrc = u32(bytes, offset + 4);
                if (id <= previous || id >= meta.pageCount || !dependencies.add(id)) {
                    throw corrupt("non-increasing or overlapping manifest entry");
                }
                previous = id;
                byte[] body = store.readAt(id * meta.pageSize, meta.pageSize);
                if (meta.encrypted) {
                    CRC32 crc = new CRC32(); crc.update(body);
                    if (crc.getValue() != expectedCrc) throw corrupt("encrypted manifest dependency mismatch");
                } else if (body[0] < 1 || body[0] > 7 || u32(body, 12) != expectedCrc
                    || bodyCrc(body) != expectedCrc || !validStamp(u16(body, 2))) throw corrupt("manifest dependency mismatch");
                freeHeadSeen |= id == meta.freeListHead;
                seen++;
            }
        }

        boolean validStamp(int stamp) {
            if (other == null || Long.compareUnsigned(other.txid, meta.txid) >= 0) return true;
            long width = meta.txid - other.txid;
            if (Long.compareUnsigned(width, 65536) >= 0) return true;
            int distance = (stamp - (int) other.txid) & 0xffff;
            return distance >= 1 && distance <= width;
        }
    }

    private static boolean bodyIndex(long index, long count, PageLayout layout) { return index >= layout.firstBody() && index < count; }
    private static CorruptStorageException corrupt(String message) { return new CorruptStorageException(message); }
}
