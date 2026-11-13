using Jed;

internal static class SimulationPointerKinds
{
    internal sealed record Result(Dictionary<string, SortedSet<uint>> Kinds, HashSet<uint> Dependencies);
    internal static async Task<Result> Read(byte[] image, StorageCredential credential)
    {
        await using var database = await Database.OpenEncryptedStoreAsync(new MemoryBlockStore(image), credential, count => new byte[count]);
        await using var pin = await database.PinAsync();
        var snapshot = pin.Snapshot; var pager = snapshot.Pager!; var recovered = database.CommittedStorageForTesting();
        var kinds = new Dictionary<string, SortedSet<uint>>(StringComparer.Ordinal);
        void Add(string kind, uint page) { if (page == 0) return; if (!kinds.TryGetValue(kind, out var pages)) kinds.Add(kind, pages = []); pages.Add(page); }
        async Task Chain(StoragePageReference reference, string head, string next)
        {
            Add(head, reference.Page);
            var seen = new HashSet<uint>();
            while (reference.Page != 0)
            {
                if (!seen.Add(reference.Page)) throw StorageFormat.Corrupt("cyclic pointer chain");
                var bytes = await pager.ReadPageAsync(reference);
                var header = StorageFormat.ParsePage(bytes.Span, verifyChecksum: false);
                Add(next, header.Next); reference = new(header.Next, bytes.Slice(16, 16).ToArray());
            }
        }
        async Task Tree(StoragePageReference reference, StorageLeafSchema schema, bool gist, int depth = 0)
        {
            if (reference.Page == 0) return;
            if (depth > 128) throw StorageFormat.Corrupt("pointer tree exceeds depth limit");
            var bytes = await pager.ReadPageAsync(reference);
            if (gist)
            {
                foreach (var child in StorageGistPage.Parse(bytes.Span, snapshot.Meta.PageCount, true).Children)
                { Add("gist_child", child.Page); await Tree(child, schema, true, depth + 1); }
                return;
            }
            var header = StorageFormat.ParsePage(bytes.Span, verifyChecksum: false);
            if (header.Type == 3)
            {
                var interior = StorageInterior.Parse(bytes.Span, snapshot.Meta.PageCount, true);
                for (var i = 0; i < interior.Children.Length; i++)
                { Add("child", interior.Children[i]); await Tree(new(interior.Children[i], interior.Tags![i]), schema, false, depth + 1); }
            }
            else
            {
                var leaf = StorageLeaf.Parse(bytes, schema, true);
                for (var i = 0; i < leaf.Count; i++)
                {
                    var row = leaf.Record(i);
                    for (var column = 0; column < row.Cells.Length; column++)
                        if (schema.Columns[column].Width == 0 && StorageLargeValues.External(row.Cells[column]))
                        {
                            var cell = row.Cells[column]!; var info = StorageLargeValues.Inspect(cell, true);
                            await Chain(new(info.First, info.Tag), cell[0] == 4 ? "overflow_lz4" : "overflow", "overflow_next");
                        }
                }
            }
        }
        await Chain(new(snapshot.Meta.Root, snapshot.Encrypted!.RootTag), "catalog", "catalog_next");
        await Chain(new(snapshot.Meta.FreeListHead, snapshot.Encrypted.FreeListTag), "free", "free_next");
        foreach (var page in recovered.ManifestPages) Add("manifest", page);
        foreach (var table in snapshot.Catalog.Tables)
        {
            Add("root", table.Root); await Tree(new(table.Root, table.RootTag), table.LeafSchema(), false);
            foreach (var index in table.Indexes ?? [])
            { Add("root", index.Root); await Tree(new(index.Root, index.RootTag), new([]), index.Kind == 2); }
        }
        return new(kinds, recovered.ProtectedPages.Union(recovered.ManifestPages).ToHashSet());
    }
}
