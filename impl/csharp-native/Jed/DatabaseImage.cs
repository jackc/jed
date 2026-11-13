namespace Jed;

public sealed partial class Database
{
    /// <summary>Exports the committed plaintext snapshot as a garbage-free database image.</summary>
    public async Task<byte[]> SerializeAsync(string name = "main", CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(name);
        if (name != "main") return await WithAttachmentAsync(name, false, target => target.SerializeAsync(cancellationToken: cancellationToken)).ConfigureAwait(false);
        await using var pin = await PinAsync(cancellationToken).ConfigureAwait(false);
        var snapshot = pin.Snapshot; var source = snapshot.Pager ?? pager;
        if (source.Encrypted) throw Errors.Unsupported("encrypted byte-buffer export");
        await using var output = new MemoryBlockStore();
        await CopySnapshotImageAsync(snapshot, output, snapshot.Meta.TransactionId, null, null, cancellationToken).ConfigureAwait(false);
        var length = await output.GetLengthAsync(cancellationToken).ConfigureAwait(false);
        if (length > Array.MaxLength) throw new SqlException(SqlStates.ProgramLimitExceeded, "serialized image exceeds the byte-buffer limit");
        var result = new byte[(int)length]; await output.ReadAsync(0, result, cancellationToken).ConfigureAwait(false); return result;
    }

    /// <summary>Copies and opens a plaintext database image in memory.</summary>
    public static Task<Database> DeserializeAsync(ReadOnlyMemory<byte> image, CancellationToken cancellationToken = default) =>
        DeserializeCoreAsync(image, cancellationToken, null, 0);

    private static async Task<Database> DeserializeCoreAsync(ReadOnlyMemory<byte> image, CancellationToken cancellationToken, DatabaseEnvironment? environment, long maxStorageBytes)
    {
        cancellationToken.ThrowIfCancellationRequested();
        if (maxStorageBytes < 0) throw new ArgumentOutOfRangeException(nameof(maxStorageBytes));
        var store = new MemoryBlockStore(image.Span);
        try
        {
            if (await InspectVariantAsync(store, cancellationToken).ConfigureAwait(false) != 0)
                throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "an in-memory database cannot be encrypted");
            environment ??= DatabaseEnvironment.Builder().Build();
            var database = await OpenStoreAsync(store, cancellationToken, ephemeral: true, environment: environment).ConfigureAwait(false);
            try
            {
                database.current = database.current with { Catalog = BindEnvironment(database.current.Catalog, environment) };
                database.SetStorageLimit(maxStorageBytes);
                return database;
            }
            catch { await database.DisposeAsync().ConfigureAwait(false); throw; }
        }
        catch { await store.DisposeAsync().ConfigureAwait(false); throw; }
    }

    private async ValueTask<StorageMeta> CopySnapshotImageAsync(DatabaseSnapshot snapshot, IBlockStore destination,
        ulong transactionId, StoragePageCodec? codec, StorageKeyBlock? selectedKeyBlock, CancellationToken token)
    {
        var source = snapshot.Pager ?? pager;
        if (source.Encrypted != (codec is not null)) throw new ArgumentException("image copy requires the same page layout");
        var output = new StorageImageWriter(destination, snapshot.Meta.PageSize, codec);
        var tables = new List<CatalogTable>();
        foreach (var table in snapshot.Catalog.Tables.OrderBy(item => item.Name, Utf8NameComparer.Instance))
        {
            var copied = await Tree(table, null, snapshot).CopyImageAsync(output, token).ConfigureAwait(false);
            var indexes = new List<CatalogIndex>();
            foreach (var index in (table.Indexes ?? []).OrderBy(item => item.Name, Utf8NameComparer.Instance))
            {
                var root = index.Kind == 2
                    ? await StorageGistPage.CopyAsync(source, new(index.Root, index.RootTag), snapshot.Meta.PageCount, output, token).ConfigureAwait(false)
                    : await IndexTree(index, null, snapshot).CopyImageAsync(output, token).ConfigureAwait(false);
                indexes.Add(index with { Root = root.Page, RootTag = root.Tag });
            }
            tables.Add(table with { Root = copied.Page, RootTag = copied.Tag, Indexes = indexes.ToArray() });
        }
        var catalog = snapshot.Catalog with { Tables = tables.ToArray(), Pages = [] };
        var catalogRoot = await output.CatalogAsync(catalog, token).ConfigureAwait(false);
        var image = await output.FinishAsync(catalogRoot, transactionId, selectedKeyBlock, token).ConfigureAwait(false);
        if (image.LivePages != snapshot.Meta.LivePages) throw StorageFormat.Corrupt("snapshot live page count differs from reachable pages");
        return image;
    }
}
