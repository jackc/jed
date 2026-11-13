namespace Jed;

public sealed partial class Database
{
    // A simulated process death must never run graceful-close durability work.
    // The host first revokes its streams, then revokes this role and releases its
    // exclusive claim. Ordinary public disposal retains the durable close path.
    internal async ValueTask AbortInjectedHostAsync()
    {
        var state = fileState ?? throw new InvalidOperationException("database is not file-backed");
        if (state.Options.RoleHost is null) throw new InvalidOperationException("only an injected role host may abort a process");
        lock (gate)
        {
            state.Failure = new(SqlStates.ConnectionFailure, "host process ended");
            EndFileRole();
        }
        state.Server?.Abort();
        if (state.Client is not null) await state.Client.DisposeAsync().ConfigureAwait(false);
        if (state.Server is not null) await state.Server.DisposeAsync().ConfigureAwait(false);
        if (state.LocalStore is not null) await state.LocalStore.DisposeAsync().ConfigureAwait(false);
        state.Options.RoleHost.Release(state.Path);
    }
}
