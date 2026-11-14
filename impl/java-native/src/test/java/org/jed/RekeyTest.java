package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import org.jed.storage.*;

/** Shared rekey image plus host admission, scratch spill, rollback and credential lifecycle. */
public final class RekeyTest {
    public static void main(String[] args) throws Exception {
        Path fixtures = Path.of(args.length == 0 ? "spec/fileformat/fixtures" : args[0]);
        Path directory = Files.createTempDirectory("jed-rekey-");
        byte[] key = new byte[32]; Arrays.fill(key, (byte) 0x4a); Credential credential = Credential.hostKey(key);
        Environment environment = Environment.builder().keyEntropy(() -> CryptoTest.seeded(1246053442)).build();
        try {
            Path source = directory.resolve("source.jed"), encrypted = directory.resolve("encrypted.jed");
            Files.copy(fixtures.resolve("encrypted_index_table.jed"), encrypted);
            try (var original = FileDatabase.openExclusive(encrypted, 1024, credential, environment);
                 var scratch = new RekeyScratch(1024, false, 1024, environment.newKeyEntropyStream());
                 var output = FileBlockStore.createNew(source)) {
                var rebuilt = scratch.rebuild(original.snapshot());
                var image = ImageWriter.write(rebuilt, 1024, 154, PageCodec.PLAINTEXT, null, (page, bytes) -> output.writeAt(page * 1024, bytes));
                output.setSize(image.pageCount() * 1024); output.sync();
            }
            byte[] before = Files.readAllBytes(source);
            try (Database database = Database.open(source, FileOptions.builder().serve(false).cacheBytes(1024).environment(environment).build())) {
                expect("55000", () -> database.rekey(credential));
            }
            check(Arrays.equals(before, Files.readAllBytes(source)), "unprotected scratch admission changed source");
            try (Database database = Database.open(source, FileOptions.builder().serve(false).cacheBytes(1024).protectedScratch(true).environment(environment).build())) {
                try (Database.Pin pin = database.pinSnapshot()) { expect("55006", () -> database.rekey(credential)); }
                database.setMaxStorageBytes(1024); expect("54P06", () -> database.rekey(credential));
                check(Arrays.equals(before, Files.readAllBytes(source)), "rekey limit failure changed source");
            }
            // A new handle restarts the injected key stream after the failed attempt.
            try (Database database = Database.open(source, FileOptions.builder().serve(false).cacheBytes(1024).protectedScratch(true).environment(environment).build())) {
                database.rekey(credential);
                check(database.encrypted() && database.processTxid() == 155, "encryption publication");
                check(Arrays.equals(Files.readAllBytes(source), Files.readAllBytes(fixtures.resolve("encrypted_rekey_index_table.jed"))), "rekey differs from canonical encrypted image");
                database.rekey(null); check(!database.encrypted(), "decryption publication");
                database.rekey(credential); check(database.encrypted(), "protected scratch survives decryption");
            }
            try (Database database = Database.openExclusive(source, 1024, credential)) {
                check(database.snapshot().tables().values().iterator().next().rows().size() == 150, "rekey row inventory");
            }
            Path parity = directory.resolve("parity.jed");
            try (Database database = Database.createExclusive(parity, 1024, 1024, credential, environment)) {
                database.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
                database.rekey(credential); check(database.processTxid() == 3, "odd compacted generation");
                database.changeCredential(credential);
                database.execute("INSERT INTO t VALUES (1)");
                byte[] bytes = Files.readAllBytes(parity);
                check(PlaintextRecovery.parseMeta(Arrays.copyOfRange(bytes, 0, 1024)).txid() == 4
                    && PlaintextRecovery.parseMeta(Arrays.copyOfRange(bytes, 1024, 2048)).txid() == 3,
                    "credential barrier after rewrite preserves canonical alternate meta slot");
            }
            System.out.println("RekeyTest passed (exact encrypted rekey fixture, bounded protected scratch, conversion and atomic admission failures)");
        } finally { try (var paths = Files.walk(directory)) { for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(path); } }
    }
    private static void expect(String code, Runnable action) { try { action.run(); throw new AssertionError("expected " + code); } catch (SqlException failure) { check(failure.sqlState().equals(code), failure.sqlState() + " instead of " + code); } }
    private static void check(boolean value, String message) { if (!value) throw new AssertionError(message); }
}
