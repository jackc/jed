namespace Jed;

internal sealed partial class StorageTree
{
    // Open retains interior structure without decoding leaf directories or
    // values. One first-child peek classifies each bottom interior's level;
    // the remaining leaf siblings stay on disk until a query touches them.
    internal async ValueTask<StorageTreeShape?> ValidateSpineAsync(CancellationToken cancellationToken = default)
    {
        var seen = new HashSet<uint>();
        async ValueTask<StorageTreeShape> Visit(Ref reference, int depth)
        {
            cancellationToken.ThrowIfCancellationRequested();
            if (depth > 128 || !seen.Add(reference.Page) || reference.Page < (pager.Encrypted ? 4 : 2) || reference.Page >= pageCount)
                throw StorageFormat.Corrupt("cyclic, repeated or out-of-bounds tree spine");
            var bytes = await pager.ReadPageAsync(new StoragePageReference(reference.Page, reference.Tag), cancellationToken).ConfigureAwait(false);
            var header = StorageFormat.ParsePage(bytes.Span, verifyChecksum: !pager.Encrypted);
            if (header.Type == 2) return new(reference.Page, reference.Tag);
            if (header.Type != 3) throw StorageFormat.Corrupt("tree spine names a non-tree page");
            var interior = StorageInterior.Parse(bytes.Span, pageCount, pager.Encrypted);
            var node = InteriorNode(interior);
            var children = new StorageTreeShape[node.Children!.Length];
            children[0] = await Visit(node.Children[0], depth + 1).ConfigureAwait(false);
            for (var i = 1; i < children.Length; i++)
            {
                var child = node.Children[i];
                if (children[0].IsLeaf)
                {
                    if (!seen.Add(child.Page)) throw StorageFormat.Corrupt("repeated leaf in tree spine");
                    children[i] = new(child.Page, child.Tag);
                }
                else
                {
                    children[i] = await Visit(child, depth + 1).ConfigureAwait(false);
                    if (children[i].Height != children[0].Height) throw StorageFormat.Corrupt("tree spine has unequal child heights");
                }
            }
            return new(reference.Page, reference.Tag, interior.Separators, children);
        }
        return root is null ? null : await Visit(root, 0).ConfigureAwait(false);
    }

    internal static async ValueTask<Catalog> ValidateCatalogSpinesAsync(StoragePager pager, Catalog catalog, StorageMeta meta, CancellationToken token)
    {
        var tables = new CatalogTable[catalog.Tables.Length];
        for (var t = 0; t < tables.Length; t++)
        {
            var table = catalog.Tables[t];
            var shape = await new StorageTree(pager, table.LeafSchema(), table.Root, meta.PageCount, table.RootTag).ValidateSpineAsync(token).ConfigureAwait(false);
            var indexes = (table.Indexes ?? []).ToArray();
            for (var i = 0; i < indexes.Length; i++)
            {
                var index = indexes[i];
                if (index.Kind == 2) indexes[i] = index with { Gist = await GistTree.LoadAsync(pager, table, index, meta, token).ConfigureAwait(false) };
                else if (index.Kind is 0 or 1)
                    indexes[i] = index with { Shape = await new StorageTree(pager, new([]), index.Root, meta.PageCount, index.RootTag).ValidateSpineAsync(token).ConfigureAwait(false) };
            }
            tables[t] = table with { Shape = shape, Indexes = table.Indexes is null ? null : indexes };
        }
        return catalog with { Tables = tables };
    }
}
