using System.Buffers.Binary;
using Jed;

internal static class StorageLargeValuesTests
{
    internal static async Task Run(string root)
    {
        Codec(root);
        var fixtures = Path.Combine(root, "spec/fileformat/fixtures");
        foreach (var name in new[] { "overflow_table", "compressed_table", "encrypted_overflow_table" })
        {
            await using var store = new MemoryBlockStore(await File.ReadAllBytesAsync(Path.Combine(fixtures, name + ".jed")));
            var encrypted = name.StartsWith("encrypted_", StringComparison.Ordinal);
            var host = CryptoTests.ReadVectors(Path.Combine(root, "spec/fileformat/encryption_vectors.toml")).Single(v => v.Name == "host_kek");
            using var credential = StorageCredential.HostKey(CryptoTests.Hex(host, "key_hex"));
            await using var database = encrypted ? await Database.OpenEncryptedStoreAsync(store, credential, System.Security.Cryptography.RandomNumberGenerator.GetBytes)
                : await Database.OpenStoreAsync(store);
            uint originalHead;
            SqlValue[]? originalValues = null;
            using (var snapshot = database.Pin())
            {
                var table = snapshot.Snapshot.Catalog.Tables.Single(); var tree = database.Tree(table, null, snapshot.Snapshot);
                var records = new List<StorageRecord>(); await foreach (var record in tree.ScanAsync()) records.Add(record);
                originalHead = StorageLargeValues.External(records[0].Cells[1]) ? BinaryPrimitives.ReadUInt32BigEndian(records[0].Cells[1]!.AsSpan(1)) : 0;
                foreach (var record in records)
                {
                    var materialized = await tree.MaterializeRecordAsync(record); var values = table.DecodeRow(materialized);
                    var id = values[0].AsInteger();
                    if (id == 1)
                    {
                        originalValues = values;
                        Corpus.Equal(encrypted ? 2500 : 600, values[1].AsText().Length, name + " text length");
                        Corpus.Equal(encrypted ? 1200 : name == "compressed_table" ? 200 : 300, values[2].AsBytes().Length, name + " bytea length");
                    }
                    var prepared = tree.PrepareRecord(table.EncodeRow(values));
                    for (var column = 0; column < record.Cells.Length; column++)
                    {
                        var actual = prepared.Cells[column]?.ToArray(); var expected = record.Cells[column]?.ToArray();
                        if (column > 0 && StorageLargeValues.External(expected))
                        {
                            expected!.AsSpan(1, 4).Clear();
                            if (encrypted) expected!.AsSpan(expected!.Length - 16).Clear();
                        }
                        Corpus.Equal(Convert.ToHexString(expected ?? []), Convert.ToHexString(actual ?? []), name + " exact planned stored cell");
                    }
                }
                Corpus.Equal(snapshot.Snapshot.Meta.LivePages, checked((uint)(snapshot.Snapshot.Catalog.Pages.Length + await tree.CountPagesAsync())), name + " reachable live pages");
            }
            await using var session = database.CreateSession();
            await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (5, 'another', NULL)");
            using (var snapshot = database.Pin())
            {
                var table = snapshot.Snapshot.Catalog.Tables.Single(); var tree = database.Tree(table, null, snapshot.Snapshot);
                var first = await tree.FindAsync(KeyEncoding.Integer(1, 4));
                if (originalHead != 0) Corpus.Equal(originalHead, BinaryPrimitives.ReadUInt32BigEndian(first!.Cells[1]!.AsSpan(1)), "unchanged row retains overflow chain");
                Corpus.Equal(snapshot.Snapshot.Meta.LivePages, checked((uint)(snapshot.Snapshot.Catalog.Pages.Length + await tree.CountPagesAsync())), "COW live pages include retained chains");
            }
            await DatabaseFileTests.Exec(session, "DELETE FROM t WHERE id=1");
            using (var snapshot = database.Pin())
            {
                var table = snapshot.Snapshot.Catalog.Tables.Single(); var tree = database.Tree(table, null, snapshot.Snapshot);
                Corpus.Equal(snapshot.Snapshot.Meta.LivePages, checked((uint)(snapshot.Snapshot.Catalog.Pages.Length + await tree.CountPagesAsync())), "deleting spilled row retires complete chain");
            }
            var replacement = originalValues!; replacement[0] = SqlValue.Integer(6, SqlType.I32);
            await session.ExecuteAsync(session.Prepare("INSERT INTO t VALUES ($1, $2, $3)"), replacement);
            Corpus.Equal(replacement[1].AsText(), await DatabaseFileTests.Scalar(session, "SELECT body FROM t WHERE id=6"), "freshly allocated chain roundtrip through lazy SQL");
            uint freshHead;
            using (var snapshot = database.Pin())
            {
                var table = snapshot.Snapshot.Catalog.Tables.Single(); var tree = database.Tree(table, null, snapshot.Snapshot);
                var record = await tree.FindAsync(KeyEncoding.Integer(6, 4));
                freshHead = StorageLargeValues.External(record!.Cells[1]) ? BinaryPrimitives.ReadUInt32BigEndian(record.Cells[1]!.AsSpan(1)) : 0;
                Corpus.Equal(snapshot.Snapshot.Meta.LivePages, checked((uint)(snapshot.Snapshot.Catalog.Pages.Length + await tree.CountPagesAsync())), "fresh chain exact live-page count");
            }
            await DatabaseFileTests.Exec(session, "UPDATE t SET id=9 WHERE id=6");
            using (var snapshot = database.Pin())
            {
                var table = snapshot.Snapshot.Catalog.Tables.Single(); var tree = database.Tree(table, null, snapshot.Snapshot);
                var record = await tree.FindAsync(KeyEncoding.Integer(9, 4));
                if (freshHead != 0) Corpus.Equal(freshHead, BinaryPrimitives.ReadUInt32BigEndian(record!.Cells[1]!.AsSpan(1)), "rekey preserves unassigned chain and cancels its retirement");
                Corpus.Equal(snapshot.Snapshot.Meta.LivePages, checked((uint)(snapshot.Snapshot.Catalog.Pages.Length + await tree.CountPagesAsync())), "rekey exact live-page count");
            }
        }
        await using (var store = new MemoryBlockStore(await File.ReadAllBytesAsync(Path.Combine(fixtures, "malformed_overflow_cycle.jed"))))
        {
            await using var database = await Database.OpenStoreAsync(store); using var snapshot = database.Pin();
            await using var session = database.CreateSession();
            Corpus.Equal("3", await DatabaseFileTests.Scalar(session, "SELECT count(*) FROM t"), "untouched corrupted chain is not fetched");
            var table = snapshot.Snapshot.Catalog.Tables.Single(); var tree = database.Tree(table, null, snapshot.Snapshot);
            await HostTests.Error(SqlStates.DataCorrupted, async () =>
            {
                await foreach (var record in tree.ScanAsync()) _ = await tree.MaterializeRecordAsync(record);
            });
        }
        Console.WriteLine("PASS large values: exact shared LZ4 vectors, malformed blocks, three plaintext/encrypted overflow goldens, exact disposition bytes, kept chains, live-page retirement and cyclic-chain rejection");
    }

    private static void Codec(string root)
    {
        var values = new Dictionary<string, string>(StringComparer.Ordinal); var count = 0;
        void Check()
        {
            if (values.Count == 0) return;
            var input = Convert.FromHexString(values["input_hex"]); var expected = Convert.FromHexString(values["compressed_hex"]);
            Corpus.Equal(Convert.ToHexString(expected), Convert.ToHexString(StorageLz4.Compress(input)), "pinned LZ4 " + values["name"]);
            Corpus.Equal(Convert.ToHexString(input), Convert.ToHexString(StorageLz4.Decompress(expected, input.Length)), "LZ4 inverse " + values["name"]);
            count++; values.Clear();
        }
        foreach (var raw in File.ReadLines(Path.Combine(root, "spec/fileformat/lz4_vectors.toml")))
        {
            var line = raw.Trim(); if (line.Length == 0 || line.StartsWith('#') || line.StartsWith("schema_version", StringComparison.Ordinal)) continue;
            if (line == "[[vector]]") { Check(); continue; }
            var split = line.IndexOf('=');
            if (split <= 0) throw new InvalidDataException("malformed LZ4 fixture field");
            var value = line[(split + 1)..].Trim();
            if (value.Length < 2 || value[0] != '"' || value[^1] != '"') throw new InvalidDataException("LZ4 fixture field is not a quoted string");
            values.Add(line[..split].Trim(), value[1..^1]);
        }
        Check(); Corpus.Equal(true, count >= 10, "complete LZ4 fixture list");
        foreach (var (hex, length) in new[] { ("", 0), ("f0", 15), ("2061", 2), ("106101", 5), ("1061000000", 5), ("1061020000", 5), ("1061010000", 4), ("00", int.MaxValue) })
        {
            try { _ = StorageLz4.Decompress(Convert.FromHexString(hex), length); }
            catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { continue; }
            throw new InvalidDataException("malformed LZ4 accepted: " + hex);
        }
    }
}
