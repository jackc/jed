using Jed;

internal static class StorageSerializationTests
{
    internal static async Task Run(string root)
    {
        var empty = await File.ReadAllBytesAsync(Path.Combine(root, "spec/fileformat/fixtures/empty_db.jed"));
        foreach (var count in new[] { 1, 40 })
        foreach (var overflow in new[] { false, true })
        {
            await using var store = new MemoryBlockStore(empty);
            var before = await StorageRecovery.OpenAsync(store);
            var pager = new StoragePager(store, 256, cacheBytes: 256);
            var table = new CatalogTable("t", [new("id", new(SqlType.I32)), new("body", new(SqlType.Text))], [0]);
            var schema = table.LeafSchema(); var tree = new StorageTree(pager, schema);
            string Body(int id) => overflow
                ? Convert.ToHexString(Enumerable.Range(0, 400).Select(i => unchecked((byte)(i * 73 + i / 17 + id))).ToArray())
                : "value-" + id;
            StorageRecord Record(int id, string body) => table.EncodeRow([SqlValue.Integer(id, SqlType.I32), SqlValue.Text(body)]);
            for (var id = 1; id <= count; id++) tree = await tree.UpsertAsync(Record(id, Body(id)));
            var pending = await tree.FindAsync(KeyEncoding.Integer(1, 4));
            Corpus.Equal(overflow, pending!.FreshChains?[1] is not null, "serialization fixture exercises fresh or inline cells");
            var pendingCell = Convert.ToHexString(pending.Cells[1]!);
            async Task<StorageCommitPlan> Serialize(StorageTree input, StorageRecovered basis)
            {
                var allocation = new StorageAllocation(basis);
                var page = await input.SerializeAsync(allocation);
                return allocation.PrepareMemory(page, checked((uint)input.PageDelta));
            }
            var first = await Serialize(tree, before);
            var shape = tree.SerializedShape; var resident = tree.SerializedRootRecords;
            Corpus.Equal(count == 1, resident is not null, "only a single-leaf root retains records");
            if (resident is not null) Corpus.Equal(overflow, resident[0].FreshChains?[1] is not null, "root preserves fresh payload ownership");
            var repeated = await Serialize(tree, before);
            Corpus.Equal(first.Body.Length, repeated.Body.Length, "serialization does not consume pending fresh chains");
            for (var i = 0; i < first.Body.Length; i++)
            {
                Corpus.Equal(first.Body[i].Page, repeated.Body[i].Page, "repeat serialization preserves page order");
                Corpus.Equal(true, first.Body[i].Bytes.AsSpan().SequenceEqual(repeated.Body[i].Bytes), "repeat serialization preserves exact bytes");
            }
            Corpus.Equal(pendingCell, Convert.ToHexString((await tree.FindAsync(KeyEncoding.Integer(1, 4)))!.Cells[1]!), "serialization leaves pending pointers immutable");
            await Persist(first);
            var snapshot = new StorageTree(pager, schema, first.Meta.Root, first.Meta.PageCount, shape: shape, residentRoot: resident);
            var borrowed = await snapshot.FindAsync(KeyEncoding.Integer(1, 4));
            borrowed!.Cells[1]![0] ^= 1;
            await Check(snapshot, false);
            var changed = await tree.UpsertAsync(Record(1, "replacement"));
            var second = await Serialize(changed, before with { Meta = first.Meta });
            await Persist(second);
            await Check(snapshot, false);
            var reopened = new StorageTree(new StoragePager(store, 256, cacheBytes: 256), schema, second.Meta.Root, second.Meta.PageCount);
            await Check(reopened, true);

            async Task Persist(StorageCommitPlan plan)
            {
                await store.SetLengthAsync((long)plan.Meta.PageCount * 256);
                foreach (var page in plan.Body) await store.WriteAsync((long)page.Page * 256, page.Bytes);
            }
            async Task Check(StorageTree input, bool updated)
            {
                var seen = 0;
                await foreach (var row in input.ScanAsync())
                {
                    var values = table.DecodeRow(await input.MaterializeRecordAsync(row));
                    var id = checked((int)values[0].AsInteger());
                    Corpus.Equal(updated && id == 1 ? "replacement" : Body(id), values[1].AsText(), "root/sibling contents survive publication and reopen");
                    seen++;
                }
                Corpus.Equal(count, seen, "serialization snapshot row count");
            }
        }
        Console.WriteLine("PASS storage serialization ownership: repeatable bytes, immutable pending pointers, root payloads, sibling snapshots and reopen");
    }
}
