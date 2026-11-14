package org.jed.storage;

import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;

/** Independent Ruby-authored goldens pin the encoder, including empty chain tails. */
public final class ManifestEncoderTest {
    public static void main(String[] args) throws Exception {
        Path fixtures = Path.of(args.length == 0 ? "spec/fileformat/fixtures" : args[0]);
        for (String name : new String[] {"cow_inline", "cow_overflow", "cow_empty_tail", "cow_missing_root",
            "cow_deferred_retained", "cow_deferred_overwrite", "cow_deferred_fold", "cow_deferred_retire",
            "encrypted_empty_db", "encrypted_pk_table", "encrypted_catalog", "encrypted_overflow_table",
            "encrypted_index_table", "encrypted_rekey_index_table", "encrypted_cow_deferred_retained"}) {
            try (BlockStore store = FileBlockStore.open(fixtures.resolve(name + ".jed"), false)) {
                var meta = PlaintextRecovery.recover(store).meta();
                var entries = new ArrayList<ManifestEncoder.Entry>();
                byte[] bytes = meta.encoded();
                int inline = (int) Math.min(meta.manifestEntries(), meta.layout().inlineManifestCapacity());
                for (int i = 0; i < inline; i++) add(entries, bytes, meta.layout().inlineManifestStart() + i * 8);
                var chain = new ArrayList<Long>();
                for (long id = meta.manifestHead(); id != 0;) {
                    chain.add(id);
                    bytes = store.readAt(id * meta.pageSize(), meta.pageSize());
                    var header = PageFormat.parseBody(bytes);
                    for (int i = 0; i < header.count(); i++) add(entries, bytes, 32 + i * 8);
                    id = header.next();
                }
                long[] ids = chain.stream().mapToLong(Long::longValue).toArray();
                var result = ManifestEncoder.encode(meta.layout(), meta.txid(), meta.root(), meta.rootTag(), meta.pageCount(),
                    meta.freeListHead(), meta.freeTag(), meta.livePages(), entries, ids, meta.remaps());
                byte[] expectedMeta = meta.encoded();
                if (meta.encrypted()) {
                    // Encoding precedes AEAD sealing; the separate crypto golden
                    // test pins the resulting nonce, tag and stored checksum.
                    Arrays.fill(expectedMeta, meta.layout().bodyEnd(), expectedMeta.length, (byte) 0);
                    PageFormat.put32(expectedMeta, 32, PageFormat.metaCrc(expectedMeta));
                }
                check(Arrays.equals(expectedMeta, result.meta()), name + " exact unsealed meta bytes");
                for (var write : result.overflow()) {
                    check(Arrays.equals(write.bytes(), store.readAt(write.page() * meta.pageSize(), meta.pageSize())),
                        name + " exact manifest bytes");
                }
            }
        }
        System.out.println("ManifestEncoderTest passed (15 shared histories, exact unsealed meta/manifest bytes)");
    }

    private static void add(ArrayList<ManifestEncoder.Entry> entries, byte[] bytes, int at) {
        entries.add(new ManifestEncoder.Entry(PageFormat.u32(bytes, at), PageFormat.u32(bytes, at + 4)));
    }
    private static void check(boolean condition, String message) {
        if (!condition) throw new AssertionError(message);
    }
}
