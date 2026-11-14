package org.jed.storage;

import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.zip.CRC32;

import static org.jed.storage.PageFormat.*;

/**
 * Exact v53 manifest and unsealed meta encoding after allocation has selected page ids.
 * No I/O occurs here: serialization failures precede the storage mutation seam.
 */
public final class ManifestEncoder {
    private ManifestEncoder() { }

    public record Entry(long page, long crc) { }
    public record Write(long page, byte[] bytes) {
        public Write { bytes = bytes.clone(); }
        @Override public byte[] bytes() { return bytes.clone(); }
    }
    public record Encoded(byte[] meta, List<Write> overflow) {
        public Encoded { meta = meta.clone(); overflow = List.copyOf(overflow); }
        @Override public byte[] meta() { return meta.clone(); }
    }

    /** Entries and remaps must already be in their canonical ascending order. */
    public static Encoded encode(int pageSize, long txid, long root, long pageCount,
                                 long freeHead, long livePages, List<Entry> entries,
                                 long[] overflowIds, List<PlaintextRecovery.Remap> remaps)
        throws CorruptStorageException {
        return encode(new PageLayout(pageSize, false), txid, root, new byte[0], pageCount, freeHead, new byte[0], livePages, entries, overflowIds, remaps);
    }
    public static Encoded encode(PageLayout layout, long txid, long root, byte[] rootTag, long pageCount,
                                 long freeHead, byte[] freeTag, long livePages, List<Entry> entries,
                                 long[] overflowIds, List<PlaintextRecovery.Remap> remaps) throws CorruptStorageException {
        int pageSize = layout.pageSize();
        if (rootTag.length != layout.tagBytes() || freeTag.length != layout.tagBytes()) throw new IllegalArgumentException("invalid meta pointer tag width");
        if (freeHead == 0 && !zeros(freeTag, 0, freeTag.length)) throw new IllegalArgumentException("nonzero absent free-list tag");
        if (pageCount <= layout.firstBody() || pageCount > 0xffffffffL || root < layout.firstBody() || root >= pageCount
            || freeHead < 0 || (freeHead != 0 && (freeHead < layout.firstBody() || freeHead >= pageCount))
            || livePages < 0 || livePages > pageCount - layout.firstBody()
            || remaps.size() > layout.remapCapacity()) throw new IllegalArgumentException("invalid meta bounds");
        int inline = Math.min(entries.size(), layout.inlineManifestCapacity());
        if ((long) overflowIds.length + entries.size() > pageCount - layout.firstBody()
            || entries.size() > inline + (long) overflowIds.length * layout.overflowManifestCapacity()
            || (entries.isEmpty() && overflowIds.length != 0)) {
            throw new IllegalArgumentException("invalid manifest allocation");
        }
        Set<Long> used = new HashSet<>();
        long previous = layout.firstBody() - 1;
        boolean freeListed = freeHead == 0;
        for (Entry entry : entries) {
            if (entry.page <= previous || entry.page >= pageCount || entry.crc < 0 || entry.crc > 0xffffffffL) {
                throw new IllegalArgumentException("invalid ordered manifest entry");
            }
            used.add(entry.page);
            previous = entry.page;
            freeListed |= entry.page == freeHead;
        }
        if (!entries.isEmpty() && !freeListed) throw new IllegalArgumentException("manifest omits free-list head");
        for (long id : overflowIds) {
            if (id < layout.firstBody() || id >= pageCount || id == root || id == freeHead || !used.add(id)) {
                throw new IllegalArgumentException("invalid or overlapping manifest chain page");
            }
        }
        byte[] meta = new byte[pageSize];
        put32(meta, 0, 0x4a454442L);
        put16(meta, 4, VERSION);
        meta[6] = (byte) (layout.encrypted() ? 1 : 0); meta[7] = (byte) layout.reserve();
        if (layout.encrypted()) { System.arraycopy(rootTag, 0, meta, 64, 16); System.arraycopy(freeTag, 0, meta, 80, 16); }
        put32(meta, 8, pageSize);
        put64(meta, 12, txid);
        put32(meta, 20, root);
        put32(meta, 24, pageCount);
        put32(meta, 28, freeHead);
        put32(meta, 36, overflowIds.length == 0 ? 0 : overflowIds[0]);
        put32(meta, 40, entries.size());
        put32(meta, 44, overflowIds.length);
        put32(meta, 56, livePages);
        put32(meta, 60, remaps.size());
        for (int i = 0; i < inline; i++) writeEntry(meta, layout.inlineManifestStart() + i * 8, entries.get(i));
        List<Write> overflow = new ArrayList<>();
        CRC32 chain = new CRC32();
        byte[] index = new byte[4];
        int consumed = inline;
        for (int ordinal = 0; ordinal < overflowIds.length; ordinal++) {
            int count = Math.min(entries.size() - consumed, layout.overflowManifestCapacity());
            byte[] page = new byte[pageSize];
            page[0] = 8;
            put16(page, 2, (int) (txid & 0xffff));
            put32(page, 4, count);
            put32(page, 8, ordinal + 1 == overflowIds.length ? 0 : overflowIds[ordinal + 1]);
            put64(page, 16, txid);
            put32(page, 24, ordinal);
            for (int i = 0; i < count; i++) writeEntry(page, 32 + i * 8, entries.get(consumed++));
            put32(page, 12, bodyCrc(page));
            put32(index, 0, overflowIds[ordinal]);
            chain.update(index);
            chain.update(page);
            overflow.add(new Write(overflowIds[ordinal], page));
        }
        put64(meta, 48, chain.getValue());
        int at = layout.remapStart();
        for (var remap : remaps) {
            put32(meta, at, remap.parent());
            put16(meta, at + 4, remap.slot());
            put32(meta, at + 6, remap.page());
            if (remap.tag().length != layout.tagBytes()) throw new IllegalArgumentException("invalid remap tag width");
            System.arraycopy(remap.tag(), 0, meta, at + 10, layout.tagBytes());
            at += 10 + layout.tagBytes();
        }
        put32(meta, 32, metaCrc(meta));
        // The decoder checks remap sorting/distinct targets and every descriptor
        // bound. Do this before returning bytes that could reach a device.
        PlaintextRecovery.parseMeta(meta);
        return new Encoded(meta, overflow);
    }

    private static void writeEntry(byte[] bytes, int at, Entry entry) {
        put32(bytes, at, entry.page);
        put32(bytes, at + 4, entry.crc);
    }
}
