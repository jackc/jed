using System.Collections.Concurrent;
using System.Diagnostics;
using System.Net.Sockets;

namespace Jed;

// Storage owns writer admission, free-page policy, Check A/B/C, durability,
// poisoning, and publication. A lease's DisposeAsync aborts only before its
// point of no return and always releases its write pin and writer gate.
internal interface IPageOwnerWrite : IAsyncDisposable
{
    PageOwnerLedger.PageOwnerPin Pin { get; }
    bool ReuseGrant { get; }
    PageWireDigest[] Pending { get; }
    uint[] Reuse { get; }
    ValueTask BeginCommitAsync(ulong transactionId, byte[] meta, PageWirePage[] manifest, CancellationToken cancellationToken);
    ValueTask WritePagesAsync(PageWirePage[] pages, CancellationToken cancellationToken);
    ValueTask<ulong> CompleteCommitAsync(CancellationToken cancellationToken);
    ValueTask AfterAcknowledgementAsync() => ValueTask.CompletedTask;
}
internal sealed record PageOwnerOptions(bool ReadOnly = false, bool Yields = false, int MaxConnections = 0,
    long IdleSessionMilliseconds = 0, long IdleTransactionMilliseconds = 60000);
internal sealed record PageOwnerBackend(PageOwnerLedger Ledger,
    Func<uint, CancellationToken, ValueTask<byte[]>> ReadPage,
    Func<bool, CancellationToken, ValueTask<IPageOwnerWrite>> BeginWrite,
    Func<CancellationToken, ValueTask> AwaitAdmission,
    Func<ulong, CancellationToken, ValueTask<byte>> RequestOwnership,
    Action<ulong>? OwnershipAnswered = null,
    Func<byte[], ulong, CancellationToken, ValueTask<PageOwnerLedger.PageOwnerPin>>? AcquireRead = null);

// Only the role driver binds/unlinks endpoints and releases presence. This
// transport can also serve a stream relay without taking file ownership itself.
internal sealed class PageOwnerServer(PageOwnerBackend ownerBackend, PageOwnerOptions ownerOptions) : IAsyncDisposable
{
    private readonly PageOwnerBackend backend = ownerBackend;
    private readonly PageOwnerOptions options = ownerOptions;
    private readonly ConcurrentDictionary<ulong, Connection> connections = new();
    private readonly CancellationTokenSource stopping = new();
    private readonly object identityGate = new();
    private ulong nextConnectionId = 1;
    private Socket? listener;
    private Task? accepting;
    private int disposed;
    internal Action? BeforeAcknowledgementForTesting;
    internal int ConnectionCount => connections.Count;
    internal bool HasConnection(ulong id) => connections.TryGetValue(id, out var connection) && connection.IsOpen;
    internal void Abort()
    {
        stopping.Cancel(); listener?.Dispose();
        foreach (var connection in connections.Values) connection.Close();
    }
    internal void Listen(string endpoint)
    {
        ObjectDisposedException.ThrowIf(disposed != 0, this);
        if (listener is not null) throw new InvalidOperationException("page server already listening");
        listener = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
        try { listener.Bind(new UnixDomainSocketEndPoint(endpoint)); listener.Listen(128); accepting = AcceptAsync(); }
        catch { listener.Dispose(); listener = null; throw; }
    }
    private async Task AcceptAsync()
    {
        try
        {
            while (!stopping.IsCancellationRequested)
            {
                var socket = await listener!.AcceptAsync(stopping.Token).ConfigureAwait(false);
                _ = ServeStreamAsync(new NetworkStream(socket, ownsSocket: true));
            }
        }
        catch (OperationCanceledException) when (stopping.IsCancellationRequested) { }
        catch (ObjectDisposedException) when (stopping.IsCancellationRequested) { }
        catch (SocketException) when (stopping.IsCancellationRequested) { }
    }
    internal async Task ServeStreamAsync(Stream stream)
    {
        ulong id;
        lock (identityGate)
        {
            if (nextConnectionId == 0) { stream.Dispose(); return; }
            id = nextConnectionId; nextConnectionId = id == ulong.MaxValue ? 0 : id + 1;
        }
        var connection = new Connection(this, id, stream);
        connections.TryAdd(id, connection);
        try { await connection.RunAsync().ConfigureAwait(false); }
        finally
        {
            // Keep the connection discoverable until its writer and reader pins
            // are released; server shutdown must join this cleanup as well.
            try { await connection.DisposeAsync().ConfigureAwait(false); }
            finally { connections.TryRemove(id, out _); }
        }
    }
    internal async ValueTask BroadcastNoticeAsync(byte kind, ulong requester = 0, CancellationToken cancellationToken = default)
    {
        foreach (var connection in connections.Values)
            await connection.NoticeAsync(kind, requester, cancellationToken).ConfigureAwait(false);
    }
    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref disposed, 1) != 0) return;
        Abort();
        if (accepting is not null) await accepting.ConfigureAwait(false);
        // Closing streams first interrupts readers. A commit already inside
        // CompleteCommitAsync ignores disconnect cancellation and still finishes.
        var current = connections.Values.ToArray();
        foreach (var connection in current) connection.Close();
        foreach (var connection in current) await connection.Finished.Task.ConfigureAwait(false);
        stopping.Dispose();
    }

    private sealed class Connection(PageOwnerServer server, ulong id, Stream stream) : IAsyncDisposable
    {
        private ulong Id => id;
        private readonly SemaphoreSlim sends = new(1);
        private readonly SemaphoreSlim writeState = new(1);
        private readonly CancellationTokenSource stopped = CancellationTokenSource.CreateLinkedTokenSource(server.stopping.Token);
        private readonly Dictionary<ulong, Stack<PageOwnerLedger.PageOwnerPin>> pins = [];
        private readonly List<Task> deferred = [];
        private IPageOwnerWrite? writer;
        private bool awaitingWriter;
        private bool readOnly;
        private bool handshaken;
        private bool commitStarted;
        private bool revoked;
        private ulong revokedVersion;
        private bool completing;
        private SqlException? rejectedDelta;
        private long lastFrame = Stopwatch.GetTimestamp();
        private Task? timer;
        private int closed;
        internal bool IsOpen => Volatile.Read(ref closed) == 0;
        internal TaskCompletionSource Finished { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        private PageOwnerBackend Backend => server.backend;
        private PageOwnerOptions Options => server.options;
        internal async Task RunAsync()
        {
            try
            {
                var hello = await PageWire.ReadAsync(stream, cancellationToken: stopped.Token).ConfigureAwait(false);
                if (hello is null) return;
                if (hello.Code != 1) { await ErrorAsync(hello.RequestId, PageWire.Invalid("first request must be hello"), true).ConfigureAwait(false); return; }
                if (hello.Get<PageWireCredential>("credential").Kind != 0)
                { await ErrorAsync(hello.RequestId, PageWire.Invalid("owner endpoint accepts no credential"), true).ConfigureAwait(false); return; }
                if (hello.Get<ushort>("protocol_min") > 1 || hello.Get<ushort>("protocol_max") < 1 || hello.Get<ushort>("format_version") != StorageFormat.Version)
                { await ErrorAsync(hello.RequestId, new(SqlStates.FeatureNotSupported, $"page service requires protocol 1 and format {StorageFormat.Version}"), true).ConfigureAwait(false); return; }
                // Admission and counting are atomic across concurrent hellos.
                bool refused;
                lock (server.identityGate)
                {
                    refused = Options.MaxConnections > 0 && server.connections.Values.Count(c => c.handshaken) >= Options.MaxConnections;
                    if (!refused) handshaken = true;
                }
                if (refused) { await ErrorAsync(hello.RequestId, new(SqlStates.TooManyConnections, "page-service connection limit reached"), true).ConfigureAwait(false); return; }
                readOnly = (hello.Get<byte>("flags") & 1) != 0;
                await ReplyAsync(0x41, hello.RequestId, ("protocol", (ushort)1), ("format_version", StorageFormat.Version),
                    ("page_size", (uint)Backend.Ledger.PageSize), ("connection_id", id),
                    ("flags", (byte)((readOnly ? 1 : 0) | (Options.ReadOnly ? 2 : 0) | (Options.Yields ? 4 : 0))),
                    ("max_read_pages", (uint)PageWire.MaxReadPages(Backend.Ledger.PageSize))).ConfigureAwait(false);
                timer = CheckIdleAsync();
                while (!stopped.IsCancellationRequested)
                {
                    var request = await PageWire.ReadAsync(stream, Backend.Ledger.PageSize, stopped.Token).ConfigureAwait(false);
                    if (request is null) break;
                    Volatile.Write(ref lastFrame, Stopwatch.GetTimestamp());
                    if (request.Code >= 0x40 || request.Code == 1) throw PageWire.Invalid("unexpected client message");
                    try { await DispatchAsync(request).ConfigureAwait(false); }
                    catch (SqlException error)
                    {
                        var fatal = error.SqlState is SqlStates.ProtocolViolation or SqlStates.IoError;
                        await ErrorAsync(request.RequestId, error, fatal).ConfigureAwait(false);
                        if (fatal) break;
                    }
                    finally { Volatile.Write(ref lastFrame, Stopwatch.GetTimestamp()); }
                }
            }
            catch (SqlException error)
            {
                try { await ErrorAsync(0, error, true).ConfigureAwait(false); } catch (Exception sendError) when (TransportError(sendError)) { }
            }
            catch (Exception error) when (TransportError(error)) { }
            finally { Close(); }
        }
        private async ValueTask DispatchAsync(PageWireMessage request)
        {
            if (commitStarted && request.Code is not (7 or 8)) throw PageWire.Invalid("request interrupts commit delta");
            switch (request.Code)
            {
                case 2:
                    // Admission can wait during a drain without stopping existing
                    // transactions from sending reads/end_read/commit on this stream.
                    deferred.Add(BeginReadAsync(request)); break;
                case 3: await ReadPagesAsync(request).ConfigureAwait(false); break;
                case 4:
                    lock (pins)
                    {
                        var version = request.Get<ulong>("txid");
                        if (!pins.TryGetValue(version, out var stack) || stack.Count == 0) throw PageWire.Invalid("end_read names no pin");
                        stack.Pop().Dispose(); if (stack.Count == 0) pins.Remove(version);
                    }
                    break;
                case 5:
                    await writeState.WaitAsync(stopped.Token).ConfigureAwait(false);
                    try
                    {
                        if (revoked) { revoked = false; throw new SqlException(SqlStates.IdleInTransactionSessionTimeout, "page-service write transaction expired"); }
                        if (writer is not null || awaitingWriter) throw PageWire.Invalid("write transaction already active");
                        if (readOnly || Options.ReadOnly) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "page-service connection is read-only");
                        awaitingWriter = true; deferred.Add(BeginWriteAsync(request));
                    }
                    finally { writeState.Release(); }
                    break;
                case 6: case 7: case 8: case 9: await WriteRequestAsync(request).ConfigureAwait(false); break;
                case 0x0a:
                    var status = await Backend.Ledger.CommitStatusAsync(request.Get<ulong>("txid"), request.Get<ulong>("digest"), stopped.Token).ConfigureAwait(false);
                    await ReplyAsync(0x4a, request.RequestId, ("status", status)).ConfigureAwait(false); break;
                case 0x0b:
                    var ownership = await Backend.RequestOwnership(id, stopped.Token).ConfigureAwait(false);
                    try { await ReplyAsync(0x4b, request.RequestId, ("status", ownership)).ConfigureAwait(false); }
                    finally { if (ownership == 0) Backend.OwnershipAnswered?.Invoke(id); }
                    break;
                default: throw PageWire.Invalid("unknown client request");
            }
            deferred.RemoveAll(task => task.IsCompletedSuccessfully);
        }
        private async Task BeginReadAsync(PageWireMessage request)
        {
            try
            {
                await Backend.AwaitAdmission(stopped.Token).ConfigureAwait(false);
                stopped.Token.ThrowIfCancellationRequested();
                var pin = Backend.AcquireRead is { } acquire
                    ? await acquire(request.Get<byte[]>("incarnation"), request.Get<ulong>("last_txid"), stopped.Token).ConfigureAwait(false)
                    : Backend.Ledger.Pin(request.Get<byte[]>("incarnation"), request.Get<ulong>("last_txid"));
                lock (pins)
                {
                    if (!pins.TryGetValue(pin.Snapshot.TransactionId, out var stack)) pins.Add(pin.Snapshot.TransactionId, stack = new());
                    stack.Push(pin);
                }
                await SnapshotReplyAsync(0x42, request.RequestId, pin.Snapshot).ConfigureAwait(false);
            }
            catch (Exception error) { await DeferredErrorAsync(request.RequestId, error).ConfigureAwait(false); }
        }
        private async ValueTask ReadPagesAsync(PageWireMessage request)
        {
            var version = request.Get<ulong>("txid"); uint count = 0; var found = false;
            lock (pins) if (pins.TryGetValue(version, out var stack) && stack.Count > 0) { count = stack.Peek().PageCount; found = true; }
            await writeState.WaitAsync(stopped.Token).ConfigureAwait(false);
            try
            {
                if (!found && writer?.Pin.Snapshot.TransactionId == version) { count = writer.Pin.PageCount; found = true; }
                if (!found && revoked && version == revokedVersion) { revoked = false; throw new SqlException(SqlStates.IdleInTransactionSessionTimeout, "page-service write transaction expired"); }
                if (!found) throw PageWire.Invalid("read_pages names no pin");
                var indices = request.Get<uint[]>("indices");
                if (indices.Length > PageWire.MaxReadPages(Backend.Ledger.PageSize) || indices.Any(index => index < 2 || index >= count)) throw PageWire.Invalid("read_pages index outside pinned file");
                var pages = new byte[indices.Length][];
                for (var i = 0; i < pages.Length; i++) pages[i] = await Backend.ReadPage(indices[i], stopped.Token).ConfigureAwait(false);
                await ReplyAsync(0x43, request.RequestId, ("pages", pages)).ConfigureAwait(false);
            }
            finally { writeState.Release(); }
        }
        private async Task BeginWriteAsync(PageWireMessage request)
        {
            using var deadline = CancellationTokenSource.CreateLinkedTokenSource(stopped.Token);
            using var deadlineStop = new CancellationTokenSource();
            var timeout = request.Get<ulong>("lock_timeout_ms");
            var deadlineTask = timeout == 0 ? Task.CompletedTask : CancelAfterAsync(deadline, timeout, deadlineStop.Token);
            IPageOwnerWrite? acquired = null;
            try
            {
                await Backend.AwaitAdmission(deadline.Token).ConfigureAwait(false);
                acquired = await Backend.BeginWrite((request.Get<byte>("flags") & 1) != 0, deadline.Token).ConfigureAwait(false);
                await writeState.WaitAsync(stopped.Token).ConfigureAwait(false);
                try
                {
                    stopped.Token.ThrowIfCancellationRequested(); writer = acquired; acquired = null; awaitingWriter = false;
                    // The adapter's pin may be computed without this client's
                    // cache, so get invalidations from a temporary equal-version
                    // ledger pin while retaining the writer's original pin.
                    using var cache = Backend.Ledger.Pin(request.Get<byte[]>("incarnation"), request.Get<ulong>("last_txid"));
                    if (cache.Snapshot.TransactionId != writer.Pin.Snapshot.TransactionId) throw new InvalidOperationException("writer gate did not freeze publication");
                    await SnapshotReplyAsync(0x45, request.RequestId, cache.Snapshot, ("reuse_grant", writer.ReuseGrant),
                        ("pending", writer.Pending), ("reuse", writer.Reuse)).ConfigureAwait(false);
                    Volatile.Write(ref lastFrame, Stopwatch.GetTimestamp());
                }
                finally { writeState.Release(); }
            }
            catch (OperationCanceledException) when (!stopped.IsCancellationRequested)
            {
                awaitingWriter = false;
                await DeferredErrorAsync(request.RequestId, new SqlException(SqlStates.LockNotAvailable, "page-service writer lock timeout")).ConfigureAwait(false);
            }
            catch (Exception error) { awaitingWriter = false; await DeferredErrorAsync(request.RequestId, error).ConfigureAwait(false); }
            finally
            {
                deadlineStop.Cancel(); await deadlineTask.ConfigureAwait(false);
                if (acquired is not null) await acquired.DisposeAsync().ConfigureAwait(false);
            }
        }
        private async ValueTask WriteRequestAsync(PageWireMessage request)
        {
            await writeState.WaitAsync(stopped.Token).ConfigureAwait(false);
            try
            {
                if (revoked)
                {
                    if (request.Code is 6 or 7) return;
                    revoked = false;
                    if (request.Code == 9) return;
                    throw new SqlException(SqlStates.IdleInTransactionSessionTimeout, "page-service write transaction expired");
                }
                if (writer is null) throw PageWire.Invalid("write request without writer gate");
                if (request.Code == 9) { await ReleaseWriteAsync().ConfigureAwait(false); return; }
                if (request.Code == 6)
                {
                    if (commitStarted) throw PageWire.Invalid("duplicate commit_begin");
                    commitStarted = true;
                    try { await writer.BeginCommitAsync(request.Get<ulong>("txid"), request.Get<byte[]>("meta"), request.Get<PageWirePage[]>("manifest_pages"), stopped.Token).ConfigureAwait(false); }
                    catch (SqlException error) when (error.SqlState == SqlStates.ProtocolViolation) { rejectedDelta = error; }
                }
                else if (request.Code == 7)
                {
                    if (!commitStarted) throw PageWire.Invalid("commit_pages before commit_begin");
                    if (rejectedDelta is null)
                        try { await writer.WritePagesAsync(request.Get<PageWirePage[]>("pages"), stopped.Token).ConfigureAwait(false); }
                        catch (SqlException error) when (error.SqlState == SqlStates.ProtocolViolation) { rejectedDelta = error; }
                }
                else
                {
                    if (!commitStarted) throw PageWire.Invalid("commit_end before commit_begin");
                    if (rejectedDelta is not null) { var failure = rejectedDelta; await ReleaseWriteAsync().ConfigureAwait(false); throw failure; }
                    completing = true;
                    var committed = false;
                    try
                    {
                        // A disconnect must not cancel storage after Check C.
                        var version = await writer.CompleteCommitAsync(CancellationToken.None).ConfigureAwait(false);
                        committed = true;
                        Interlocked.Exchange(ref server.BeforeAcknowledgementForTesting, null)?.Invoke();
                        await ReplyAsync(0x48, request.RequestId, ("txid", version)).ConfigureAwait(false);
                    }
                    catch (SqlException error) when (error.SqlState == SqlStates.ProtocolViolation) { throw; }
                    catch { Close(); throw; }
                    finally
                    {
                        try
                        {
                            // The committed client generation is already acknowledged
                            // (or its reply was lost). Keep the writer gate through
                            // owner-only reclamation; invalid/failed deltas never run it.
                            if (committed) await writer.AfterAcknowledgementAsync().ConfigureAwait(false);
                        }
                        catch { Close(); throw; }
                        finally { completing = false; await ReleaseWriteAsync().ConfigureAwait(false); }
                    }
                }
            }
            finally { writeState.Release(); }
        }
        private async ValueTask ReleaseWriteAsync()
        {
            var old = writer; writer = null; commitStarted = false; rejectedDelta = null;
            if (old is not null) await old.DisposeAsync().ConfigureAwait(false);
        }
        private async Task CheckIdleAsync()
        {
            try
            {
                while (!stopped.IsCancellationRequested)
                {
                    await Task.Delay(100, stopped.Token).ConfigureAwait(false);
                    var elapsed = Stopwatch.GetElapsedTime(Volatile.Read(ref lastFrame)).TotalMilliseconds;
                    if (!await writeState.WaitAsync(0, stopped.Token).ConfigureAwait(false)) continue;
                    try
                    {
                        if (writer is not null && !completing && Options.IdleTransactionMilliseconds > 0 && elapsed >= Options.IdleTransactionMilliseconds)
                        { revokedVersion = writer.Pin.Snapshot.TransactionId; await ReleaseWriteAsync().ConfigureAwait(false); revoked = true; }
                        bool hasPins; lock (pins) hasPins = pins.Count > 0;
                        if (!hasPins && writer is null && !awaitingWriter && Options.IdleSessionMilliseconds > 0 && elapsed >= Options.IdleSessionMilliseconds) Close();
                    }
                    finally { writeState.Release(); }
                }
            }
            catch (OperationCanceledException) when (stopped.IsCancellationRequested) { }
        }
        private static async Task CancelAfterAsync(CancellationTokenSource deadline, ulong milliseconds, CancellationToken stop)
        {
            try
            {
                while (milliseconds > 0)
                {
                    var chunk = (int)Math.Min(milliseconds, 3600000ul); await Task.Delay(chunk, stop).ConfigureAwait(false); milliseconds -= (ulong)chunk;
                }
                deadline.Cancel();
            }
            catch (OperationCanceledException) when (stop.IsCancellationRequested) { }
        }
        private async ValueTask SnapshotReplyAsync(byte code, uint requestId, PageReadSnapshot snapshot, params (string Name, object Value)[] extra)
        {
            var fields = new List<(string, object)> { ("incarnation", snapshot.Incarnation), ("txid", snapshot.TransactionId),
                ("invalidate", (byte)(snapshot.InvalidateAll ? 1 : 0)), ("indices", snapshot.Invalidated), ("meta", snapshot.Meta) };
            fields.AddRange(extra); await ReplyAsync(code, requestId, fields.ToArray()).ConfigureAwait(false);
        }
        private async ValueTask DeferredErrorAsync(uint requestId, Exception error)
        {
            if (stopped.IsCancellationRequested) return;
            try
            {
                var sql = error as SqlException ?? new SqlException(SqlStates.IoError, error.Message);
                var fatal = sql.SqlState is SqlStates.ProtocolViolation or SqlStates.IoError;
                await ErrorAsync(requestId, sql, fatal).ConfigureAwait(false); if (fatal) Close();
            }
            catch (Exception sendError) when (TransportError(sendError)) { Close(); }
        }
        internal async ValueTask NoticeAsync(byte kind, ulong requester, CancellationToken cancellationToken)
        {
            if (!handshaken || stopped.IsCancellationRequested) return;
            try { await SendAsync(PageClientConnection.Message(0x7e, 0, ("kind", kind), ("connection_id", requester)), cancellationToken).ConfigureAwait(false); }
            catch (Exception error) when (TransportError(error)) { Close(); }
        }
        private ValueTask ReplyAsync(byte code, uint requestId, params (string Name, object Value)[] fields) =>
            SendAsync(PageClientConnection.Message(code, requestId, fields), stopped.Token);
        private ValueTask ErrorAsync(uint requestId, SqlException error, bool fatal) => ReplyAsync(0x7f, requestId,
            ("sqlstate", error.SqlState), ("message", error.Message), ("detail", ""), ("flags", (byte)(fatal ? 1 : 0)));
        private async ValueTask SendAsync(PageWireMessage message, CancellationToken cancellationToken)
        {
            await sends.WaitAsync(cancellationToken).ConfigureAwait(false);
            try { await PageWire.WriteAsync(stream, message, Backend.Ledger.PageSize, cancellationToken).ConfigureAwait(false); }
            finally { sends.Release(); }
        }
        internal void Close()
        {
            if (Interlocked.Exchange(ref closed, 1) != 0) return;
            stopped.Cancel(); stream.Dispose();
        }
        public async ValueTask DisposeAsync()
        {
            Close();
            try
            {
                if (timer is not null) await timer.ConfigureAwait(false);
                await Task.WhenAll(deferred).ConfigureAwait(false);
                await writeState.WaitAsync().ConfigureAwait(false);
                try { await ReleaseWriteAsync().ConfigureAwait(false); } finally { writeState.Release(); }
                lock (pins) { foreach (var stack in pins.Values) foreach (var pin in stack) pin.Dispose(); pins.Clear(); }
                await stream.DisposeAsync().ConfigureAwait(false);
            }
            finally { Finished.TrySetResult(); stopped.Dispose(); }
        }
        private static bool TransportError(Exception error) => error is IOException or SocketException or OperationCanceledException or ObjectDisposedException;
    }
}
