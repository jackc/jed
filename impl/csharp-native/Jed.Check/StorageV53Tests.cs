using System.Buffers.Binary;
using System.Runtime.InteropServices;
using Jed;

internal static class StorageV53Tests
{
    internal static async Task Run(string root)
    {
        await Stamp();
        await CatalogOnlyCommits();
        foreach (var encrypted in new[] { false, true }) await Histories(encrypted);
        foreach (var path in Directory.GetFiles(Path.Combine(root, "spec/fileformat/fixtures"), "cow_deferred_*.jed"))
        {
            await using var database = await Database.DeserializeAsync(await File.ReadAllBytesAsync(path));
            using var pin = database.Pin();
            foreach (var table in pin.Snapshot.Catalog.Tables)
            {
                long count = 0;
                await foreach (var record in database.Tree(table, null, pin.Snapshot).ScanAsync()) { _ = table.DecodeRow(record); count++; }
                Corpus.Equal(table.RowCount, count, "v53 shared deferred fixture row count");
            }
        }
        Console.WriteLine("PASS v53: stamp rejects stale page independently of agreeing CRC; plaintext/encrypted retention, fold, old snapshots, backup and reopen");
    }
    private static async Task CatalogOnlyCommits()
    {
        var directory = Path.Combine(Path.GetTempPath(), "jed-v53-catalog-" + Guid.NewGuid().ToString("N")); Directory.CreateDirectory(directory);
        try
        {
            foreach (var synchronous in new[] { Synchronous.On, Synchronous.Off })
            {
                var path = Path.Combine(directory, synchronous + ".jed");
                var options = new DatabaseFileOptions { Synchronous = synchronous, EnableIdleFlush = false, FlushIntervalMilliseconds = 0 };
                await using (var database = await Database.CreateFileAsync(path, options, 256))
                {
                    await using var session = database.CreateSession();
                    await DatabaseFileTests.Exec(session, "CREATE TABLE t (id i64 PRIMARY KEY, v i64)");
                    await DatabaseFileTests.Exec(session, "INSERT INTO t SELECT g, g*7 FROM generate_series(1,50) g");
                    await DatabaseFileTests.Exec(session, "ANALYZE t");
                    var before = database.CommittedStorageForTesting();
                    await DatabaseFileTests.Exec(session, "ANALYZE t");
                    var after = database.CommittedStorageForTesting();
                    Corpus.Equal(before.Meta.TransactionId + 1, after.Meta.TransactionId, "repeated ANALYZE publishes its transaction");
                    Corpus.Equal(synchronous == Synchronous.On, before.Meta.Root == after.Meta.Root,
                        "unchanged catalog is retained except when a relaxed commit needs body writes");
                    if (synchronous == Synchronous.On)
                        Corpus.Equal(false, after.ProtectedPages.Contains(after.Meta.Root), "durable catalog-only commit retains root outside new manifest");
                    else
                        Corpus.Equal(true, after.ProtectedPages.Contains(after.Meta.Root), "relaxed catalog-only commit writes a manifest-covered root");
                    await DatabaseFileTests.Exec(session, "CREATE TABLE ranges (id i64 PRIMARY KEY, r i32range)");
                    await DatabaseFileTests.Exec(session, "CREATE INDEX ranges_r ON ranges USING gist (r)");
                    await DatabaseFileTests.Exec(session, "INSERT INTO ranges VALUES (1, i32range(1,4)), (2, i32range(5,9))");
                    await using var old = await database.PinAsync();
                    var oldIndex = old.Snapshot.Catalog.Tables.Single(table => table.Name == "ranges").Indexes!.Single();
                    await DatabaseFileTests.Exec(session, "CREATE TABLE marker (id i64 PRIMARY KEY)");
                    await using var changed = await database.PinAsync();
                    var newIndex = changed.Snapshot.Catalog.Tables.Single(table => table.Name == "ranges").Indexes!.Single();
                    Corpus.Equal(false, oldIndex.Root == newIndex.Root, "unrelated commit rewrites the complete GiST tree");
                    foreach (var page in newIndex.Gist!.Pages)
                    {
                        var bytes = await changed.Snapshot.Pager!.ReadPageAsync(page);
                        Corpus.Equal(unchecked((ushort)changed.Snapshot.Meta.TransactionId), StorageFormat.ParsePage(bytes.Span).Stamp,
                            "unchanged GiST pages carry the current commit stamp");
                    }
                }
                await using var reopened = await Database.OpenFileAsync(path);
                await using var reader = reopened.CreateSession();
                Corpus.Equal("50", await DatabaseFileTests.Scalar(reader, "SELECT count(*) FROM t"), "catalog-only commit survives reopen");
            }
        }
        finally { Directory.Delete(directory, recursive: true); }
    }
    private static async Task Stamp()
    {
        await using var store = new MemoryBlockStore();
        var before = new StorageMeta(256, 1, 2, 3, 0, 1);
        var meta = StorageFormat.EncodeMeta(before);
        await store.WriteAsync(0, meta); await store.WriteAsync(256, meta);
        await store.WriteAsync(512, StorageFormat.EncodePage(256, new(1, 0, 0, 0, 1), []));
        var recovered = await StorageRecovery.OpenAsync(store);
        var allocation = new StorageAllocation(recovered);
        var root = allocation.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
        var plan = allocation.Prepare(root, 1);
        await new StorageCommitter(new StoragePager(store, 256), recovered).CommitAsync(plan);
        var stale = plan.Body.Single(page => page.Page == root).Bytes.ToArray();
        StorageFormat.Stamp(stale, 1);
        await store.WriteAsync((long)root * 256, stale);
        var changedMeta = plan.MetaBytes.ToArray();
        var entries = (int)Math.Min(plan.Meta.ManifestEntries, (uint)StorageFormat.InlineCapacity(256));
        for (var i = 0; i < entries; i++)
            if (BinaryPrimitives.ReadUInt32BigEndian(changedMeta.AsSpan(64 + i * 8)) == root)
                BinaryPrimitives.WriteUInt32BigEndian(changedMeta.AsSpan(68 + i * 8), StorageFormat.ManifestDigest(stale, false));
        StorageFormat.SealMeta(changedMeta); await store.WriteAsync(0, changedMeta);
        Corpus.Equal(1UL, (await StorageRecovery.OpenAsync(store)).Meta.TransactionId, "stale stamp rejected with matching manifest CRC");
    }
    private static async Task Histories(bool encrypted)
    {
        var directory = Path.Combine(Path.GetTempPath(), "jed-v53-" + Guid.NewGuid().ToString("N")); Directory.CreateDirectory(directory);
        var path = Path.Combine(directory, "main.jed");
        using var credential = DatabaseCredential.FromKey(new byte[32]);
        var options = new DatabaseFileOptions { Credential = encrypted ? credential : null };
        var size = encrypted ? 1024 : 256;
        var sawRemap = false; var sawFold = false; var sawCatalogKeep = false;
        try
        {
            await using (var database = await Database.CreateFileAsync(path, options, size))
            {
                await using var session = database.CreateSession();
                await DatabaseFileTests.Exec(session, "CREATE TABLE t (id i64 PRIMARY KEY, v i64)");
                await DatabaseFileTests.Exec(session, "INSERT INTO t SELECT g, g*100000000000 FROM generate_series(1,2000) g");
                await using var old = await database.PinAsync(); var table = old.Snapshot.Catalog.Tables.Single();
                if (encrypted) OwnedTags(old.Snapshot);
                for (var i = 0; i < 80; i++)
                {
                    var before = database.CommittedStorageForTesting().Meta;
                    await DatabaseFileTests.Exec(session, $"UPDATE t SET v=v+1 WHERE id={1 + (i * 137) % 2000}");
                    var after = database.CommittedStorageForTesting().Meta;
                    sawRemap |= after.Remap.Length > 0; sawFold |= before.Remap.Length > 0 && after.Remap.Length == 0;
                    sawCatalogKeep |= before.Root == after.Root;
                }
                await foreach (var row in database.Tree(table, null, old.Snapshot).ScanAsync())
                {
                    var values = table.DecodeRow(row);
                    Corpus.Equal((long.Parse(values[0].ToString()) * 100000000000).ToString(), values[1].ToString(), "old snapshot resolves own retained interiors");
                }
                Corpus.Equal(true, sawRemap, "updates exercise remap retention");
                Corpus.Equal(true, sawFold, "bounded remap list folds");
                Corpus.Equal(true, sawCatalogKeep, "unchanged catalog chain retained");
                if (encrypted)
                {
                    await using var latest = await database.PinAsync();
                    OwnedTags(latest.Snapshot);
                }
                await old.DisposeAsync();
                await database.BackupForTestingAsync(Path.Combine(directory, "copy.jed"), false);
            }
            foreach (var name in new[] { "main.jed", "copy.jed" })
            {
                await using var database = await Database.OpenFileAsync(Path.Combine(directory, name), options);
                await using var session = database.CreateSession();
                Corpus.Equal("2000", await DatabaseFileTests.Scalar(session, "SELECT count(*) FROM t"), "retained tree survives reopen/backup");
                Corpus.Equal("80", await DatabaseFileTests.Scalar(session, "SELECT count(*) FROM t WHERE v%100000000000=1"), "remap selects newest leaf values");
                await DatabaseFileTests.Exec(session, "DELETE FROM t WHERE id < 250");
                Corpus.Equal("1751", await DatabaseFileTests.Scalar(session, "SELECT count(*) FROM t"), "retired parents drop their remap entries");
            }
        }
        finally { Directory.Delete(directory, recursive: true); }
    }
    private static void OwnedTags(DatabaseSnapshot snapshot)
    {
        static void Tag(ReadOnlyMemory<byte> tag)
        {
            Corpus.Equal(true, MemoryMarshal.TryGetArray(tag, out var storage) && storage.Array!.Length == 16 && storage.Offset == 0 && storage.Count == 16,
                "retained authentication tag owns only its sixteen bytes, never a ciphertext page");
        }
        static void Shape(StorageTreeShape? shape)
        {
            if (shape is null) return;
            Tag(shape.Tag);
            foreach (var child in shape.Children) Shape(child);
        }
        Tag(snapshot.Encrypted!.RootTag); Tag(snapshot.Encrypted.FreeListTag);
        foreach (var entry in snapshot.Meta.Remap) Tag(entry.Tag);
        foreach (var table in snapshot.Catalog.Tables)
        {
            Tag(table.RootTag); Shape(table.Shape);
            foreach (var index in table.Indexes ?? []) { Tag(index.RootTag); Shape(index.Shape); }
        }
    }
}
