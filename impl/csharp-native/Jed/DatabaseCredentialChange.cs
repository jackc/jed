namespace Jed;

public sealed partial class Database
{
    /// <summary>Rewraps an encrypted file's data key for a new credential. This owner-only operation never waits for an active writer.</summary>
    public async Task ChangeCredentialAsync(DatabaseCredential credential, string name = "main", CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(credential); ArgumentNullException.ThrowIfNull(name);
        cancellationToken.ThrowIfCancellationRequested();
        if (name != "main")
        {
            await WithAttachmentAsync(name, target => target.ChangeCredentialAsync(credential, cancellationToken: cancellationToken)).ConfigureAwait(false);
            return;
        }
        FileState state;
        lock (gate)
        {
            Check(); state = fileState ?? throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "credential changes require a file database");
        }
        // The role lock protects the held key from disposal or ownership change
        // while deriving the new KEK. It does not block an existing SQL writer.
        if (!state.RoleGate.Wait(0)) throw new SqlException(SqlStates.ObjectInUse, "database ownership operation is active");
        SemaphoreSlim? heldWriter = null;
        try
        {
            StorageKeySelection selected; StoragePageCodec codec;
            lock (gate)
            {
                Check(); CheckRole(state.Role);
                if (state.Client is not null || state.NeedsElection) throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "credential change requires the file owner");
                if (state.Options.ReadOnly) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "database is read-only");
                if (state.Closing || state.Admission is not null) throw new SqlException(SqlStates.ObjectInUse, "database ownership is changing");
                codec = ownedCodec ?? throw new SqlException(SqlStates.CredentialNotApplicable, "credential does not apply to a plaintext file");
                selected = keyBlock ?? throw StorageFormat.Corrupt("encrypted database has no selected key block");
            }
            if (selected.Block.Generation == ulong.MaxValue) throw new SqlException(SqlStates.ProgramLimitExceeded, "credential generation exhausted");
            // Argon2id, wrapping and their entropy draws all precede the writer
            // gate. Only the key block changes; no page nonce is consumed.
            var replacement = codec.Wrap(pager.PageSize, selected.Block.Generation + 1, credential.Value);
            cancellationToken.ThrowIfCancellationRequested();
            lock (gate)
            {
                Check();
                if (!writer.Wait(0)) throw new SqlException(SqlStates.ObjectInUse, "database has an active writer");
                heldWriter = writer;
                if (keyBlock?.Block.Generation != selected.Block.Generation) throw new SqlException(SqlStates.ObjectInUse, "credential changed while deriving replacement");
            }
            cancellationToken.ThrowIfCancellationRequested();
            try
            {
                await committer.FlushAsync(token: cancellationToken).ConfigureAwait(false);
                await SyncOwnerDirectoryAsync(cancellationToken).ConfigureAwait(false);
                var changed = await StorageCredentialChange.ReplaceAsync(pager.Store, selected, replacement).ConfigureAwait(false);
                lock (gate) { keyBlock = changed; }
            }
            catch (Exception error)
            {
                PoisonFile(error is SqlException sql ? sql : new SqlException(SqlStates.IoError, "credential change failed; reopen the database"));
                if (error is SqlException) throw;
                throw new SqlException(SqlStates.IoError, "credential change failed; reopen the database");
            }
        }
        finally { heldWriter?.Release(); state.RoleGate.Release(); }
    }
}

internal static class StorageCredentialChange
{
    internal static async ValueTask<StorageKeySelection> ReplaceAsync(IBlockStore store, StorageKeySelection selected,
        StorageKeyBlock replacement, CancellationToken cancellationToken = default)
    {
        if (selected.Block.Generation == ulong.MaxValue || replacement.Generation != selected.Block.Generation + 1 ||
            replacement.Bytes.Length != selected.Block.Bytes.Length)
            throw new ArgumentException("invalid replacement key block");
        cancellationToken.ThrowIfCancellationRequested();
        var size = replacement.Bytes.Length; var first = selected.Page == 2 ? 3u : 2u;
        // Once storage starts, complete both barriers even if the caller stops
        // waiting. Any I/O failure leaves exactly the old or new durable key.
        await store.SyncAsync(CancellationToken.None).ConfigureAwait(false);
        await store.WriteAsync((long)first * size, replacement.Bytes, CancellationToken.None).ConfigureAwait(false);
        await store.SyncAsync(CancellationToken.None).ConfigureAwait(false);
        await store.WriteAsync((long)selected.Page * size, replacement.Bytes, CancellationToken.None).ConfigureAwait(false);
        await store.SyncAsync(CancellationToken.None).ConfigureAwait(false);
        return new(replacement, 2, false);
    }
}
