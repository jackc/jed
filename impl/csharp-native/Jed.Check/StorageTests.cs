using System.Buffers.Binary;
using Jed;

internal static class StorageTests
{
    internal static async Task Run(string root)
    {
        Corpus.Equal(0xCBF43926u, StorageFormat.Crc32("123456789"u8), "CRC32 vector");
        Corpus.Equal(0x6C40DF5F0B497347UL, StorageFormat.Crc64("123456789"u8), "CRC64 vector");
        Corpus.Equal(StorageFormat.Crc32("123456789"u8), StorageFormat.Crc32("456789"u8, StorageFormat.Crc32("123"u8)), "streamed CRC32");
        Corpus.Equal(StorageFormat.Crc64("123456789"u8), StorageFormat.Crc64("456789"u8, StorageFormat.Crc64("123"u8)), "streamed CRC64");
        await using (var memory = new MemoryBlockStore()) await Device(memory);
        var path = Path.Combine(Path.GetTempPath(), $"jed-csharp-storage-{Guid.NewGuid():N}");
        try
        {
            await using (var file = new FileBlockStore(path, FileMode.CreateNew)) await Device(file);
            await using (var reopen = new FileBlockStore(path, FileMode.Open, readOnly: true))
                Corpus.Equal(65540L, await reopen.GetLengthAsync(), "durable length after reopen");
        }
        finally { File.Delete(path); }

        var fixtureRoot = Path.Combine(root, "spec/fileformat/fixtures");
        var empty = await File.ReadAllBytesAsync(Path.Combine(fixtureRoot, "empty_db.jed"));
        var expectedMeta = new StorageMeta(256, 1, 2, 3, 0, 1);
        Corpus.Equal(true, StorageFormat.EncodeMeta(expectedMeta).AsSpan().SequenceEqual(empty.AsSpan(0, 256)), "bootstrap meta exact golden");
        Corpus.Equal(true, StorageFormat.EncodePage(256, new(1, 0, 0, 0), []).AsSpan().SequenceEqual(empty.AsSpan(512, 256)), "empty catalog exact golden");
        await using (var memory = new MemoryBlockStore(empty))
        {
            var recovered = await StorageRecovery.OpenAsync(memory);
            Corpus.Equal(expectedMeta, recovered.Meta, "empty golden recovered");
            Corpus.Equal(0, recovered.Slot, "bootstrap tie prefers slot zero");
            Corpus.Equal(1, recovered.FloorSlot, "bootstrap parity floor");
        }

        // The immutable shared goldens exercise both bootstrap and incremental
        // manifests. Logical catalog/tree corruption remains a later layer.
        var count = 0;
        foreach (var fixture in Directory.EnumerateFiles(fixtureRoot, "*.jed").Order(StringComparer.Ordinal))
        {
            if (Path.GetFileName(fixture).StartsWith("encrypted_", StringComparison.Ordinal)) continue;
            await using var device = new FileBlockStore(fixture, FileMode.Open, readOnly: true);
            if (Path.GetFileName(fixture) == "cow_invalid_free_list.jed")
                await Corrupt(async () => { _ = await StorageRecovery.OpenAsync(device); });
            else
            {
                var selected = await StorageRecovery.OpenAsync(device);
                Corpus.Equal(true, selected.Meta.PageCount >= 3, "fixture has a catalog root");
                var name = Path.GetFileName(fixture);
                if (name.StartsWith("cow_", StringComparison.Ordinal))
                    Corpus.Equal(name is "cow_inline.jed" or "cow_overflow.jed" or "cow_empty_tail.jed" ? 2UL : 1UL,
                        selected.Meta.TransactionId, $"recovery candidate for {name}");
            }
            count++;
        }
        await Cache();
        await Commit(empty);
        await Preallocation(empty);
        await RelaxedCommits(empty);
        await FreeListBoundary(empty);
        await TrailingTruncation(empty);
        await Leaves(fixtureRoot);
        await Trees(empty, fixtureRoot);
        await CacheRaces();
        await Catalogs(fixtureRoot);
        await IndexStorage();
        Console.WriteLine($"PASS storage: positioned async memory/file devices, durable growth, checksums, byte-exact headers, recovery for {count} shared plaintext fixtures, bounded cache, COW manifests, allocation, poisoning and recovery stabilization");
    }

    private static async Task IndexStorage()
    {
        await using var database = await Database.CreateInMemoryAsync(256);
        await using var session = database.CreateSession();
        static async Task Execute(Session session, string sql)
        {
            await using var cursor = await session.QueryAsync(sql); await foreach (var unused in cursor) { }
        }
        await Execute(session, "CREATE TABLE indexed (id i32 PRIMARY KEY, v text UNIQUE)");
        await Execute(session, "INSERT INTO indexed VALUES (1,'A'),(2,'B')");
        using var old = database.Pin();
        var oldTable = old.Snapshot.Catalog.Tables.Single(); var oldIndex = oldTable.Indexes!.Single();
        var keys = new List<string>();
        await foreach (var record in database.IndexTree(oldIndex, null, old.Snapshot).ScanAsync()) keys.Add(Convert.ToHexString(record.Key));
        Corpus.Equal("0041000180000001,0042000180000002", string.Join(',', keys), "physical unique index nullable-text prefix and storage-key suffix");
        await Execute(session, "UPDATE indexed SET v=CASE WHEN id=1 THEN 'B' ELSE 'A' END");
        await Execute(session, "ALTER TABLE indexed ADD extra i32 DEFAULT 7, ALTER extra SET DEFAULT 9");
        using var current = database.Pin();
        var table = current.Snapshot.Catalog.Tables.Single(); var index = table.Indexes!.Single();
        long live = current.Snapshot.Catalog.Pages.Length + await database.Tree(table, null, current.Snapshot).CountPagesAsync() +
            await database.IndexTree(index, null, current.Snapshot).CountPagesAsync();
        Corpus.Equal((uint)live, current.Snapshot.Meta.LivePages, "index nodes included in exact live-page delta");
        keys.Clear();
        await foreach (var record in database.IndexTree(index, null, current.Snapshot).ScanAsync()) keys.Add(Convert.ToHexString(record.Key));
        Corpus.Equal("0041000180000002,0042000180000001", string.Join(',', keys), "unique index end-state rekey bytes after ALTER rewrite");
        keys.Clear();
        await foreach (var record in database.IndexTree(oldIndex, null, old.Snapshot).ScanAsync()) keys.Add(Convert.ToHexString(record.Key));
        Corpus.Equal("0041000180000001,0042000180000002", string.Join(',', keys), "retained index root survives index update and table rewrite");
        Console.WriteLine("PASS index storage: exact nullable/text/suffix keys, end-state swaps, live-page accounting and retained COW roots");
    }

    private static async Task Catalogs(string fixtureRoot)
    {
        var count = 0;
        foreach (var fixture in Directory.EnumerateFiles(fixtureRoot, "*.jed").Order(StringComparer.Ordinal))
        {
            var name = Path.GetFileName(fixture);
            if (name.StartsWith("encrypted_", StringComparison.Ordinal) || name == "cow_invalid_free_list.jed") continue;
            await using var device = new FileBlockStore(fixture, FileMode.Open, readOnly: true);
            var before = await StorageRecovery.OpenAsync(device); var pager = new StoragePager(device, before.Meta.PageSize);
            if (name is "malformed_missing_type.jed" or "malformed_fk_type.jed")
            { await Corrupt(async () => { _ = await Catalog.ReadAsync(pager, before.Meta); }); continue; }
            Catalog catalog;
            try { catalog = await Catalog.ReadAsync(pager, before.Meta); }
            catch (Exception error) { throw new InvalidDataException($"catalog read {name}", error); }
            var allocation = new StorageAllocation(before); var encoded = catalog.Serialize(allocation);
            var plan = allocation.Prepare(encoded.Root, before.Meta.LivePages);
            Corpus.Equal(catalog.Pages.Length, encoded.PageCount, $"catalog canonical page count {name}");
            for (var i = 0; i < encoded.Pages.Length; i++)
            {
                var original = await pager.ReadPageAsync(catalog.Pages[i]);
                var copied = plan.Body.Single(page => page.Page == encoded.Pages[i]).Bytes;
                Corpus.Equal(true, original.Span[..8].SequenceEqual(copied.AsSpan(0, 8)) && original.Span[16..].SequenceEqual(copied.AsSpan(16)),
                    $"catalog canonical metadata bytes {name} page {i}");
            }
            count++;
        }
        Console.WriteLine($"PASS catalogs: {count} shared plaintext fixtures decode/re-encode exact entry bytes, all v52 catalog kinds and continuations");
    }

    private static async Task Device(IBlockStore store)
    {
        Corpus.Equal(0L, await store.GetLengthAsync(), "initial length");
        await store.SetLengthAsync(65540);
        var bytes = new byte[8];
        await store.ReadAsync(65530, bytes);
        Corpus.Equal(true, StorageFormat.IsZero(bytes), "durable growth zero fills");
        await store.WriteAsync(65533, new byte[] { 1, 2, 3, 4, 5, 6, 7 });
        await store.ReadAsync(65532, bytes);
        Corpus.Equal("0001020304050607", Convert.ToHexString(bytes), "cross-segment positioned write");
        await store.SetLengthAsync(65535);
        await store.SetLengthAsync(65540);
        await store.ReadAsync(65532, bytes);
        Corpus.Equal("0001020000000000", Convert.ToHexString(bytes), "truncated bytes do not resurrect");
        await store.SyncAsync();
        try { await store.ReadAsync(65539, new byte[2]); throw new InvalidDataException("short read accepted"); }
        catch (EndOfStreamException) { }
        using var cancellation = new CancellationTokenSource(); cancellation.Cancel();
        try { await store.WriteAsync(0, new byte[] { 99 }, cancellation.Token); throw new InvalidDataException("canceled write accepted"); }
        catch (OperationCanceledException) { }
        await store.ReadAsync(0, bytes);
        Corpus.Equal(true, StorageFormat.IsZero(bytes), "canceled write leaves bytes untouched");
    }

    private static async Task Cache()
    {
        await using var store = new MemoryBlockStore();
        await store.SetLengthAsync(5 * 256);
        for (uint id = 2; id < 5; id++)
            await store.WriteAsync(id * 256, StorageFormat.EncodePage(256, new(1, 0, id, 0), []));
        var pager = new StoragePager(store, 256, 256);
        var retained = await pager.ReadPageAsync(2);
        _ = await pager.ReadPageAsync(3); _ = await pager.ReadPageAsync(4);
        Corpus.Equal(1, pager.ResidentPages, "cache page budget");
        Corpus.Equal(2u, StorageFormat.ParsePage(retained.Span).Count, "evicted buffer remains owned by reader");
        pager.Invalidate(4);
        await store.WriteAsync(4 * 256, StorageFormat.EncodePage(256, new(1, 0, 99, 0), []));
        Corpus.Equal(99u, StorageFormat.ParsePage((await pager.ReadPageAsync(4)).Span).Count, "reuse invalidation");
        var damaged = StorageFormat.EncodePage(256, new(1, 0, 0, 0), []); damaged[20] = 1;
        pager.Invalidate(4); await store.WriteAsync(4 * 256, damaged);
        await Corrupt(async () => { _ = await pager.ReadPageAsync(4); });
    }

    private static async Task Corrupt(Func<Task> action)
    {
        try { await action(); }
        catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { return; }
        throw new InvalidDataException("expected data_corrupted");
    }

    private static async Task Leaves(string fixtureRoot)
    {
        var integer = new StorageColumn(4, 0, Integer: true);
        var cases = new Dictionary<string, StorageLeafSchema[]>
        {
            ["pk_table.jed"] = [new([integer, new(2, Integer: true)], 4)],
            ["text_table.jed"] = [new([integer, new(Dictionary: true)], 4)],
            ["bool_table.jed"] = [new([integer, new(1)], 4)],
            ["compact_leaf_table.jed"] = [
                new([new(4, 0, Integer: true), new(8, 4, Integer: true), new(Dictionary: true),
                    new(8, Integer: true), new(8, Integer: true), new(ArrayElementWidth: 4), new(Dictionary: true)], 12),
                new([new(Dictionary: true), new(2, Integer: true), new(1), new(4, Integer: true)]), new([])]
        };
        var count = 0;
        foreach (var (name, schemas) in cases)
        {
            var image = await File.ReadAllBytesAsync(Path.Combine(fixtureRoot, name));
            var size = (int)BinaryPrimitives.ReadUInt32BigEndian(image.AsSpan(8));
            for (var at = size * 2; at < image.Length; at += size)
            {
                var page = image.AsMemory(at, size);
                if (page.Span[0] != 2) continue;
                var matched = false;
                foreach (var schema in schemas)
                {
                    try
                    {
                        var leaf = StorageLeaf.Parse(page, schema);
                        var encoded = StorageLeaf.Encode(size, schema, Enumerable.Range(0, leaf.Count).Select(leaf.Record).ToArray());
                        if (!encoded.AsSpan().SequenceEqual(page.Span)) continue;
                        matched = true; break;
                    }
                    catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { }
                }
                Corpus.Equal(true, matched, $"canonical packed-leaf golden {name} page {at / size}");
                count++;
            }
        }
        var dictionarySchema = new StorageLeafSchema([new(Dictionary: true)], 4);
        var repeated = new byte[2001]; repeated[0] = 5; repeated.AsSpan(1).Fill(65);
        var dense = Enumerable.Range(0, 40).Select(i => new StorageRecord(KeyEncoding.Integer(i, 4), [repeated])).ToArray();
        var densePage = StorageLeaf.Encode(8192, dictionarySchema, dense);
        Corpus.Equal(40, StorageLeaf.Parse(densePage, dictionarySchema).Count, "dictionary can replace a plain region larger than u16 offsets");
        Corpus.Equal((long)StorageLeaf.EncodePayload(dictionarySchema, dense).Length, StorageLeaf.MeasurePayload(dictionarySchema, dense), "exact compact payload size");
        Corpus.Equal(true, count > 10, "all compact leaf shapes covered");
        Console.WriteLine($"PASS packed leaves: {count} shared golden pages byte-exact after lazy decode/re-encode, including prefix/elided keys, integer offsets, dictionaries and packed arrays");
    }

    private static async Task Trees(byte[] emptyImage, string fixtureRoot)
    {
        await using var device = new MemoryBlockStore(emptyImage);
        var before = await StorageRecovery.OpenAsync(device);
        var pager = new StoragePager(device, 256, 256);
        var schema = new StorageLeafSchema([new(4, 0, Integer: true), new(8, Integer: true)], 4);
        var tree = new StorageTree(pager, schema);
        static byte[] Integer(long value, int width)
        {
            var bytes = new byte[width];
            var bits = unchecked((ulong)value);
            for (var i = width - 1; i >= 0; i--) { bytes[i] = (byte)(bits & 255); bits >>= 8; }
            bytes[0] ^= 128; return bytes;
        }
        static StorageRecord Row(int key, long value)
        {
            var bytes = Integer(key, 4); return new(bytes, [bytes, Integer(value, 8)]);
        }
        for (var i = 1; i <= 600; i++) if (i != 400) tree = await tree.UpsertAsync(Row(i, 7));
        var snapshot = tree;
        tree = await tree.UpsertAsync(Row(60, 4000000000000));
        tree = await tree.UpsertAsync(Row(400, -4000000000000));
        Corpus.Equal("8000000000000007", Convert.ToHexString((await snapshot.FindAsync(Integer(60, 4)))!.Cells[1]!), "immutable snapshot before replacement");
        var allocation = new StorageAllocation(before);
        var root = await tree.SerializeAsync(allocation);
        var catalog = allocation.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
        var plan = allocation.Prepare(catalog, (uint)(tree.PageDelta + 1));
        var expectedLeaves = new List<string>();
        var golden = await File.ReadAllBytesAsync(Path.Combine(fixtureRoot, "multiway_split.jed"));
        for (var at = 512; at < golden.Length; at += 256)
        {
            if (golden[at] != 2) continue;
            try
            {
                _ = StorageLeaf.Parse(golden.AsMemory(at, 256), schema);
                expectedLeaves.Add(Convert.ToHexString(golden.AsSpan(at, 256)));
            }
            catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { }
        }
        var actualLeaves = plan.Body.Where(write => write.Bytes[0] == 2).Select(write => Convert.ToHexString(write.Bytes)).Order(StringComparer.Ordinal);
        Corpus.Equal(string.Join("\n", expectedLeaves.Order(StringComparer.Ordinal)), string.Join("\n", actualLeaves), "shared multi-way split leaf boundaries and bytes");
        var writer = new StorageCommitter(pager, before); await writer.CommitAsync(plan);
        tree = new StorageTree(pager, schema, root, plan.Meta.PageCount);
        var rows = new List<StorageRecord>(); var logicalPages = 0;
        await foreach (var row in tree.ScanAsync(pageRead: () => logicalPages++)) rows.Add(row);
        Corpus.Equal(600, rows.Count, "paged tree scan count");
        Corpus.Equal(true, logicalPages > 2, "scan charges each logical node");
        Corpus.Equal(1, pager.ResidentPages, "scan evicts within one-page cache");
        Corpus.Equal(true, (await tree.FindAsync(Integer(400, 4)))!.Cells[1]!.AsSpan().SequenceEqual(Integer(-4000000000000, 8)), "paged multiway replacement value");
        var readSnapshot = tree;
        // Deterministic permutation exercises both sibling preferences, merge
        // overflow, balanced re-split and repeated root collapse.
        var permutation = Enumerable.Range(1, 600).OrderBy(i => i * 137 % 601).ToArray();
        for (var index = 0; index < permutation.Length; index++)
        {
            tree = await tree.DeleteAsync(Integer(permutation[index], 4));
            if (index % 37 == 0)
            {
                var n = 0;
                await foreach (var row in tree.ScanAsync()) n++;
                Corpus.Equal(599 - index, n, "delete rebalance scan count");
            }
        }
        Corpus.Equal(true, tree.IsEmpty, "deletion collapses final root");
        Corpus.Equal(true, await readSnapshot.FindAsync(Integer(1, 4)) is not null, "reader snapshot survives all deletes");
        Corpus.Equal(-logicalPages, tree.PageDelta, "deletion retires exact old tree node count");

        // All shared plaintext interior pages must re-encode byte-exactly, even
        // the legal zero-separator node in the near-cap separator fixture.
        var interiors = 0;
        foreach (var name in new[] { "tall_tree.jed", "max_sep_table.jed", "compact_leaf_table.jed", "multiway_split.jed" })
        {
            var image = await File.ReadAllBytesAsync(Path.Combine(fixtureRoot, name));
            for (var at = 512; at < image.Length; at += 256)
                if (image[at] == 3)
                {
                    var page = image.AsSpan(at, 256);
                    Corpus.Equal(true, StorageInterior.Parse(page, (uint)(image.Length / 256)).Encode(256).AsSpan().SequenceEqual(page), "interior golden re-encoding");
                    interiors++;
                }
        }
        Console.WriteLine($"PASS COW tree: shared multi-way split bytes, {interiors} interior goldens, async bounded-cache scans, immutable snapshots, 600 permuted deletes and exact retirement delta");
    }

    private static async Task FreeListBoundary(byte[] image)
    {
        await using var device = new MemoryBlockStore(image);
        var before = await StorageRecovery.OpenAsync(device);
        // At 256 bytes each list page holds 60 entries. Starting with 63 safe
        // pages, the catalog draws one and two list pages draw two more. The
        // canonical monotone serializer keeps the resulting empty second chunk.
        before = before with { Meta = before.Meta with { PageCount = 66 }, FreePages = new(Enumerable.Range(3, 63).Select(value => (uint)value)) };
        var allocation = new StorageAllocation(before, eagerReclaim: false);
        var root = allocation.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
        var plan = allocation.Prepare(root, 1);
        Corpus.Equal(2, plan.FreeListPages.Count, "free-list boundary retains allocated chunks");
        Corpus.Equal(true, plan.Body.Any(page => StorageFormat.ParsePage(page.Bytes) is { Type: 7, Count: 0 }), "empty free-list tail encoded");
        var writer = new StorageCommitter(new StoragePager(device, 256), before);
        await writer.CommitAsync(plan);
        var recovered = await StorageRecovery.OpenAsync(device);
        Corpus.Equal(60, recovered.FreePages.Count, "empty free-list tail reopens");
        Corpus.Equal(2, recovered.FreeListPages.Count, "empty trailing list page stays protected");
    }

    private static async Task RelaxedCommits(byte[] image)
    {
        await using var device = new RecordingStore(image);
        var before = await StorageRecovery.OpenAsync(device);
        before = before with { Meta = before.Meta with { PageCount = 32 }, FreePages = new(Enumerable.Range(3, 29).Select(id => (uint)id)) };
        var pager = new StoragePager(device, 256);
        var writer = new StorageCommitter(pager, before);
        var deadline = await writer.BeginPlanAsync(false, 999, 1);
        Corpus.Equal(1999L, deadline!.Value, "deadline retains fractional milliseconds");
        StorageCommitPlan Plan()
        {
            var allocation = new StorageAllocation(writer.Current, eagerReclaim: false);
            var root = allocation.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
            allocation.CollectUnreachable(new HashSet<uint> { root }, default);
            return allocation.Prepare(root, 1, truncate: true);
        }
        var first = Plan(); await writer.CommitAsync(first, synchronous: false, deadlineMicroseconds: deadline);
        Corpus.Equal(0, device.Events.Count(item => item == "sync"), "relaxed commit omits barrier");
        Corpus.Equal(1, writer.Current.FloorSlot, "relaxed commit preserves durable floor slot");
        var firstIds = first.Body.Select(page => page.Page).ToHashSet();
        var second = Plan();
        Corpus.Equal(1U, second.LastReclamationLive!.Value, "older batch dependencies do not inflate reclamation live count");
        Corpus.Equal(false, second.Body.Any(page => firstIds.Contains(page.Page)), "batch never rewrites an earlier body page");
        Corpus.Equal(false, second.Body.Any(page => page.Page == 2), "newly orphaned floor root is not reusable inside batch");
        Corpus.Equal((uint)(first.Body.Length + second.Body.Length), second.Meta.ManifestEntries, "manifest includes prior batch body writes");
        Corpus.Equal(false, second.FreePages.Overlaps(firstIds), "pending body pages excluded from persisted free list");
        var orphanAllocation = new StorageAllocation(writer.Current, eagerReclaim: false);
        var orphanRoot = orphanAllocation.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
        var orphan = orphanAllocation.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
        orphanAllocation.CollectUnreachable(new HashSet<uint> { orphanRoot }, default);
        var orphanPlan = orphanAllocation.Prepare(orphanRoot, 1);
        Corpus.Equal(2U, orphanPlan.LastReclamationLive!.Value, "current orphan writes count toward reclamation threshold");
        Corpus.Equal(false, orphanPlan.FreePages.Contains(orphan), "current orphan write remains a protected manifest dependency");
        await writer.CommitAsync(second, synchronous: false, deadlineMicroseconds: deadline);
        Corpus.Equal(first.Meta.PageCount <= second.Meta.PageCount, true, "batch cannot lower high-water after its first commit");
        Corpus.Equal(3UL, (await StorageRecovery.OpenAsync(device)).Meta.TransactionId, "complete cumulative manifest reopens newest generation");
        // Losing any earlier member rejects the entire newest candidate, even
        // when that member is already orphaned by the latest catalog root.
        var torn = first.Body[0].Bytes.ToArray(); torn[20] ^= 1;
        await device.WriteAsync((long)first.Body[0].Page * 256, torn);
        Corpus.Equal(1UL, (await StorageRecovery.OpenAsync(device)).Meta.TransactionId, "lost earlier batch page falls back to durable floor");
        await device.WriteAsync((long)first.Body[0].Page * 256, first.Body[0].Bytes);
        _ = await writer.BeginPlanAsync(false, 1998, 1);
        Corpus.Equal(0, device.Events.Count(item => item == "sync"), "before deadline does not flush");
        var nextDeadline = await writer.BeginPlanAsync(false, 1999, 1);
        Corpus.Equal(2999L, nextDeadline!.Value, "deadline flush reuses same captured clock reading");
        Corpus.Equal(1, device.Events.Count(item => item == "sync"), "deadline issues one barrier");
        Corpus.Equal(true, writer.Current.Batch is null, "flush clears pending batch");
        Corpus.Equal(writer.Current.Slot, writer.Current.FloorSlot, "flush advances durable floor");
        var next = Plan();
        Corpus.Equal(2u, next.Meta.Root, "floor orphan becomes reusable after flush");
        await writer.CommitAsync(next);
        Corpus.Equal(2, device.Events.Count(item => item == "sync"), "synchronous commit follows cleared batch");

        foreach (var fail in new[] { false, true })
        {
            await using var capped = new RecordingStore(image) { FailSync = fail };
            var initial = await StorageRecovery.OpenAsync(capped); var cappedPager = new StoragePager(capped, 256);
            var cappedWriter = new StorageCommitter(cappedPager, initial); var allocation = new StorageAllocation(initial);
            var root = allocation.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
            for (var i = 1; i < StorageFormat.InlineCapacity(256); i++) allocation.AddPage(StorageFormat.EncodePage(256, new(4, 0, 0, 0), []));
            await cappedWriter.CommitAsync(allocation.Prepare(root, 1), synchronous: false);
            Corpus.Equal(2UL, cappedWriter.Current.Meta.TransactionId, "cap flush failure cannot fail published commit");
            Corpus.Equal(1, capped.Events.Count(item => item == "sync"), "cap issues one barrier");
            Corpus.Equal(fail, cappedWriter.IsPoisoned, "cap flush failure poisons storage");
            if (fail)
            {
                try { _ = await cappedPager.ReadPageAsync(root); throw new InvalidDataException("revoked pager remained readable"); }
                catch (SqlException error) when (error.SqlState == SqlStates.IoError) { }
                try { await cappedWriter.FlushAsync(); throw new InvalidDataException("poisoned close flush succeeded"); }
                catch (SqlException error) when (error.SqlState == SqlStates.IoError) { }
            }
            else Corpus.Equal(true, cappedWriter.Current.Batch is null, "successful cap flush clears batch");
        }
        Console.WriteLine("PASS relaxed COW invariants: cumulative manifests, floor-safe reuse, orphan exclusion, torn-batch fallback, precise deadlines, cap barriers and post-publication revocation");
    }

    private static async Task Commit(byte[] image)
    {
        await using var device = new RecordingStore(image);
        var before = await StorageRecovery.OpenAsync(device);
        var writer = new StorageCommitter(new StoragePager(device, 256), before);
        var allocation = new StorageAllocation(before);
        var root = allocation.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
        // Dirty orphans are intentional: even an unreferenced body write must
        // remain protected until another generation succeeds.
        for (var i = 0; i < 49; i++) allocation.AddPage(StorageFormat.EncodePage(256, new(4, 0, 0, 0), [(byte)i]));
        var plan = allocation.Prepare(root, 1);
        Corpus.Equal(2u, plan.Meta.ManifestPages, "overflow manifest planning");
        device.Events.Clear();
        await writer.CommitAsync(plan);
        Corpus.Equal("grow", device.Events[0], "preallocate before writes");
        Corpus.Equal("write:0", device.Events[^2], "alternate meta follows body and manifest");
        Corpus.Equal("sync", device.Events[^1], "one final commit barrier");
        Corpus.Equal(1, device.Events.Count(value => value == "sync"), "steady commit sync count");
        var reopened = await StorageRecovery.OpenAsync(device);
        Corpus.Equal(2UL, reopened.Meta.TransactionId, "complete manifest recovered");
        Corpus.Equal(52, reopened.ProtectedPages.Count, "dirty orphans and overflow pages protected");

        // A valid new generation recovered from a failed process must become
        // durable BEFORE its next body write, including before its growth.
        writer = new StorageCommitter(new StoragePager(device, 256), reopened);
        allocation = new StorageAllocation(reopened);
        root = allocation.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
        foreach (var id in reopened.ProtectedPages) allocation.Reclaim(id);
        allocation.Reclaim(2);
        plan = allocation.Prepare(root, 1);
        device.Events.Clear();
        await writer.CommitAsync(plan);
        Corpus.Equal("sync", device.Events[0], "adopted generation stabilization first");
        Corpus.Equal(2, device.Events.Count(value => value == "sync"), "stabilize plus final barrier");
        reopened = await StorageRecovery.OpenAsync(device);
        Corpus.Equal(3UL, reopened.Meta.TransactionId, "second commit recovered");
        Corpus.Equal(true, reopened.FreePages.Contains(2), "orphan pages persisted for next generation");
        Corpus.Equal(false, reopened.FreePages.Overlaps(reopened.ProtectedPages), "dependencies excluded from free list");
        allocation = new StorageAllocation(writer.Current);
        root = allocation.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
        Corpus.Equal(2u, root, "lowest safe free id reused");
        plan = allocation.Prepare(root, 1);
        device.FailSync = true;
        try { await writer.CommitAsync(plan); throw new InvalidDataException("failed sync accepted"); }
        catch (IOException) { }
        Corpus.Equal(true, writer.IsPoisoned, "failed barrier poisons handle");
        Corpus.Equal(3UL, writer.Current.Meta.TransactionId, "failed commit not published in process");
        var events = device.Events.Count;
        try { await writer.CommitAsync(plan); throw new InvalidDataException("poisoned writer accepted commit"); }
        catch (SqlException error) when (error.SqlState == SqlStates.IoError) { }
        Corpus.Equal(events, device.Events.Count, "poisoned writer does no I/O");
        device.FailSync = false;
        reopened = await StorageRecovery.OpenAsync(device);
        Corpus.Equal(4UL, reopened.Meta.TransactionId, "unacknowledged complete generation may recover");

        // A torn body invalidates the newest manifest even if its meta is valid.
        var changed = plan.Body[0].Bytes.ToArray(); changed[20] ^= 1;
        await device.WriteAsync((long)plan.Body[0].Page * 256, changed);
        reopened = await StorageRecovery.OpenAsync(device);
        Corpus.Equal(3UL, reopened.Meta.TransactionId, "torn body falls back to intact predecessor");

        // Read errors are not corruption and must not silently trigger fallback.
        device.FailReadPage = reopened.Meta.Root;
        try { _ = await StorageRecovery.OpenAsync(device); throw new InvalidDataException("I/O error silently ignored"); }
        catch (IOException) { }
    }

    private static async Task Preallocation(byte[] empty)
    {
        Corpus.Equal(67L * 256, StorageGrowth.Length(3L * 256, 4, 256), "small-file 16 KiB growth floor");
        Corpus.Equal(134L * 256, StorageGrowth.Length(67L * 256, 68, 256), "small-file geometric growth");
        Corpus.Equal(8192L * 256, StorageGrowth.Length(4096L * 256, 4097, 256), "large-file 1 MiB growth cap");
        Corpus.Equal(4L * 65536, StorageGrowth.Length(2L * 65536, 3, 65536), "growth floor scales to large pages");
        await using var store = new RecordingStore(empty);
        var before = await StorageRecovery.OpenAsync(store);
        var writer = new StorageCommitter(new StoragePager(store, 256), before);
        var allocation = new StorageAllocation(before);
        var root = allocation.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
        var plan = allocation.Prepare(root, 1);
        await writer.CommitAsync(plan);
        Corpus.Equal(67L * 256, await store.GetLengthAsync(), "commit uses geometric physical reserve");
        Corpus.Equal(plan.Meta.PageCount, (await StorageRecovery.OpenAsync(store)).Meta.PageCount, "physical slack does not change logical high-water");
        var tail = new byte[checked((int)(await store.GetLengthAsync() - plan.Meta.PageCount * 256L))];
        await store.ReadAsync(plan.Meta.PageCount * 256L, tail);
        Corpus.Equal(true, StorageFormat.IsZero(tail), "unreferenced preallocation tail stays zero");
    }

    private static async Task TrailingTruncation(byte[] empty)
    {
        foreach (var fail in new[] { false, true })
        {
            var image = new byte[100 * 256]; empty.CopyTo(image, 0);
            var meta = new StorageMeta(256, 1, 2, 100, 0, 1);
            var before = new StorageRecovered(meta, 0, 1, [2], new(Enumerable.Range(3, 97).Select(id => (uint)id)), []);
            await using var store = new RecordingStore(image) { FailResize = fail };
            var writer = new StorageCommitter(new StoragePager(store, 256), before);
            var allocation = new StorageAllocation(before, eagerReclaim: false);
            var root = allocation.AddPage(StorageFormat.EncodePage(256, new(1, 0, 0, 0), []));
            var plan = allocation.Prepare(root, 1, truncate: true);
            Corpus.Equal(4u, plan.Meta.PageCount, "only previously free tail trimmed");
            await writer.CommitAsync(plan);
            Corpus.Equal("sync", store.Events[^2], "truncation follows durable barrier");
            Corpus.Equal("grow", store.Events[^1], "physical resize is final operation");
            Corpus.Equal(fail ? 100L * 256 : 68L * 256, await store.GetLengthAsync(), "one geometric growth step retained after shrink");
            Corpus.Equal(false, writer.IsPoisoned, "failed optional shrink does not poison durable commit");
            Corpus.Equal(2UL, writer.Current.Meta.TransactionId, "shrink failure still publishes successful commit");
        }
    }

    private sealed class RecordingStore(byte[] initial) : IBlockStore
    {
        private readonly MemoryBlockStore inner = new(initial);
        internal readonly List<string> Events = [];
        internal bool FailSync;
        internal bool FailResize;
        internal uint? FailReadPage;
        public ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken cancellationToken = default)
        {
            if (FailReadPage is { } id && offset == id * 256L) throw new IOException("injected read failure");
            return inner.ReadAsync(offset, destination, cancellationToken);
        }
        public ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken cancellationToken = default)
        {
            Events.Add($"write:{offset / 256}"); return inner.WriteAsync(offset, source, cancellationToken);
        }
        public ValueTask<long> GetLengthAsync(CancellationToken cancellationToken = default) => inner.GetLengthAsync(cancellationToken);
        public ValueTask SetLengthAsync(long length, CancellationToken cancellationToken = default)
        {
            Events.Add("grow");
            if (FailResize) throw new IOException("injected resize failure");
            return inner.SetLengthAsync(length, cancellationToken);
        }
        public ValueTask SyncAsync(CancellationToken cancellationToken = default)
        {
            Events.Add("sync");
            if (FailSync) throw new IOException("injected barrier failure");
            return inner.SyncAsync(cancellationToken);
        }
        public ValueTask DisposeAsync() => inner.DisposeAsync();
    }

    private static async Task CacheRaces()
    {
        await using var device = new DelayedStore();
        await device.SetLengthAsync(3 * 256);
        await device.WriteAsync(512, StorageFormat.EncodePage(256, new(1, 0, 1, 0), []));
        var pager = new StoragePager(device, 256, 256);
        var first = pager.ReadPageAsync(2).AsTask();
        await device.Started.Task;
        var sameFlight = pager.ReadPageAsync(2).AsTask();
        Corpus.Equal(1, device.Reads, "same-page misses single-flight");
        pager.Invalidate(2);
        await device.WriteAsync(512, StorageFormat.EncodePage(256, new(1, 0, 2, 0), []));
        var newest = await pager.ReadPageAsync(2);
        Corpus.Equal(2u, StorageFormat.ParsePage(newest.Span).Count, "invalidation detaches old flight");
        device.Release.SetResult();
        Corpus.Equal(1u, StorageFormat.ParsePage((await first).Span).Count, "old flight keeps immutable generation");
        Corpus.Equal(1u, StorageFormat.ParsePage((await sameFlight).Span).Count, "old flight waiters share result");
        Corpus.Equal(2u, StorageFormat.ParsePage((await pager.ReadPageAsync(2)).Span).Count, "old completion cannot replace current cache");
        Corpus.Equal(2, device.Reads, "latest generation remains cached");
    }

    private sealed class DelayedStore : IBlockStore
    {
        private readonly MemoryBlockStore inner = new();
        internal readonly TaskCompletionSource Started = new(TaskCreationOptions.RunContinuationsAsynchronously);
        internal readonly TaskCompletionSource Release = new(TaskCreationOptions.RunContinuationsAsynchronously);
        internal int Reads;
        public async ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken cancellationToken = default)
        {
            var call = Interlocked.Increment(ref Reads);
            await inner.ReadAsync(offset, destination, cancellationToken);
            if (call == 1) { Started.SetResult(); await Release.Task.WaitAsync(cancellationToken); }
        }
        public ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken cancellationToken = default) => inner.WriteAsync(offset, source, cancellationToken);
        public ValueTask<long> GetLengthAsync(CancellationToken cancellationToken = default) => inner.GetLengthAsync(cancellationToken);
        public ValueTask SetLengthAsync(long length, CancellationToken cancellationToken = default) => inner.SetLengthAsync(length, cancellationToken);
        public ValueTask SyncAsync(CancellationToken cancellationToken = default) => inner.SyncAsync(cancellationToken);
        public ValueTask DisposeAsync() => inner.DisposeAsync();
    }
}
