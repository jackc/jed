package org.jed.storage;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

public final class DurableCommitterTest {
    public static void main(String[] args) throws Exception {
        Path fixtures = Path.of(args.length == 0 ? "spec/fileformat/fixtures" : args[0]);
        byte[] fixture = Files.readAllBytes(fixtures.resolve("cow_inline.jed"));
        byte[] prior = Arrays.copyOf(fixture, 4 * 256);
        System.arraycopy(prior, 256, prior, 0, 256); // Both slots hold complete txid 1.
        byte[] meta = Arrays.copyOfRange(fixture, 0, 256);
        List<ManifestEncoder.Write> writes = List.of(
            new ManifestEncoder.Write(4, Arrays.copyOfRange(fixture, 4 * 256, 5 * 256)),
            new ManifestEncoder.Write(5, Arrays.copyOfRange(fixture, 5 * 256, 6 * 256)));

        for (int failAt = 0; failAt <= 5; failAt++) {
            try (MemoryBlockStore memory = new MemoryBlockStore(256)) {
                memory.writeAt(0, prior);
                var fault = new FaultStore(memory, failAt);
                try (Pager pager = new Pager(fault, 256, 256, false)) {
                    var current = PlaintextRecovery.recover(fault);
                    var writer = new DurableCommitter(pager, current, null);
                    int alternate = writer.current().floorSlot();
                    writer.coveredByBarrier();
                    check(writer.current().floorSlot() == alternate, "credential barrier preserves initial alternate meta slot");
                    boolean failed = false;
                    try { writer.commit(meta, writes, ignored -> true); }
                    catch (IOException e) { failed = true; check(e.getMessage().equals("injected storage failure"), "fault identity"); }
                    check(failed == (failAt != 0), "injection reached");
                    check(pager.poisoned() == failed, "failed mutation poisons");
                    var recovered = PlaintextRecovery.recover(memory);
                    check(recovered.meta().txid() == (failAt == 0 || failAt == 5 ? 2 : 1), "correct crash fallback");
                    if (!failed) {
                        check(fault.events.equals(List.of("resize", "write:4", "write:5", "write:0", "sync")),
                            "body, alternate meta, one barrier ordering");
                        check(Arrays.equals(memory.readAt(0, fixture.length), fixture), "exact shared fixture output");
                    } else {
                        int previous = fault.events.size();
                        try { writer.commit(meta, writes, ignored -> true); throw new AssertionError("writer reused after failure"); }
                        catch (IOException expected) { check(fault.events.size() == previous, "poison fails before host work"); }
                    }
                }
            }
        }
        // A serialization/unsafe-allocation rejection must perform no host work
        // and must leave the writer usable for a subsequent correct commit.
        try (MemoryBlockStore memory = new MemoryBlockStore(256)) {
            memory.writeAt(0, prior);
            var fault = new FaultStore(memory, 0);
            try (Pager pager = new Pager(fault, 256, 256, false)) {
                var writer = new DurableCommitter(pager, PlaintextRecovery.recover(fault), null);
                try {
                    writer.commit(meta, List.of(new ManifestEncoder.Write(2, writes.getFirst().bytes())), ignored -> true);
                    throw new AssertionError("live-page overwrite accepted");
                } catch (IllegalArgumentException expected) {
                    check(!pager.poisoned() && fault.events.isEmpty(), "planning error touched storage");
                }
                writer.commit(meta, writes, ignored -> true);
            }
        }
        relaxed(prior, meta, writes);
        System.out.println("DurableCommitterTest passed (exact shared fixture, 5 storage failure points, cumulative relaxed recovery and barrier revocation)");
    }
    private static void relaxed(byte[] prior, byte[] meta, List<ManifestEncoder.Write> writes) throws Exception {
        try (MemoryBlockStore memory = new MemoryBlockStore(256)) {
            memory.writeAt(0, prior); FaultStore traced = new FaultStore(memory, 0);
            try (Pager pager = new Pager(traced, 256, 256, false)) {
                var writer = new DurableCommitter(pager, PlaintextRecovery.recover(memory), null);
                writer.commit(meta, writes, ignored -> true, false);
                check(!traced.events.contains("sync") && writer.pendingEntries().size() == 2, "relaxed commit deferred barrier");
                byte[] nextBody = writes.getFirst().bytes(); PageFormat.put16(nextBody, 2, 3); PageFormat.put32(nextBody, 12, PageFormat.bodyCrc(nextBody));
                var identities = new ArrayList<>(writer.pendingEntries()); identities.add(new ManifestEncoder.Entry(6, PageFormat.bodyCrc(nextBody)));
                var next = ManifestEncoder.encode(256, 3, 6, 7, 0, 2, identities, new long[0], List.of());
                writer.commit(next.meta(), List.of(new ManifestEncoder.Write(6, nextBody)), ignored -> true, false);
                check(writer.current().floorSlot() == 1 && writer.current().slot() == 0, "batch preserves original floor slot");
                check(Arrays.equals(memory.readAt(256, 256), Arrays.copyOfRange(prior, 256, 512)), "durable floor overwritten");
                check(PlaintextRecovery.recover(memory).meta().txid() == 3, "complete relaxed generation recovers");
                byte[] intact = memory.readAt(5L * 256, 256), corrupt = intact.clone(); corrupt[30] ^= 1; memory.writeAt(5L * 256, corrupt);
                check(PlaintextRecovery.recover(memory).meta().txid() == 1, "cumulative manifest rejects missing prior-batch page");
                memory.writeAt(5L * 256, intact); writer.flush(false);
                check(writer.pendingEntries().isEmpty() && writer.current().floorSlot() == 0, "flush advances durable floor");
                check(traced.events.stream().filter("sync"::equals).count() == 1, "one barrier covers batch");
            }
        }
        try (MemoryBlockStore memory = new MemoryBlockStore(256)) {
            memory.writeAt(0, prior); FaultStore fault = new FaultStore(memory, 5);
            try (Pager pager = new Pager(fault, 256, 256, false)) {
                var writer = new DurableCommitter(pager, PlaintextRecovery.recover(memory), null);
                writer.commit(meta, writes, ignored -> true, false);
                try { writer.flush(false); throw new AssertionError("failed relaxed flush accepted"); } catch (IOException expected) { }
                check(pager.revoked(), "failed flush must revoke readers too");
                try { pager.readRaw(4); throw new AssertionError("read after failed flush"); } catch (IOException expected) { }
            }
        }
    }

    private static final class FaultStore implements BlockStore {
        final BlockStore source;
        final int failAt;
        final List<String> events = new ArrayList<>();
        FaultStore(BlockStore source, int failAt) { this.source = source; this.failAt = failAt; }
        private void event(String event) throws IOException {
            events.add(event);
            if (events.size() == failAt) throw new IOException("injected storage failure");
        }
        public byte[] readAt(long offset, int length) throws IOException { return source.readAt(offset, length); }
        public void writeAt(long offset, byte[] bytes) throws IOException { event("write:" + offset / 256); source.writeAt(offset, bytes); }
        public void sync() throws IOException { event("sync"); source.sync(); }
        public long size() throws IOException { return source.size(); }
        public void setSize(long length) throws IOException { event("resize"); source.setSize(length); }
        public void close() { }
    }

    private static void check(boolean condition, String message) {
        if (!condition) throw new AssertionError(message);
    }
}
