using Jed;

// Separate live and reopened-image walks are intentional. A disk-only walk
// cannot detect stale references retained by the committed in-memory root.
internal static class SimulationStorageChecks
{
    internal sealed record Result(ulong Version, uint Live, uint Pages, int Free, string Image);

    internal static async Task<Result> Image(byte[] image, ulong version, StorageCredential? credential = null)
    {
        await using var database = credential is null
            ? await Database.OpenStoreAsync(new MemoryBlockStore(image))
            : await Database.OpenEncryptedStoreAsync(new MemoryBlockStore(image), credential, count => new byte[count]);
        Require(database.Version == version, "image recovered an unexpected generation");
        await Live(database);
        var storage = database.CommittedStorageForTesting();
        return new(storage.Meta.TransactionId, storage.Meta.LivePages, storage.Meta.PageCount, storage.FreePages.Count,
            SimulationRows.Fold(image.AsSpan(0, checked((int)(storage.Meta.PageCount * storage.Meta.PageSize)))));
    }

    internal static async Task Live(Database database)
    {
        await using var pin = await database.PinAsync();
        var snapshot = pin.Snapshot; var pager = snapshot.Pager!;
        var recovered = database.CommittedStorageForTesting();
        var disk = await new StorageSnapshotCopy(pager, snapshot).ReachableAsync(default);
        Require(!disk.Overlaps(recovered.FreePages), "free list overlaps reachable image pages");
        Require(!recovered.ProtectedPages.Overlaps(recovered.FreePages), "free list overlaps manifest dependencies");
        var live = new HashSet<uint>();
        void Page(uint page)
        {
            Require(page >= (pager.Encrypted ? 4 : 2) && page < snapshot.Meta.PageCount, "live reference outside high-water");
            Require(!recovered.FreePages.Contains(page), "live reference points into the allocator free list");
            Require(live.Add(page), "two live references share a page");
        }
        void Shape(StorageTreeShape? shape, uint root)
        {
            if (root == 0) { Require(shape is null, "empty tree retains a shape"); return; }
            Require(shape is not null && shape.Page == root, "committed root and resident shape disagree");
            var stack = new Stack<StorageTreeShape>(); stack.Push(shape!);
            while (stack.TryPop(out var node)) { Page(node.Page); foreach (var child in node.Children) stack.Push(child); }
        }
        foreach (var page in snapshot.Catalog.Pages) Page(page);
        await using var session = database.CreateSession();
        var meter = new Meter(session, 0, default);
        foreach (var table in snapshot.Catalog.Tables)
        {
            Shape(table.Shape, table.Root);
            var tree = database.Tree(table, null, snapshot); var schema = table.LeafSchema();
            var rows = new List<(StorageRecord Record, SqlValue[] Values)>(); byte[]? previous = null;
            await foreach (var record in tree.ScanAsync())
            {
                Require(previous is null || previous.AsSpan().SequenceCompareTo(record.Key) < 0, "table keys do not ascend strictly");
                previous = record.Key;
                for (var column = 0; column < record.Cells.Length; column++)
                    if (schema.Columns[column].Width == 0 && StorageLargeValues.External(record.Cells[column]))
                        await StorageLargeValues.WalkChainAsync(record.Cells[column]!, pager, snapshot.Meta.PageCount, (id, _) => Page(id), default);
                var values = table.DecodeRow(await tree.MaterializeRecordAsync(record));
                var encoded = table.EncodeRow(values, table.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(record.Key) : 0);
                Require(encoded.Key.AsSpan().SequenceEqual(record.Key), "stored primary key differs from row encoding");
                rows.Add((record, values));
            }
            Require(rows.Count == table.RowCount, "catalog row count differs from leaf total");
            foreach (var index in table.Indexes ?? [])
            {
                if (index.Kind == 2)
                {
                    if (index.Gist is { } gist) foreach (var page in gist.Pages) Page(page);
                    else Require(index.Root == 0, "committed GiST has no live tree");
                    continue;
                }
                Shape(index.Shape, index.Root);
                var entries = new List<byte[]>(); previous = null;
                await foreach (var entry in database.IndexTree(index, null, snapshot).ScanAsync())
                {
                    Require(previous is null || previous.AsSpan().SequenceCompareTo(entry.Key) < 0, "index keys do not ascend strictly");
                    previous = entry.Key; entries.Add(entry.Key);
                }
                // Derive column B-tree entries independently of index maintenance.
                // Expression indexes are also checked using the resolved expression,
                // while GIN's term semantics have their own golden corpus.
                if (index.Kind != 0) continue;
                var expected = new List<byte[]>();
                var keys = StatementExecution.ResolveIndexKeys(table, index, snapshot.Catalog);
                var predicate = StatementExecution.ResolveIndexPredicate(table, index, snapshot.Catalog);
                foreach (var (record, values) in rows)
                {
                    if (!QueryExecution.Keeps(predicate, values, meter)) continue;
                    var writer = new WireWriter();
                    for (var i = 0; i < keys.Length; i++)
                    {
                        var key = index.Keys[i]; var descending = (key.Options & 1) != 0;
                        var value = Evaluate.Expr(keys[i], meter, row: values);
                        var collation = keys[i].Collation?.Table ?? key.CollationData ?? (key.Expression is null ? table.Columns[key.Column].CollationData : null);
                        var bytes = KeyEncoding.Encode(value, nullable: true, descending: descending, collation: collation);
                        if (((key.Options & 2) != 0) != descending) bytes[0] ^= 1;
                        writer.Bytes(bytes);
                    }
                    writer.Bytes(record.Key); expected.Add(writer.ToArray());
                }
                expected.Sort((a, b) => a.AsSpan().SequenceCompareTo(b));
                Require(expected.Count == entries.Count, "secondary index entry count differs from table derivation");
                for (var i = 0; i < expected.Count; i++) Require(expected[i].AsSpan().SequenceEqual(entries[i]), "secondary index entry differs from table derivation");
            }
        }
        Require(live.SetEquals(disk), "live references and reopened image reachability disagree");
    }

    internal static void Require(bool condition, string message)
    { if (!condition) throw new InvalidDataException("simulation invariant: " + message); }
}
