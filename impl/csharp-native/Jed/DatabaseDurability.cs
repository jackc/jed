namespace Jed;

public sealed partial class Database
{
    internal Func<long> Clock => fileState?.Options.Clock ?? Environment.ClockSource.NowMicroseconds;
    private bool SynchronousCommits => fileState?.Options.Synchronous != Synchronous.Off;

    // Called only for an actual owner commit, after writer admission. Memory
    // domains and page clients never read a durability clock.
    private async ValueTask<long?> PrepareCommitPolicyAsync(CancellationToken token)
    {
        if (fileState is not { Client: null, NeedsElection: false } state) return null;
        long? now = state.Options.FlushIntervalMilliseconds != 0 &&
            (!SynchronousCommits || committer.Current.Batch is not null)
            ? (state.Options.Clock ?? SessionOptions.HostClock)() : null;
        try
        {
            return await committer.BeginPlanAsync(SynchronousCommits, now,
                state.Options.FlushIntervalMilliseconds, token: token).ConfigureAwait(false);
        }
        catch (Exception error) { if (committer.IsPoisoned) PoisonFile(error); throw; }
    }
    private void FinishCommitPolicy()
    {
        if (fileState is not { Client: null, NeedsElection: false } state) return;
        if (committer.IsPoisoned)
        { PoisonFile(new SqlException(SqlStates.IoError, "durability flush failed; reopen the database")); return; }
        lock (gate)
        {
            if (!state.Options.EnableIdleFlush || committer.Current.Batch?.DeadlineMicroseconds is null || state.Closing || state.IdleFlush is { IsCompleted: false }) return;
            state.IdleFlush = IdleFlushAsync(state, state.Role, state.RoleEnded.Token);
        }
    }
    private async Task IdleFlushAsync(FileState state, long role, CancellationToken token)
    {
        try
        {
            while (true)
            {
                await Task.Delay((int)Math.Clamp(state.Options.FlushIntervalMilliseconds, 1, int.MaxValue), token).ConfigureAwait(false);
                SemaphoreSlim held;
                lock (gate)
                {
                    if (state.Closing || state.Role != role || state.Failure is not null || committer.Current.Batch is null) return;
                    held = writer;
                    if (state.Admission is not null || !held.Wait(0)) continue;
                }
                try
                {
                    await state.Publication.WaitAsync(token).ConfigureAwait(false);
                    try
                    {
                        if (state.Closing || state.Role != role) return;
                        if (committer.Current.Batch?.DeadlineMicroseconds is not { } deadline) return;
                        if ((state.Options.Clock ?? SessionOptions.HostClock)() >= deadline)
                            await committer.FlushAsync(token: token).ConfigureAwait(false);
                    }
                    finally { state.Publication.Release(); }
                }
                finally { held.Release(); }
            }
        }
        catch (OperationCanceledException) when (token.IsCancellationRequested) { }
        catch (Exception error) { PoisonFile(error); }
    }
}
