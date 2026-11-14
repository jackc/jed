package org.jed.storage;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.Map;
import java.util.zip.CRC32;

/** Byte-level shared fixtures, not SQL semantic substitutes. */
public final class PlaintextRecoveryTest {
    public static void main(String[] args) throws Exception {
        Path fixtures = Path.of(args.length == 0 ? "spec/fileformat/fixtures" : args[0]);
        CRC32 crc = new CRC32();
        crc.update("123456789".getBytes(StandardCharsets.US_ASCII));
        check(crc.getValue() == 0xcbf43926L, "CRC-32/IEEE vector");
        Map<String, Long> expected = Map.ofEntries(
            Map.entry("inline", 2L), Map.entry("overflow", 2L), Map.entry("empty_tail", 2L),
            Map.entry("missing_root", 2L), Map.entry("torn_body", 1L), Map.entry("stale_body", 1L),
            Map.entry("stale_orphan", 1L), Map.entry("torn_meta", 1L), Map.entry("torn_manifest", 1L),
            Map.entry("wrong_manifest_txid", 1L), Map.entry("manifest_cycle", 1L),
            Map.entry("duplicate_entry", 1L), Map.entry("out_of_bounds", 1L),
            Map.entry("root_manifest", 1L), Map.entry("free_head_manifest", 1L),
            Map.entry("bad_padding", 1L), Map.entry("invalid_free_list", 2L),
            Map.entry("deferred_retained", 2L), Map.entry("deferred_overwrite", 3L),
            Map.entry("deferred_fold", 3L), Map.entry("deferred_retire", 3L));
        for (var fixture : expected.entrySet()) {
            try (BlockStore store = FileBlockStore.open(fixtures.resolve("cow_" + fixture.getKey() + ".jed"), false)) {
                var selected = PlaintextRecovery.recover(store);
                check(selected.meta().txid() == fixture.getValue(), "recovered generation: " + fixture.getKey());
                // invalid_free_list intentionally selects generation 2: its later
                // semantic free-list failure must not fall back to generation 1.
                check(selected.needsStabilization() == (fixture.getValue() > 1), "stabilization flag");
            }
        }
        for (String name : new String[] {"empty_db", "pk_table", "catalog", "overflow_table", "index_table", "rekey_index_table", "cow_deferred_retained"}) {
            try (BlockStore store = FileBlockStore.open(fixtures.resolve("encrypted_" + name + ".jed"), false)) {
                var selected = PlaintextRecovery.recover(store);
                long expectedTxid = name.equals("rekey_index_table") ? 155 : name.equals("cow_deferred_retained") ? 2 : 1;
                check(selected.meta().txid() == expectedTxid, "encrypted recovered generation: " + name);
                check(selected.meta().encrypted() && selected.meta().rootTag().length == 16, "encrypted layout tags");
            }
        }
        try (BlockStore store = FileBlockStore.open(fixtures.resolve("cow_inline.jed"), false)) {
            BlockStore broken = new BlockStore() {
                public byte[] readAt(long at, int length) throws IOException {
                    if (at >= 512) throw new IOException("injected host read failure");
                    return store.readAt(at, length);
                }
                public void writeAt(long at, byte[] bytes) { throw new AssertionError(); }
                public void sync() { throw new AssertionError(); }
                public long size() throws IOException { return store.size(); }
                public void setSize(long size) { throw new AssertionError(); }
                public void close() { }
            };
            try {
                PlaintextRecovery.recover(broken);
                throw new AssertionError("host I/O error incorrectly allowed fallback");
            } catch (IOException e) {
                check(e.getMessage().equals("injected host read failure"), "I/O error identity");
            }
        }
        System.out.println("PlaintextRecoveryTest passed (28 shared recovery fixtures)");
    }

    private static void check(boolean condition, String message) {
        if (!condition) throw new AssertionError(message);
    }
}
