namespace Jed;

public sealed partial class Database
{
    // A host maintenance/export operation retains registry ownership without
    // pinning a snapshot: compaction must still be able to drain target readers.
    private async Task<T> WithAttachmentAsync<T>(string name, bool write, Func<Database, Task<T>> action)
    {
        var attached = AcquireAttachmentRegistry();
        try
        {
            var target = attached.FirstOrDefault(item => item.Name == name)
                ?? throw new SqlException(SqlStates.UndefinedObject, "database is not attached");
            if (write && target.ReadOnly) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "attached database is read-only");
            return await action(target.Database).ConfigureAwait(false);
        }
        finally { ReleaseAttachmentRegistry(); }
    }

    private Task WithAttachmentAsync(string name, Func<Database, Task> action) =>
        WithAttachmentAsync(name, true, async target => { await action(target).ConfigureAwait(false); return true; });
}
