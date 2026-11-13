namespace Jed;

public sealed partial class Database
{
    internal Action<string>? MaintenanceStepForTesting { get; set; }
    /// <summary>Rewrites the committed snapshot without free pages. Fails immediately while a reader or writer is active.</summary>
    public Task CompactAsync(string name = "main", CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(name);
        cancellationToken.ThrowIfCancellationRequested();
        if (name != "main") return WithAttachmentAsync(name, target => target.CompactAsync(cancellationToken: cancellationToken));
        return RunImageMaintenanceAsync(name, async (snapshot, state, token) =>
        {
            if (state is null) await CompactMemoryAsync(snapshot, token).ConfigureAwait(false);
            else await CompactFileAsync(state, snapshot, token).ConfigureAwait(false);
        }, cancellationToken);
    }

    private async Task RunImageMaintenanceAsync(string name, Func<DatabaseSnapshot, FileState?, CancellationToken, Task> action, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(name);
        if (name != "main") throw new SqlException(SqlStates.UndefinedObject, "database is not attached");
        cancellationToken.ThrowIfCancellationRequested();
        FileState? state;
        lock (gate)
        {
            Check(); state = fileState;
            if (state?.Options.ReadOnly == true) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "database is read-only");
            if (state is { Client: not null } || state?.NeedsElection == true)
                throw new SqlException(SqlStates.ObjectInUse, "compaction requires the file owner");
        }
        if (state is not null && !state.RoleGate.Wait(0)) throw new SqlException(SqlStates.ObjectInUse, "database ownership operation is active");
        SemaphoreSlim? heldWriter = null; TaskCompletionSource? paused = null;
        try
        {
            lock (gate)
            {
                Check();
                if (state is not null) CheckRole(state.Role);
                if (state is { Client: not null } || state?.NeedsElection == true)
                    throw new SqlException(SqlStates.ObjectInUse, "compaction requires the file owner");
                if (pins.Count != 0 || state?.Ledger?.PinCount > 0 || maintenance is not null ||
                    state?.Admission is not null || state?.Closing == true || !writer.Wait(0))
                    throw new SqlException(SqlStates.ObjectInUse, "database has active readers, writer or maintenance");
                heldWriter = writer;
                paused = new(TaskCreationOptions.RunContinuationsAsynchronously);
                if (state is null) maintenance = paused; else state.Admission = paused;
            }
            if (state is not null)
            {
                await committer.FlushAsync(token: cancellationToken).ConfigureAwait(false);
                _ = await OwnerSnapshotAsync(cancellationToken).ConfigureAwait(false);
            }
            DatabaseSnapshot snapshot;
            lock (gate) snapshot = current with { Pager = pager };
            if (snapshot.Meta.TransactionId == ulong.MaxValue) throw new SqlException(SqlStates.ProgramLimitExceeded, "database transaction identifier exhausted");
            await action(snapshot, state, cancellationToken).ConfigureAwait(false);
        }
        catch (Exception error)
        {
            if (state is not null && committer.IsPoisoned) PoisonFile(error);
            if (error is IOException or UnauthorizedAccessException) throw new SqlException(SqlStates.IoError, error.Message);
            throw;
        }
        finally
        {
            lock (gate)
            {
                if (paused is not null)
                {
                    if (state is null) maintenance = null; else state.Admission = null;
                    paused.TrySetResult();
                }
            }
            heldWriter?.Release(); state?.RoleGate.Release();
        }
    }

    private async ValueTask CompactMemoryAsync(DatabaseSnapshot snapshot, CancellationToken token)
    {
        var output = new MemoryBlockStore(); var installed = false;
        try
        {
            await CopySnapshotImageAsync(snapshot, output, snapshot.Meta.TransactionId + 1, ownedCodec, keyBlock?.Block, token).ConfigureAwait(false);
            var opened = await OpenFileStorageAsync(output, new(), ownedCodec, token).ConfigureAwait(false);
            lock (gate)
            {
                Check(); pager = opened.Pager; committer = new(pager, opened.Recovered);
                current = new(opened.Catalog, opened.Recovered.Meta) { Encrypted = opened.Recovered.Encrypted }; retiredAt.Clear(); installed = true;
            }
            await snapshot.Pager!.Store.DisposeAsync().ConfigureAwait(false);
        }
        finally { if (!installed) await output.DisposeAsync().ConfigureAwait(false); }
    }

    private async ValueTask CompactFileAsync(FileState state, DatabaseSnapshot snapshot, CancellationToken token)
    {
        if (!OperatingSystem.IsLinux()) throw new PlatformNotSupportedException("file compaction currently targets Linux");
        var temporary = state.Path + ".jedtmp"; var original = committer.Current; var role = state.Role;
        var host = state.Options.FileHost;
        var mode = await host.GetPermissionsAsync(state.Path, token).ConfigureAwait(false);
        // The reserved stale name is unlinked first; CreateNew then refuses a
        // concurrent replacement or symlink instead of following it.
        await host.DeleteAsync(temporary).ConfigureAwait(false);
        try
        {
            await using (var output = await host.OpenAsync(temporary, FileMode.CreateNew, token: token).ConfigureAwait(false))
            {
                await host.SetPermissionsAsync(temporary, mode, token).ConfigureAwait(false);
                await CopySnapshotImageAsync(snapshot, output, snapshot.Meta.TransactionId + 1, ownedCodec, keyBlock?.Block, token).ConfigureAwait(false);
                await output.SyncAsync(token).ConfigureAwait(false);
            }
            MaintenanceStepForTesting?.Invoke("image-synced");
            token.ThrowIfCancellationRequested();
            state.Ledger!.BeginPublication();
            var restored = false;
            try
            {
                // Cancellation after closing the old inode cannot strand the
                // live handle between files. Complete installation or poison.
                await pager.Store.DisposeAsync().ConfigureAwait(false);
                try { MaintenanceStepForTesting?.Invoke("before-rename"); await host.MoveAsync(temporary, state.Path, overwrite: true).ConfigureAwait(false); }
                catch
                {
                    var reopened = await host.OpenAsync(state.Path, FileMode.Open).ConfigureAwait(false);
                    lock (gate)
                    {
                        state.LocalStore = reopened;
                        pager = new(new FileRoleStore(reopened, () => CheckRole(role)), original.Meta.PageSize, state.Options.CacheBytes, ownedCodec);
                        committer = new(pager, original); current = new(snapshot.Catalog, original.Meta) { Encrypted = original.Encrypted };
                        restored = true;
                    }
                    throw;
                }
                MaintenanceStepForTesting?.Invoke("after-rename");
                var store = await host.OpenAsync(state.Path, FileMode.Open).ConfigureAwait(false);
                OpenedStorage opened; byte[] meta;
                try
                {
                    opened = await OpenFileStorageAsync(store, state.Options, ownedCodec, CancellationToken.None).ConfigureAwait(false);
                    await host.SyncDirectoryAsync(state.Path).ConfigureAwait(false);
                    meta = new byte[opened.Recovered.Meta.PageSize];
                    await store.ReadAsync((long)opened.Recovered.Slot * meta.Length, meta, CancellationToken.None).ConfigureAwait(false);
                    MaintenanceStepForTesting?.Invoke("before-publish");
                }
                catch { await store.DisposeAsync().ConfigureAwait(false); throw; }
                lock (gate)
                {
                    state.LocalStore = store;
                    pager = new(new FileRoleStore(store, () => CheckRole(role)), opened.Recovered.Meta.PageSize, state.Options.CacheBytes, ownedCodec);
                    committer = new(pager, opened.Recovered); committer.Adopt(opened.Recovered, stabilized: true);
                    current = new(opened.Catalog, opened.Recovered.Meta) { Encrypted = opened.Recovered.Encrypted }; keyBlock = opened.KeyBlock;
                    state.CatalogDirty = false; retiredAt.Clear(); state.Ledger.PublishReplacement(meta);
                }
            }
            catch (Exception error)
            {
                state.Ledger.FailPublication(error);
                // A failed rename successfully reopened the old file above;
                // every failure after replacement requires a fresh open.
                if (!restored) PoisonFile(error);
                throw;
            }
        }
        finally { try { await host.DeleteAsync(temporary).ConfigureAwait(false); } catch (IOException) { } catch (UnauthorizedAccessException) { } }
    }
}
