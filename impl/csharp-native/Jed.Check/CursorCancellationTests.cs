using Jed;

internal static class CursorCancellationTests
{
    internal static async Task Run()
    {
        byte[] image;
        await using (var database = await Database.CreateInMemoryAsync(256))
        {
            await using var session = database.CreateSession();
            await session.ExecuteAsync("CREATE TABLE t(id i32 PRIMARY KEY, body bytea)");
            var payload = new byte[8192]; uint state = 71;
            for (var i = 0; i < payload.Length; i++) { state = unchecked(state * 1664525 + 1013904223); payload[i] = (byte)(state >> 24); }
            await session.ExecuteAsync("INSERT INTO t VALUES (1, $1)", [SqlValue.Bytes(payload)]);
            image = await database.SerializeAsync();
        }
        foreach (var mode in new[] { "read", "enumerator", "query" })
        {
            await using var store = new SuspendedStore(image);
            await using var database = await Database.OpenStoreAsync(store);
            await using var session = database.CreateSession();
            using var cancel = new CancellationTokenSource();
            using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(10));
            if (mode == "enumerator") await session.ExecuteAsync("BEGIN");
            var before = session.LifetimeCost;
            await using var rows = await session.QueryAsync("SELECT body FROM t WHERE id=1",
                cancellationToken: mode == "query" ? cancel.Token : default);
            store.Armed = true;
            await using var iterator = mode == "enumerator" ? rows.GetAsyncEnumerator(cancel.Token) : null;
            var read = iterator is null ? Read() : Move();
            try
            {
                await store.Started.Task.WaitAsync(timeout.Token);
                Corpus.Equal(false, read.IsCompleted, mode + " read suspends in actual device I/O");
                cancel.Cancel();
                try { await read.WaitAsync(timeout.Token); throw new InvalidDataException("cancelled device read succeeded"); }
                catch (SqlCanceledException error)
                {
                    Corpus.Equal(SqlStates.QueryCanceled, error.SqlState, mode + " cancellation SQLSTATE");
                    Corpus.Equal(cancel.Token, error.CancellationToken, mode + " originating cancellation token");
                    Corpus.Equal(rows.Cost, error.Cost, mode + " cancellation cost");
                }
                Corpus.Equal(true, read.IsCanceled, mode + " read has canceled task state");
                Corpus.Equal(true, store.ReadToken.IsCancellationRequested, mode + " token reaches suspended device");
                Corpus.Equal(session.LifetimeCost - before, rows.Cost, mode + " work remains in lifetime cost");
            }
            finally { store.Release.TrySetResult(); }
            if (mode == "enumerator")
            {
                await HostTests.Error(SqlStates.InFailedSqlTransaction, async () => { await session.ExecuteAsync("SELECT 1"); });
                await session.ExecuteAsync("ROLLBACK");
            }
            await session.ExecuteAsync("INSERT INTO t VALUES (2, NULL)");
            Corpus.Equal(database.Version, database.OldestLiveVersion, mode + " terminal cancellation releases snapshot pin");
            async Task Read() { _ = await rows.ReadAsync(mode == "read" ? cancel.Token : default); }
            async Task Move() { _ = await iterator!.MoveNextAsync(); }
        }
        await using (var session = new Session())
        {
            using var firstRead = new CancellationTokenSource();
            await using var rows = await session.QueryAsync("SELECT * FROM generate_series(1, 2)");
            Corpus.Equal(1L, (await rows.ReadAsync(firstRead.Token))![0].AsInteger(), "first scoped read");
            firstRead.Cancel();
            Corpus.Equal(2L, (await rows.ReadAsync())![0].AsInteger(), "completed read token does not arm subsequent read");
        }
        Console.WriteLine("PASS cursor cancellation: suspended device I/O, query/read/enumeration tokens, SQLSTATE/cost, transaction failure, pin release and read registration lifetime");
    }

    private sealed class SuspendedStore(byte[] image) : IBlockStore
    {
        private readonly MemoryBlockStore inner = new(image);
        internal bool Armed;
        internal readonly TaskCompletionSource Started = new(TaskCreationOptions.RunContinuationsAsynchronously);
        internal readonly TaskCompletionSource Release = new(TaskCreationOptions.RunContinuationsAsynchronously);
        internal CancellationToken ReadToken;
        public async ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken cancellationToken = default)
        {
            if (Armed)
            {
                Armed = false; ReadToken = cancellationToken; Started.TrySetResult();
                await Release.Task.WaitAsync(cancellationToken);
            }
            await inner.ReadAsync(offset, destination, cancellationToken);
        }
        public ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken cancellationToken = default) => inner.WriteAsync(offset, source, cancellationToken);
        public ValueTask<long> GetLengthAsync(CancellationToken cancellationToken = default) => inner.GetLengthAsync(cancellationToken);
        public ValueTask SetLengthAsync(long length, CancellationToken cancellationToken = default) => inner.SetLengthAsync(length, cancellationToken);
        public ValueTask SyncAsync(CancellationToken cancellationToken = default) => inner.SyncAsync(cancellationToken);
        public ValueTask DisposeAsync() => inner.DisposeAsync();
    }
}
