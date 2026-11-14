using Jed;

internal static class IncrementalReclaimTests
{
    internal static async Task Run()
    {
        HeldBatchOrphans();
        var directory = Path.Combine(Path.GetTempPath(), "jed-csharp-incremental-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(directory);
        try
        {
            await using var database = await Database.CreateFileAsync(Path.Combine(directory, "main.jed"), pageSize: 256);
            await using (var bootstrap = await database.PinAsync())
            {
                var bytes = await bootstrap.Snapshot.Pager!.ReadPageAsync(bootstrap.Snapshot.Meta.Root);
                Corpus.Equal((ushort)1, StorageFormat.ParsePage(bytes.Span).Stamp, "new file stamps its bootstrap catalog generation");
            }
            await using var session = database.CreateSession();
            await DatabaseFileTests.Exec(session, "CREATE TABLE t (id i64 PRIMARY KEY, v text)");
            await DatabaseFileTests.Exec(session, "INSERT INTO t SELECT g, repeat('a',30) FROM generate_series(1,200) g");
            async Task Churn(char value, int count)
            {
                for (var i = 0; i < count; i++)
                    await DatabaseFileTests.Exec(session, $"UPDATE t SET v=repeat('{value}',30) WHERE id={1 + i % 200}");
            }
            await Churn('b', 50);
            var steady = database.CommittedStorageForTesting().Meta.PageCount;
            await using var pin = await database.PinAsync();
            var table = pin.Snapshot.Catalog.Tables.Single();
            var tree = database.Tree(table, null, pin.Snapshot);
            await Churn('c', 300);
            var grown = database.CommittedStorageForTesting().Meta.PageCount;
            Corpus.Equal(true, grown > steady, "reader holds newer free generations while writer advances");
            var rows = 0;
            await foreach (var row in tree.ScanAsync())
            {
                var values = table.DecodeRow(row);
                Corpus.Equal(true, values[1].ToString() is var text && (text == new string('a', 30) || text == new string('b', 30)),
                    "retained reader sees original values despite free-list publication");
                rows++;
            }
            Corpus.Equal(200, rows, "reader retains every row across incremental reclamation");
            await pin.DisposeAsync();
            await Churn('d', 300);
            var after = database.CommittedStorageForTesting();
            Corpus.Equal(0L, after.LeakedPages, "quiet owner lists every orphan without a reachability rebuild");
            Corpus.Equal(true, after.Meta.PageCount <= grown, "closing reader resumes reuse and stops growth");
        }
        finally { Directory.Delete(directory, recursive: true); }
        Console.WriteLine("PASS incremental reclamation: retained readers, generation-aware reuse and exact zero-leak accounting");
    }

    private static void HeldBatchOrphans()
    {
        var before = new StorageRecovered(new(256, 5, 2, 10, 0, 1), 1, 0, [], [], [])
        { Batch = new(4, [], [new(3, 0)], null) };
        var allocation = new StorageAllocation(before);
        allocation.Reclaim(3); allocation.Reclaim(4);
        var root = allocation.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
        var plan = allocation.Prepare(root, 1);
        Corpus.Equal(true, plan.HeldOrphans.SetEquals([3u]), "pending orphan remains held outside persisted free list");
        Corpus.Equal(true, plan.FreePages.Contains(4) && !plan.FreePages.Contains(3), "only pre-batch orphan is listed");
        var flushed = new StorageRecovered(plan.Meta, 1, 1, plan.ProtectedPages, plan.FreePages, plan.FreeListPages)
        { HeldOrphans = plan.HeldOrphans };
        var next = new StorageAllocation(flushed);
        next.Reclaim(root);
        var nextRoot = next.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
        var after = next.Prepare(nextRoot, 1);
        Corpus.Equal(true, nextRoot != 3 && after.FreePages.Contains(3), "first post-flush commit lists held orphan without reusing it");
        Corpus.Equal(0, after.HeldOrphans.Count, "post-flush plan drains held orphan set");
    }
}
