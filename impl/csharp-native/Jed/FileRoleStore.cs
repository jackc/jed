namespace Jed;

// Every actual byte-device access checks its owning role. Closing first blocks
// new operations, then drains outstanding positioned I/O before the driver may
// release presence. Cached cursors can finish; later page fetches fail closed.
internal sealed class FileRoleStore(IBlockStore store, Action checkRole) : IBlockStore
{
    private readonly object gate = new();
    private int active;
    private bool closing;
    private TaskCompletionSource? drained;
    private Task? disposal;
    private void Enter()
    {
        lock (gate)
        {
            if (closing) throw new SqlException(SqlStates.ConnectionFailure, "database file role ended");
            checkRole(); active++;
        }
    }
    private void Leave()
    { lock (gate) { active--; if (active == 0) drained?.TrySetResult(); } }
    public async ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken cancellationToken = default)
    { Enter(); try { await store.ReadAsync(offset, destination, cancellationToken).ConfigureAwait(false); } catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        { throw new SqlException(SqlStates.IoError, error.Message); } finally { Leave(); } }
    public async ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken cancellationToken = default)
    { Enter(); try { await store.WriteAsync(offset, source, cancellationToken).ConfigureAwait(false); } catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        { throw new SqlException(SqlStates.IoError, error.Message); } finally { Leave(); } }
    public async ValueTask<long> GetLengthAsync(CancellationToken cancellationToken = default)
    { Enter(); try { return await store.GetLengthAsync(cancellationToken).ConfigureAwait(false); } catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        { throw new SqlException(SqlStates.IoError, error.Message); } finally { Leave(); } }
    public async ValueTask SetLengthAsync(long length, CancellationToken cancellationToken = default)
    { Enter(); try { await store.SetLengthAsync(length, cancellationToken).ConfigureAwait(false); } catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        { throw new SqlException(SqlStates.IoError, error.Message); } finally { Leave(); } }
    public async ValueTask SyncAsync(CancellationToken cancellationToken = default)
    { Enter(); try { await store.SyncAsync(cancellationToken).ConfigureAwait(false); } catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        { throw new SqlException(SqlStates.IoError, error.Message); } finally { Leave(); } }
    public ValueTask DisposeAsync()
    {
        lock (gate)
        {
            if (disposal is not null) return new(disposal);
            closing = true;
            if (active != 0) drained = new(TaskCreationOptions.RunContinuationsAsynchronously);
            disposal = CloseAsync(drained?.Task ?? Task.CompletedTask); return new(disposal);
        }
    }
    private async Task CloseAsync(Task wait)
    { await wait.ConfigureAwait(false); await store.DisposeAsync().ConfigureAwait(false); }
}
