namespace Jed;

// A deferred cell belongs to one immutable snapshot. Concurrent consumers share
// one load; cancellation of a waiting consumer does not cancel another reader's
// successfully started load. The owning query's token governs the physical read.
internal sealed class DeferredSqlValue(Func<CancellationToken, ValueTask<SqlValue>> loader)
{
    private readonly object gate = new();
    private Task<SqlValue>? loading;
    private SqlValue value;
    private bool resolved;
    internal ValueTask<SqlValue> ResolveAsync(CancellationToken cancellationToken = default)
    {
        // Publish the complete immutable value before the flag. Resident page
        // decodes avoid allocating a completed Task, and repeated cell reads do
        // not reacquire the load lock.
        if (Volatile.Read(ref resolved)) return new(value);
        lock (gate)
        {
            if (resolved) return new(value);
            if (loading is null)
            {
                try
                {
                    var pending = loader(cancellationToken);
                    if (pending.IsCompletedSuccessfully)
                    {
                        value = pending.Result;
                        Volatile.Write(ref resolved, true);
                        return new(value);
                    }
                    loading = pending.AsTask();
                }
                catch (Exception error) { loading = Task.FromException<SqlValue>(error); }
            }
            if (loading.IsCompletedSuccessfully)
            {
                value = loading.Result;
                Volatile.Write(ref resolved, true);
                return new(value);
            }
            return new(loading.WaitAsync(cancellationToken));
        }
    }
}
