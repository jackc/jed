package org.jed;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import org.jed.storage.*;

/** Host credential changes preserve data pages and survive every key-copy write boundary. */
public final class CredentialChangeTest {
    public static void main(String[] args) throws Exception {
        byte[] firstKey = new byte[32], nextKey = new byte[32]; Arrays.fill(nextKey, (byte) 7);
        Credential old = Credential.hostKey(firstKey), next = Credential.hostKey(nextKey);
        byte[] dek = new byte[32]; Arrays.fill(dek, (byte) 11);
        byte[] before = KeyBlock.wrap(1024, 1, dek, old, CryptoTest.seeded(1));
        byte[] after = KeyBlock.wrap(1024, 2, dek, next, CryptoTest.seeded(2));
        for (int failAt = 0; failAt <= 6; failAt++) {
            try (var raw = new MemoryBlockStore(1024)) {
                raw.writeAt(2048, before); raw.writeAt(3072, before);
                var fault = new FaultStore(raw, failAt);
                try (var pager = new Pager(fault, 1024, 1024, false)) {
                    boolean failed = false;
                    try { KeyBlock.publishChange(pager, KeyBlock.select(before, before), after, () -> fault.event("directory")); }
                    catch (IOException failure) { failed = true; }
                    check(failed == (failAt != 0) && pager.poisoned() == failed, "key change fault poison");
                    var selected = KeyBlock.select(raw.readAt(2048, 1024), raw.readAt(3072, 1024));
                    Credential winner = selected.generation() == 1 ? old : next, loser = selected.generation() == 1 ? next : old;
                    check(Arrays.equals(KeyBlock.unlock(selected, winner), dek), "recoverable data key");
                    expect("28P01", () -> KeyBlock.unlock(selected, loser));
                    if (!failed) check(fault.events.equals(List.of("sync", "directory", "write:3", "sync", "write:2", "sync")), "non-selected first and barrier ordering");
                }
            }
        }
        try (var raw = new MemoryBlockStore(1024)) {
            raw.writeAt(2048, before); raw.writeAt(3072, before);
            var fault = new FaultStore(raw, 0);
            try (var pager = new Pager(fault, 1024, 1024, false)) {
                KeyBlock.publishChange(pager, KeyBlock.select(before, before), after, null);
                check(fault.events.equals(List.of("sync", "write:3", "sync", "write:2", "sync")), "subsequent key change retains its file stabilization barrier");
            }
        }
        Path directory = Files.createTempDirectory("jed-key-change-");
        try {
            Path path = directory.resolve("database.jed");
            try (var db = Database.createExclusive(path, 1024, 1024, old); var session = db.session()) {
                execute(session, "CREATE TABLE t (id i32 PRIMARY KEY, value text)"); execute(session, "INSERT INTO t VALUES (1, 'retained secret')");
                expect("42704", () -> db.changeCredential("missing", next));
                execute(session, "BEGIN"); expect("55006", () -> db.changeCredential(next)); execute(session, "ROLLBACK");
                byte[] bytes = Files.readAllBytes(path); db.changeCredential(next); byte[] changed = Files.readAllBytes(path);
                check(Arrays.equals(Arrays.copyOf(bytes, 2048), Arrays.copyOf(changed, 2048))
                    && Arrays.equals(Arrays.copyOfRange(bytes, 4096, bytes.length), Arrays.copyOfRange(changed, 4096, changed.length)), "credential change rewrote meta/body pages");
                try (Rows rows = session.query("SELECT value FROM t")) { check(rows.next().get(0).asText().equals("retained secret"), "reader survives credential change"); }
            }
            expect("28P01", () -> Database.openExclusive(path, 1024, old));
            try (var db = Database.openExclusive(path, 1024, next)) { db.changeCredential(Credential.passphrase("new passphrase", new Credential.ArgonParameters(32, 1, 1))); }
            try (var db = Database.openExclusive(path, 1024, Credential.passphrase("new passphrase"))) { check(db.encrypted(), "passphrase rewrap"); }
            try (var db = Database.memory()) { expect("55000", () -> db.changeCredential(next)); }
            try (var db = Database.createExclusive(directory.resolve("plain.jed"), 1024, 1024)) { expect("28P03", () -> db.changeCredential(next)); }
            derivationGate(directory.resolve("concurrent.jed"), old, next);
            busyDerivation(directory.resolve("busy.jed"), old, next);
            retainedPromotion(directory.resolve("promoted.jed"), old, next);
        } finally { try (var paths = Files.walk(directory)) { for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(path); } }
        System.out.println("CredentialChangeTest: passed 6 torn-write/barrier faults, host state, page preservation, passphrase and derivation gate");
    }
    private static void retainedPromotion(Path path, Credential old, Credential next) throws Exception {
        try (var created = Database.createExclusive(path, 1024, 1024, old)) { check(created.encrypted(), "encrypted promotion setup"); }
        EncryptedPageCodec retained; byte[] replacement;
        try (var original = FileDatabase.openHost(null, FileHost.NATIVE, path, 1024, old, Environment.empty(), false, false, null)) {
            retained = original.retainedCodec(); replacement = original.prepareCredential(next).keyBlock();
        }
        // The first key-copy write reached storage before the former owner died.
        try (var store = FileBlockStore.open(path, true)) { store.writeAt(3072, replacement); store.sync(); }
        byte[] interrupted = Files.readAllBytes(path);
        try (var promoted = FileDatabase.openHost(null, FileHost.NATIVE, path, 1024, null, Environment.empty(), false, true, retained)) {
            check(Arrays.equals(promoted.currentKeyBlock(), replacement), "promotion adopts selected key block");
            check(Arrays.equals(interrupted, Files.readAllBytes(path)), "retained-DEK promotion must not repair credential bytes");
        }
        try (var reopened = Database.openExclusive(path, 1024, next)) { check(reopened.encrypted(), "credential-open repairs interrupted pair"); }
        byte[] repaired = Files.readAllBytes(path);
        check(Arrays.equals(Arrays.copyOfRange(repaired, 2048, 3072), Arrays.copyOfRange(repaired, 3072, 4096)), "credential-open did not repair selected key block");
    }
    private static void busyDerivation(Path path, Credential old, Credential next) throws Exception {
        var bytesDrawn = new java.util.concurrent.atomic.AtomicLong();
        Environment environment = Environment.builder().keyEntropy(() -> {
            var source = CryptoTest.seeded(12);
            return bytes -> { bytesDrawn.addAndGet(bytes.length); source.accept(bytes); };
        }).build();
        try (var db = Database.createExclusive(path, 1024, 1024, old, environment); var session = db.session()) {
            session.execute("BEGIN"); long before = bytesDrawn.get();
            expect("55006", () -> db.changeCredential(next));
            check(bytesDrawn.get() - before == 44, "busy credential change derives its salt and nonce before testing the writer gate");
            before = bytesDrawn.get(); expect("55006", () -> db.rekey(next));
            check(bytesDrawn.get() - before == 76, "busy rekey derives its data key and key block before testing the writer gate");
            session.execute("ROLLBACK");
        }
    }
    private static void derivationGate(Path path, Credential old, Credential next) throws Exception {
        CountDownLatch entered = new CountDownLatch(1), release = new CountDownLatch(1);
        var block = new java.util.concurrent.atomic.AtomicBoolean();
        Environment environment = Environment.builder().keyEntropy(() -> {
            var source = CryptoTest.seeded(8);
            return bytes -> { if (block.compareAndSet(true, false)) { entered.countDown(); try { if (!release.await(5, TimeUnit.SECONDS)) throw new AssertionError("key derivation wait timeout"); } catch (InterruptedException failure) { throw new AssertionError(failure); } } source.accept(bytes); };
        }).build();
        try (var db = Database.createExclusive(path, 1024, 1024, old, environment); var session = db.session()) {
            block.set(true);
            var outcome = new java.util.concurrent.CompletableFuture<Throwable>();
            Thread change = Thread.ofPlatform().start(() -> { try { db.changeCredential(next); outcome.complete(null); } catch (Throwable failure) { outcome.complete(failure); } });
            check(entered.await(5, TimeUnit.SECONDS), "entered derivation");
            try { execute(session, "SELECT 1"); } finally { release.countDown(); }
            Throwable failure = outcome.get(5, TimeUnit.SECONDS); change.join(); if (failure != null) throw new AssertionError(failure);
        }
    }
    private static final class FaultStore implements BlockStore {
        final BlockStore raw; final int failAt; final List<String> events = new ArrayList<>();
        FaultStore(BlockStore raw, int failAt) { this.raw = raw; this.failAt = failAt; }
        void event(String name) throws IOException { events.add(name); if (events.size() == failAt) throw new IOException("key change fault"); }
        public byte[] readAt(long offset, int count) throws IOException { return raw.readAt(offset, count); }
        public void writeAt(long offset, byte[] bytes) throws IOException {
            if (events.size() + 1 == failAt) raw.writeAt(offset, Arrays.copyOf(bytes, 64)); // Tear the key header before its CRC.
            event("write:" + offset / 1024); raw.writeAt(offset, bytes);
        }
        public long size() throws IOException { return raw.size(); }
        public void setSize(long length) throws IOException { raw.setSize(length); }
        public void sync() throws IOException { event("sync"); }
        public void close() { }
    }
    private static void execute(Session session, String sql) { try (Rows rows = session.query(sql)) { while (rows.next() != null) { } } }
    private interface Action { void run() throws Exception; }
    private static void expect(String state, Action action) throws Exception { try { action.run(); throw new AssertionError("expected " + state); } catch (SqlException failure) { check(failure.sqlState().equals(state), "expected " + state + ", got " + failure.sqlState()); } }
    private static void check(boolean value, String message) { if (!value) throw new AssertionError(message); }
}
