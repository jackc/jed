namespace Jed;

public sealed partial class Database
{
    internal void BeforeNextAcknowledgementForTesting(Action hook)
    {
        ArgumentNullException.ThrowIfNull(hook);
        lock (gate)
        {
            Check();
            if (fileState?.Server is not { } server) throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "acknowledgement injection requires an owner");
            if (Interlocked.CompareExchange(ref server.BeforeAcknowledgementForTesting, hook, null) is not null)
                throw new SqlException(SqlStates.ObjectInUse, "acknowledgement hook already armed");
        }
    }
    /// <summary>The currently published main-domain storage generation.</summary>
    public ulong Version { get { lock (gate) { Check(); return current.Meta.TransactionId; } } }
    /// <summary>The oldest retained main-domain snapshot, or the current generation.</summary>
    public ulong OldestLiveVersion
    {
        get
        {
            lock (gate)
            {
                Check(); var oldest = pins.Count == 0 ? current.Meta.TransactionId : pins.Keys.Min();
                if (fileState?.Ledger?.OldestPin is { } remote) oldest = Math.Min(oldest, remote);
                return oldest;
            }
        }
    }
    /// <summary>Pin the committed roots until the returned read handle is disposed.</summary>
    public Task<Session> OpenReadSessionAsync(SessionOptions? options = null, CancellationToken cancellationToken = default) =>
        OpenAdmittedSession(true, options, cancellationToken);
    /// <summary>Await the writer gate and retain it until commit, rollback, or disposal.</summary>
    public Task<Session> OpenWriteSessionAsync(SessionOptions? options = null, CancellationToken cancellationToken = default) =>
        OpenAdmittedSession(false, options, cancellationToken);
    /// <summary>Await writer admission up to the supplied host deadline (55P03 on timeout).</summary>
    public async Task<Session> OpenWriteSessionAsync(TimeSpan lockTimeout, SessionOptions? options = null, CancellationToken cancellationToken = default)
    {
        if (lockTimeout <= TimeSpan.Zero || lockTimeout.TotalMilliseconds > uint.MaxValue - 1) throw new ArgumentOutOfRangeException(nameof(lockTimeout));
        using var deadline = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        deadline.CancelAfter(lockTimeout);
        try { return await OpenAdmittedSession(false, options, deadline.Token).ConfigureAwait(false); }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested && deadline.IsCancellationRequested)
        { throw new SqlException(SqlStates.LockNotAvailable, "writer admission deadline exceeded"); }
    }
    private async Task<Session> OpenAdmittedSession(bool readOnly, SessionOptions? options, CancellationToken cancellationToken)
    {
        var session = CreateSession(options ?? new());
        try
        {
            await using var rows = await session.QueryAsync(readOnly ? "BEGIN READ ONLY" : "BEGIN", cancellationToken: cancellationToken).ConfigureAwait(false);
            session.MarkReadHandle(readOnly); return session;
        }
        catch { await session.DisposeAsync().ConfigureAwait(false); throw; }
    }
}

public sealed partial class Session
{
    private bool hostReadSession;
    internal void MarkReadHandle(bool readOnly) => hostReadSession = readOnly;
    /// <summary>Publish the current write transaction and close this session.</summary>
    public async Task CommitAsync(CancellationToken cancellationToken = default)
    {
        if (hostReadSession) { await DisposeAsync().ConfigureAwait(false); return; }
        try { await using var rows = await QueryAsync("COMMIT", cancellationToken: cancellationToken).ConfigureAwait(false); }
        finally { await DisposeAsync().ConfigureAwait(false); }
    }
    /// <summary>Discard pending writes and close this session.</summary>
    public async Task RollbackAsync(CancellationToken cancellationToken = default)
    {
        if (hostReadSession) { await DisposeAsync().ConfigureAwait(false); return; }
        try { await using var rows = await QueryAsync("ROLLBACK", cancellationToken: cancellationToken).ConfigureAwait(false); }
        finally { await DisposeAsync().ConfigureAwait(false); }
    }
}
