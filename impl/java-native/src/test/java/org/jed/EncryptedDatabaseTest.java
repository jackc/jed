package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.List;
import org.jed.storage.*;

/** Encrypted file/SQL lifecycle, bound-pointer traversal, chain preservation and shared goldens. */
public final class EncryptedDatabaseTest {
    public static void main(String[] args) throws Exception {
        Path fixtures = Path.of(args.length == 0 ? "spec/fileformat/fixtures" : args[0]);
        Path directory = Files.createTempDirectory("jed-encrypted-"); byte[] hostKey = new byte[32]; Arrays.fill(hostKey, (byte) 0x4a);
        Credential credential = Credential.hostKey(hostKey);
        Environment environment = Environment.builder().keyEntropy(() -> CryptoTest.seeded(1246053442)).build();
        try {
            Path empty = directory.resolve("empty.jed");
            try (var db = FileDatabase.createExclusive(empty, 1024, 1024, credential, environment)) { check(db.snapshot().tables().isEmpty(), "empty encrypted database"); }
            check(Arrays.equals(Files.readAllBytes(empty), Files.readAllBytes(fixtures.resolve("encrypted_empty_db.jed"))), "seeded empty database differs from shared image");
            byte[] broken = Files.readAllBytes(empty); broken[3 * 1024] ^= 1;
            Path repair = directory.resolve("repair.jed"); Files.write(repair, broken);
            expect("28P01", () -> FileDatabase.openExclusive(repair, 1024, Credential.hostKey(new byte[32]), environment));
            check(Arrays.equals(broken, Files.readAllBytes(repair)), "wrong credential repaired key block");
            try (var database = FileDatabase.openExclusive(repair, 1024, credential, environment)) { check(database.snapshot().tables().isEmpty(), "repaired encrypted open"); }
            byte[] repaired = Files.readAllBytes(repair);
            check(Arrays.equals(Arrays.copyOfRange(repaired, 2048, 3072), Arrays.copyOfRange(repaired, 3072, 4096)), "key-block pair not repaired");
            // A CRC-valid selected meta with a bad authentication tag must not fall
            // back to an older authenticated meta, which would hide tampering.
            Path tampered = directory.resolve("meta-tampered.jed"); Files.copy(empty, tampered);
            int selectedSlot;
            try (var store = FileBlockStore.open(tampered, false)) { selectedSlot = PlaintextRecovery.recover(store).slot(); }
            byte[] damaged = Files.readAllBytes(tampered), meta = Arrays.copyOfRange(damaged, selectedSlot * 1024, (selectedSlot + 1) * 1024);
            meta[1023] ^= 1; PageFormat.put32(meta, 32, PageFormat.metaCrc(meta)); System.arraycopy(meta, 0, damaged, selectedSlot * 1024, 1024); Files.write(tampered, damaged);
            expect("XX001", () -> FileDatabase.openExclusive(tampered, 1024, credential, environment));
            check(Arrays.equals(damaged, Files.readAllBytes(tampered)), "failed authentication modified file");
            for (String fixture : List.of("encrypted_empty_db", "encrypted_pk_table", "encrypted_overflow_table", "encrypted_catalog")) {
                Path path = directory.resolve(fixture + ".jed"); Files.copy(fixtures.resolve(fixture + ".jed"), path);
                try (var db = FileDatabase.openExclusive(path, 1024, credential, environment)) {
                    long count = 0;
                    for (var table : db.snapshot().tables().values()) {
                        try (var cursor = table.rows().cursor()) {
                            PersistentTree.Entry entry; long rows = 0;
                            while ((entry = cursor.next()) != null) { for (var value : Encoding.row(entry, table.types())) { } rows++; }
                            check(rows == table.rows().size(), "fixture row count"); count += rows;
                        }
                    }
                    if (fixture.equals("encrypted_pk_table")) check(count > 0, "fixture row inventory");
                    check(Arrays.equals(bootstrap(db.snapshot(), credential), Files.readAllBytes(path)), "fresh canonical image differs: " + fixture);
                }
            }
            Path file = directory.resolve("written.jed");
            byte[] large = new byte[20_000]; new java.util.Random(9).nextBytes(large);
            try (var db = FileDatabase.createExclusive(file, 1024, 2048, credential, environment)) {
                var types = List.of(SqlType.I64, SqlType.BYTEA, SqlType.TEXT); int[] primary = {0};
                var columns = List.of(new Engine.Column("id", SqlType.I64, true, null), new Engine.Column("content", SqlType.BYTEA, false, null), new Engine.Column("label", SqlType.TEXT, false, null));
                var tree = PersistentTree.empty(1024, new TableLeafCodec(types, primary), db.pageCodec());
                for (int i = 0; i < 500; i++) tree = tree.put(Encoding.entry(Encoding.key(SqlValue.ofInteger(i)), List.of(SqlValue.ofInteger(i), SqlValue.ofBytes(i == 0 ? large : new byte[]{(byte) i}), SqlValue.ofText("row" + i)), null, null, db.layout(), null, primary));
                var state = db.commit(db.snapshot().put(new Engine.Table("items", columns, primary, List.of(), tree, 0)));
                var pinned = state; db.pin(pinned);
                for (int round = 0; round < 40; round++) {
                    var table = state.tables().get("items"); byte[] key = Encoding.key(SqlValue.ofInteger(round)); var prior = table.rows().getEntry(key);
                    var entry = Encoding.entry(key, List.of(SqlValue.ofInteger(round), SqlValue.nullValue(SqlType.BYTEA), SqlValue.ofText("updated" + round)), prior, new boolean[]{false, false, true}, db.layout(), null, primary);
                    state = db.commit(state.put(table.changed(table.rows().put(entry), 0)));
                }
                check(Encoding.row(pinned.tables().get("items").rows().getEntry(Encoding.key(SqlValue.ofInteger(0))), types).get(2).asText().equals("row0"), "encrypted pin changed");
                db.unpin(pinned);
                check(Arrays.equals(Encoding.row(state.tables().get("items").rows().getEntry(Encoding.key(SqlValue.ofInteger(0))), types).get(1).asBytes(), large), "kept encrypted chain");
            }
            expect("28P02", () -> FileDatabase.openExclusive(file, 1024));
            expect("28P01", () -> FileDatabase.openExclusive(file, 1024, Credential.hostKey(new byte[32]), environment));
            try (var db = FileDatabase.openExclusive(file, 1024, credential, environment)) {
                check(db.snapshot().tables().get("items").rows().size() == 500, "encrypted reopen rows");
                db.commit(db.snapshot().drop("items"));
            }
            Path sql = directory.resolve("sql.jed");
            try (var db = Database.createExclusive(sql, 1024, 2048, credential); var session = db.session()) {
                execute(session, "CREATE TABLE t (id i64 PRIMARY KEY, value text)"); execute(session, "INSERT INTO t VALUES (1, 'secret')");
                execute(session, "UPDATE t SET value = 'new secret' WHERE id = 1");
                try (var rows = session.query("SELECT value FROM t WHERE id = 1")) { check(rows.next().get(0).asText().equals("new secret"), "encrypted SQL result"); }
            }
            try (var db = Database.openExclusive(sql, 1024, credential); var session = db.session(); var rows = session.query("SELECT value FROM t")) { check(rows.next().get(0).asText().equals("new secret"), "encrypted SQL reopen"); }
            System.out.println("EncryptedDatabaseTest: passed 4 fresh byte-exact seeded images, 500 rows/40 pinned commits, chains, key repair, no-fallback authentication and SQL reopen");
        } finally { try (var paths = Files.walk(directory)) { for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(path); } }
    }
    private static byte[] bootstrap(Engine.Snapshot snapshot, Credential credential) throws Exception {
        var entropy = CryptoTest.seeded(1246053442); byte[] dataKey = new byte[32]; entropy.accept(dataKey);
        byte[] key = KeyBlock.wrap(1024, 1, dataKey, credential, entropy);
        var codec = new EncryptedPageCodec(dataKey, entropy); var layout = new PageLayout(1024, true);
        PageAllocator allocator = new PageAllocator(4, new long[0]);
        var writes = new java.util.ArrayList<ManifestEncoder.Write>(); var entries = new java.util.ArrayList<byte[]>();
        for (SqlType type : CatalogCodec.orderedTypes(snapshot)) entries.add(CatalogCodec.definition(type));
        for (var table : CatalogCodec.ordered(snapshot)) {
            var tree = PersistentTree.empty(1024, new TableLeafCodec(table.types(), table.primaryKey()), codec);
            try (var cursor = table.rows().cursor()) {
                PersistentTree.Entry row;
                while ((row = cursor.next()) != null) tree = tree.put(Encoding.entry(row.key(), Encoding.row(row, table.types()), null, null, layout, null, table.primaryKey()));
            }
            var prepared = tree.writeImage(allocator, 1, codec, (page, bytes) -> writes.add(new ManifestEncoder.Write(page, bytes))); allocator = prepared.allocator();
            entries.add(CatalogCodec.table(table, prepared.root(), prepared.rootTag()));
        }
        var catalog = CatalogChain.prepare(1024, entries, allocator, 1, null, 0, 4, codec, new byte[16]);
        writes.addAll(catalog.writes()); long pages = catalog.allocator().highWater();
        byte[] meta = ManifestEncoder.encode(layout, 1, catalog.root(), catalog.rootTag(), pages, 0, new byte[16], pages - 4, List.of(), new long[0], List.of()).meta();
        byte[] image = new byte[(int) pages * 1024];
        System.arraycopy(codec.sealMeta(0, meta, List.of()), 0, image, 0, 1024);
        System.arraycopy(codec.sealMeta(1, meta, List.of()), 0, image, 1024, 1024);
        System.arraycopy(key, 0, image, 2048, 1024); System.arraycopy(key, 0, image, 3072, 1024);
        for (var write : writes) System.arraycopy(write.bytes(), 0, image, (int) write.page() * 1024, 1024);
        return image;
    }
    private static void execute(Session session, String sql) { try (Rows rows = session.query(sql)) { while (rows.next() != null) { } } }
    private interface Action { void run() throws Exception; }
    private static void expect(String state, Action action) throws Exception { try { action.run(); throw new AssertionError("expected " + state); } catch (SqlException failure) { check(failure.sqlState().equals(state), "expected " + state + ", got " + failure.sqlState()); } }
    private static void check(boolean value, String message) { if (!value) throw new AssertionError(message); }
}
