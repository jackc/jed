using System.Security.Cryptography;

namespace Jed;

public sealed partial class Database
{
    /// <summary>Rewrite live records under a fresh data key, or decrypt when credential is null. Requires an idle file owner.</summary>
    public Task RekeyAsync(DatabaseCredential? credential, CancellationToken cancellationToken = default) =>
        RunImageMaintenanceAsync("main", async (snapshot, state, token) =>
        {
            if (state is null) throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "rekey requires a file database");
            if (credential is not null && !ScratchHost!.Encrypted) throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "encrypted rekey requires encrypted scratch storage");
            if (credential is not null && snapshot.Meta.PageSize < 1024) throw Errors.Unsupported("encrypted page size must be at least 1024");
            StoragePageCodec? replacement = null; StorageKeyBlock? block = null;
            try
            {
                if (credential is not null)
                {
                    var key = StorageKeyBlock.Entropy(state.Options.KeyEntropy, 32);
                    try { replacement = new(key, state.Options.KeyEntropy); } finally { CryptographicOperations.ZeroMemory(key); }
                    block = replacement.Wrap(snapshot.Meta.PageSize, 1, credential.Value);
                }
                await ReplaceKeyImageAsync(snapshot, state, replacement, block, token).ConfigureAwait(false);
            }
            finally { if (!ReferenceEquals(replacement, ownedCodec)) replacement?.Dispose(); }
        }, cancellationToken);

    private async ValueTask<StorageMeta> RebuildImageAsync(DatabaseSnapshot snapshot, IBlockStore outputStore,
        StoragePageCodec? replacement, StorageKeyBlock? block, CancellationToken token)
    {
        var output = new StorageImageWriter(outputStore, snapshot.Meta.PageSize, replacement);
        var tables = new List<CatalogTable>();
        foreach (var table in snapshot.Catalog.Tables.OrderBy(item => item.Name, Utf8NameComparer.Instance))
        {
            var root = await Tree(table, null, snapshot).RebuildImageAsync(output, ScratchHost!, fileState!.Options.ScratchEntropy,
                fileState.Options.CacheBytes, null, token).ConfigureAwait(false);
            var indexes = new List<CatalogIndex>();
            foreach (var index in (table.Indexes ?? []).OrderBy(item => item.Name, Utf8NameComparer.Instance))
            {
                var types = index.Kind == 2 ? index.Keys.Select(key => table.Columns[key.Column].Type.Type).ToArray() : null;
                var copied = await IndexTree(index, null, snapshot).RebuildImageAsync(output, ScratchHost!, fileState.Options.ScratchEntropy,
                    fileState.Options.CacheBytes, types, token).ConfigureAwait(false);
                indexes.Add(index with { Root = copied.Page, RootTag = copied.Tag, Shape = null, Gist = null });
            }
            tables.Add(table with { Root = root.Page, RootTag = root.Tag, Shape = null, Indexes = indexes.ToArray() });
        }
        var catalog = snapshot.Catalog with { Tables = tables.ToArray(), Pages = [] };
        var catalogRoot = await output.CatalogAsync(catalog, token).ConfigureAwait(false);
        var image = await output.FinishAsync(catalogRoot, checked(snapshot.Meta.TransactionId + 1), block, token).ConfigureAwait(false);
        if (MaxStorageBytes > 0 && (ulong)image.LivePages * (uint)image.PageSize > (ulong)MaxStorageBytes)
            throw new SqlException(SqlStates.StorageLimitExceeded, "rekey exceeds max_storage_bytes");
        return image;
    }

    private async ValueTask ReplaceKeyImageAsync(DatabaseSnapshot snapshot, FileState state,
        StoragePageCodec? replacement, StorageKeyBlock? block, CancellationToken token)
    {
        if (!OperatingSystem.IsLinux()) throw new PlatformNotSupportedException("file rekey currently targets Linux");
        var temporary = state.Path + ".jedtmp"; var original = committer.Current; var role = state.Role;
        var host = state.Options.FileHost;
        var mode = await host.GetPermissionsAsync(state.Path, token).ConfigureAwait(false); var previousCodec = ownedCodec;
        await host.DeleteAsync(temporary).ConfigureAwait(false);
        try
        {
            await using (var output = await host.OpenAsync(temporary, FileMode.CreateNew, token: token).ConfigureAwait(false))
            {
                await host.SetPermissionsAsync(temporary, mode, token).ConfigureAwait(false);
                await RebuildImageAsync(snapshot, output, replacement, block, token).ConfigureAwait(false);
                await output.SyncAsync(token).ConfigureAwait(false);
            }
            MaintenanceStepForTesting?.Invoke("image-synced"); token.ThrowIfCancellationRequested();
            state.Ledger!.BeginPublication(); var restored = false;
            try
            {
                await pager.Store.DisposeAsync().ConfigureAwait(false);
                try { MaintenanceStepForTesting?.Invoke("before-rename"); await host.MoveAsync(temporary, state.Path, overwrite: true).ConfigureAwait(false); }
                catch
                {
                    var reopened = await host.OpenAsync(state.Path, FileMode.Open).ConfigureAwait(false);
                    lock (gate)
                    {
                        state.LocalStore = reopened;
                        pager = new(new FileRoleStore(reopened, () => CheckRole(role)), original.Meta.PageSize, state.Options.CacheBytes, previousCodec);
                        committer = new(pager, original); current = new(snapshot.Catalog, original.Meta) { Encrypted = original.Encrypted }; restored = true;
                    }
                    throw;
                }
                MaintenanceStepForTesting?.Invoke("after-rename");
                var store = await host.OpenAsync(state.Path, FileMode.Open).ConfigureAwait(false); OpenedStorage opened; byte[] meta;
                try
                {
                    opened = await OpenFileStorageAsync(store, state.Options with { Credential = null }, replacement, CancellationToken.None).ConfigureAwait(false);
                    await host.SyncDirectoryAsync(state.Path).ConfigureAwait(false); meta = new byte[opened.Recovered.Meta.PageSize];
                    await store.ReadAsync((long)opened.Recovered.Slot * meta.Length, meta, CancellationToken.None).ConfigureAwait(false);
                    MaintenanceStepForTesting?.Invoke("before-publish");
                }
                catch { await store.DisposeAsync().ConfigureAwait(false); throw; }
                lock (gate)
                {
                    ownedCodec = replacement; state.Options = state.Options with { Credential = null }; state.LocalStore = store;
                    pager = new(new FileRoleStore(store, () => CheckRole(role)), opened.Recovered.Meta.PageSize, state.Options.CacheBytes, replacement);
                    committer = new(pager, opened.Recovered); committer.Adopt(opened.Recovered, stabilized: true);
                    current = new(opened.Catalog, opened.Recovered.Meta) { Encrypted = opened.Recovered.Encrypted }; keyBlock = opened.KeyBlock;
                    state.CatalogDirty = false; retiredAt.Clear(); state.Ledger.PublishReplacement(meta);
                }
                previousCodec?.Dispose();
            }
            catch (Exception error)
            {
                state.Ledger.FailPublication(error); if (!restored) PoisonFile(error); throw;
            }
        }
        finally { try { await host.DeleteAsync(temporary).ConfigureAwait(false); } catch (IOException) { } catch (UnauthorizedAccessException) { } }
    }
}
