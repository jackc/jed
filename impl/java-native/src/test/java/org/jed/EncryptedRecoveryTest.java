package org.jed;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.List;
import org.jed.storage.*;

/** Authenticated publication and keyless recovery against the shared encrypted history. */
public final class EncryptedRecoveryTest {
    public static void main(String[] args) throws Exception {
        Path root = Path.of(args.length == 0 ? "spec/fileformat" : args[0]);
        String vectors = Files.readString(root.resolve("encryption_vectors.toml"));
        var matcher = java.util.regex.Pattern.compile("(?s)\\[host_kek]\\s*key_hex\\s*=\\s*\"([0-9a-f]+)\"").matcher(vectors);
        if (!matcher.find()) throw new AssertionError("shared host-key vector absent");
        Credential credential = Credential.hostKey(HexFormat.of().parseHex(matcher.group(1)));
        byte[] prior = Files.readAllBytes(root.resolve("fixtures/encrypted_pk_table.jed"));
        byte[] after = Files.readAllBytes(root.resolve("fixtures/encrypted_cow_deferred_retained.jed"));
        int size = (int) PageFormat.u32(prior, 8);
        var selectedKey = KeyBlock.select(Arrays.copyOfRange(prior, 2 * size, 3 * size), Arrays.copyOfRange(prior, 3 * size, 4 * size));
        var codec = new EncryptedPageCodec(KeyBlock.unlock(selectedKey, credential), ignored -> { throw new AssertionError("unexpected entropy draw"); });
        byte[] meta = Arrays.copyOf(after, size);
        long id = PageFormat.u32(meta, 96);
        var writes = List.of(new ManifestEncoder.Write(id, Arrays.copyOfRange(after, (int) id * size, ((int) id + 1) * size)));
        for (int failAt = 0; failAt <= 4; failAt++) {
            try (MemoryBlockStore memory = new MemoryBlockStore(size)) {
                memory.writeAt(0, prior);
                var fault = new FaultStore(memory, size, failAt);
                try (Pager pager = new Pager(fault, size, size, false, codec)) {
                    var current = PlaintextRecovery.recover(fault);
                    codec.authenticateMeta(current.slot(), current.meta().encoded(), List.of());
                    var writer = new DurableCommitter(pager, current, null, codec::authenticateMeta);
                    boolean failed = false;
                    try { writer.commit(meta, writes, ignored -> true); }
                    catch (IOException error) { failed = true; check(error.getMessage().equals("injected encrypted storage failure"), "fault identity"); }
                    check(failed == (failAt != 0) && pager.poisoned() == failed, "mutation failure poison");
                    var recovered = PlaintextRecovery.recover(memory);
                    check(recovered.meta().txid() == (failAt == 0 || failAt == 4 ? 2 : 1), "encrypted crash selection");
                    codec.authenticateMeta(recovered.slot(), recovered.meta().encoded(), List.of());
                    if (!failed) {
                        check(fault.events.equals(List.of("resize", "write:" + id, "write:0", "sync")), "encrypted publication ordering");
                        check(Arrays.equals(memory.readAt(0, after.length), after), "exact shared encrypted commit bytes");
                    }
                }
            }
        }
        try (MemoryBlockStore memory = new MemoryBlockStore(size)) {
            memory.writeAt(0, prior);
            var fault = new FaultStore(memory, size, 0);
            try (Pager pager = new Pager(fault, size, size, false, codec)) {
                var current = PlaintextRecovery.recover(fault);
                try { new DurableCommitter(pager, current, null); throw new AssertionError("missing meta authenticator admitted"); }
                catch (IllegalArgumentException expected) { }
                var writer = new DurableCommitter(pager, current, null, codec::authenticateMeta);
                byte[] tampered = meta.clone(); tampered[tampered.length - 1] ^= 1;
                PageFormat.put32(tampered, 32, PageFormat.metaCrc(tampered));
                try { writer.commit(tampered, writes, ignored -> true); throw new AssertionError("tampered publication admitted"); }
                catch (SqlException expected) { check(expected.sqlState().equals("XX001"), "authentication error state"); }
                check(!pager.poisoned() && fault.events.isEmpty(), "authentication precedes storage mutation");
                writer.commit(meta, writes, ignored -> true);
                // CRC-valid tampering remains the selected generation. The holder
                // authenticates that selection and must report XX001, never retry.
                memory.writeAt(0, tampered);
                var selected = PlaintextRecovery.recover(memory);
                check(selected.meta().txid() == 2, "keyless selection does not hide tag failure");
                try { codec.authenticateMeta(selected.slot(), selected.meta().encoded(), List.of()); throw new AssertionError("selected tag mismatch accepted"); }
                catch (SqlException expected) { check(expected.sqlState().equals("XX001"), "selected authentication state"); }
            }
        }
        System.out.println("EncryptedRecoveryTest passed (exact shared commit, 4 mutation faults, authentication before writes)");
    }
    private static final class FaultStore implements BlockStore {
        final BlockStore source; final int size, failAt; final List<String> events = new ArrayList<>();
        FaultStore(BlockStore source, int size, int failAt) { this.source = source; this.size = size; this.failAt = failAt; }
        void event(String value) throws IOException { events.add(value); if (events.size() == failAt) throw new IOException("injected encrypted storage failure"); }
        public byte[] readAt(long offset, int length) throws IOException { return source.readAt(offset, length); }
        public long size() throws IOException { return source.size(); }
        public void writeAt(long offset, byte[] bytes) throws IOException { event("write:" + offset / size); source.writeAt(offset, bytes); }
        public void setSize(long size) throws IOException { event("resize"); source.setSize(size); }
        public void sync() throws IOException { event("sync"); source.sync(); }
        public void close() { }
    }
    private static void check(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
}
