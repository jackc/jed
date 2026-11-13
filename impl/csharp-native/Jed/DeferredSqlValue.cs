namespace Jed;

// A deferred cell belongs to one immutable snapshot. Concurrent consumers share
// one load; cancellation of a waiting consumer does not cancel another reader's
// successfully started load. The owning query's token governs the physical read.
internal sealed class DeferredSqlValue(Func<CancellationToken, ValueTask<SqlValue>> loader)
{
    private readonly object gate = new();
    private Task<SqlValue>? loading;
    internal ValueTask<SqlValue> ResolveAsync(CancellationToken cancellationToken = default)
    {
        lock (gate)
        {
            if (loading is null)
            {
                try
                {
                    var pending = loader(cancellationToken);
                    if (pending.IsCompletedSuccessfully) loading = Task.FromResult(pending.Result);
                    else loading = pending.AsTask();
                }
                catch (Exception error) { loading = Task.FromException<SqlValue>(error); }
            }
            if (loading.IsCompletedSuccessfully) return new(loading.Result);
            return new(loading.WaitAsync(cancellationToken));
        }
    }
}
