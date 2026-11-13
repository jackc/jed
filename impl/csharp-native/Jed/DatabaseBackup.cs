namespace Jed;

public sealed partial class Database
{
    // Canonical page-service §9 intentionally leaves the public backup API
    // unnamed. Keep this internal tooling seam until that product decision.
    internal async Task BackupForTestingAsync(string destination, bool compacted, CancellationToken token = default)
    {
        ArgumentException.ThrowIfNullOrEmpty(destination);
        var host = fileState?.Options.FileHost ?? NativeFileHost.Instance;
        var path = Path.GetFullPath(destination); var temporary = path + ".jedtmp";
        if (await host.ExistsAsync(path, token).ConfigureAwait(false)) throw new SqlException(SqlStates.DuplicateFile, "backup destination exists");
        await using var pin = await PinAsync(token).ConfigureAwait(false);
        if (!compacted && fileState is null) throw Errors.Unsupported("page-identical backup of an in-memory database");
        var snapshot = pin.Snapshot; var source = snapshot.Pager ?? pager;
        if (compacted && snapshot.Meta.TransactionId == ulong.MaxValue) throw new SqlException(SqlStates.ProgramLimitExceeded, "database transaction identifier exhausted");
        StorageKeyBlock? selected = null;
        if (source.Encrypted)
        {
            var first = new byte[source.PageSize]; var second = new byte[source.PageSize];
            await source.Store.ReadAsync(2L * source.PageSize, first, token).ConfigureAwait(false);
            await source.Store.ReadAsync(3L * source.PageSize, second, token).ConfigureAwait(false);
            selected = StorageKeyBlock.Select(first, second).Block;
        }
        var published = false; var ownsTemporary = false;
        try
        {
            await using (var output = await host.OpenAsync(temporary, FileMode.CreateNew, token: token).ConfigureAwait(false))
            {
                ownsTemporary = true;
                if (compacted)
                {
                    var image = await CopySnapshotImageAsync(snapshot, output, snapshot.Meta.TransactionId + 1, source.Codec, selected, token).ConfigureAwait(false);
                    if (image.LivePages != snapshot.Meta.LivePages) throw StorageFormat.Corrupt("snapshot live page count differs from reachable pages");
                }
                else await new StorageSnapshotCopy(source, snapshot, output).CopyAsync(selected, token).ConfigureAwait(false);
                await output.SyncAsync(token).ConfigureAwait(false);
                token.ThrowIfCancellationRequested(); pin.CheckRole();
                MaintenanceStepForTesting?.Invoke("backup-before-rename");
                try { await host.MoveAsync(temporary, path, overwrite: false, token).ConfigureAwait(false); ownsTemporary = false; published = true; }
                catch (IOException)
                {
                    if (await host.ExistsAsync(path, token).ConfigureAwait(false)) throw new SqlException(SqlStates.DuplicateFile, "backup destination exists");
                    throw;
                }
                MaintenanceStepForTesting?.Invoke("backup-after-rename");
                await host.SyncDirectoryAsync(path, token).ConfigureAwait(false); pin.CheckRole();
            }
        }
        catch (Exception error)
        {
            if (published) { try { await host.DeleteAsync(path).ConfigureAwait(false); } catch (IOException) { } catch (UnauthorizedAccessException) { } }
            if (error is IOException or UnauthorizedAccessException) throw new SqlException(SqlStates.IoError, error.Message);
            throw;
        }
        finally
        {
            if (ownsTemporary) { try { await host.DeleteAsync(temporary).ConfigureAwait(false); } catch (IOException) { } catch (UnauthorizedAccessException) { } }
        }
    }
}
