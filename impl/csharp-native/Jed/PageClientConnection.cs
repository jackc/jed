using System.Collections.Concurrent;
using System.Net.Sockets;

namespace Jed;

internal sealed record PageReadSnapshot(byte[] Incarnation, ulong TransactionId, bool InvalidateAll, uint[] Invalidated, byte[] Meta);
internal sealed record PageWriteSnapshot(PageReadSnapshot Snapshot, bool ReuseGrant, PageWireDigest[] Pending, uint[] Reuse);

// One asynchronous reader matches replies by request id, so an owner waiting for
// its writer gate can continue answering this connection's independent reads.
// Transaction/pin lifetimes and role re-election belong to the database driver.
internal sealed class PageClientConnection : IAsyncDisposable
{
    private sealed class Pending(byte expectedCode)
    {
        internal byte ExpectedCode { get; } = expectedCode;
        internal TaskCompletionSource<PageWireMessage> Completion { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        internal int Released;
    }
    private void ReleaseSlot(Pending request)
    { if (Interlocked.Exchange(ref request.Released, 1) == 0) requestSlots.Release(); }
    private readonly Stream stream;
    private readonly SemaphoreSlim sendGate = new(1);
    private readonly SemaphoreSlim requestSlots = new(1024);
    private readonly ConcurrentDictionary<uint, Pending> pending = new();
    private readonly CancellationTokenSource stopping = new();
    private readonly Task reader;
    private uint nextRequestId = 2;
    private Exception? failed;
    private int disposed;
    internal bool IsConnected => Volatile.Read(ref failed) is null && Volatile.Read(ref disposed) == 0;
    internal int PageSize { get; }
    internal int MaxReadPages { get; }
    internal ulong ConnectionId { get; }
    internal bool ReadOnly { get; }
    internal bool OwnerReadOnly { get; }
    internal bool OwnerYields { get; }
    internal event Action<byte, ulong>? Notice;

    private PageClientConnection(Stream stream, PageWireMessage hello)
    {
        this.stream = stream;
        PageSize = (int)hello.Get<uint>("page_size");
        MaxReadPages = (int)hello.Get<uint>("max_read_pages");
        ConnectionId = hello.Get<ulong>("connection_id");
        var flags = hello.Get<byte>("flags"); ReadOnly = (flags & 1) != 0; OwnerReadOnly = (flags & 2) != 0; OwnerYields = (flags & 4) != 0;
        reader = ReadRepliesAsync();
    }
    internal static async ValueTask<PageClientConnection> ConnectLocalAsync(string endpoint, bool readOnly = false, CancellationToken cancellationToken = default)
    {
        var socket = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
        try
        {
            await socket.ConnectAsync(new UnixDomainSocketEndPoint(endpoint), cancellationToken).ConfigureAwait(false);
            return await ConnectStreamAsync(new NetworkStream(socket, ownsSocket: true), readOnly, new(0, []), cancellationToken).ConfigureAwait(false);
        }
        catch { socket.Dispose(); throw; }
    }
    internal static async ValueTask<PageClientConnection> ConnectStreamAsync(Stream stream, bool readOnly, PageWireCredential credential, CancellationToken cancellationToken = default)
    {
        try
        {
            var hello = Message(1, 1, ("protocol_min", PageWire.ProtocolVersion), ("protocol_max", PageWire.ProtocolVersion),
                ("format_version", StorageFormat.Version), ("flags", (byte)(readOnly ? 1 : 0)), ("client_name", "jed-csharp"), ("credential", credential));
            await PageWire.WriteAsync(stream, hello, cancellationToken: cancellationToken).ConfigureAwait(false);
            var reply = await PageWire.ReadAsync(stream, cancellationToken: cancellationToken).ConfigureAwait(false) ?? throw Lost();
            if (reply.RequestId != 1) throw PageWire.Invalid("hello reply id differs");
            if (reply.Code == 0x7f) throw RemoteError(reply);
            if (reply.Code != 0x41 || reply.Get<ushort>("protocol") != PageWire.ProtocolVersion || reply.Get<ushort>("format_version") != StorageFormat.Version)
                throw PageWire.Invalid("invalid hello reply");
            var pageSize = reply.Get<uint>("page_size"); var maximum = reply.Get<uint>("max_read_pages");
            if (pageSize > int.MaxValue || !StorageFormat.ValidPageSize((int)pageSize) || reply.Get<ulong>("connection_id") == 0 ||
                maximum == 0 || maximum > PageWire.MaxReadPages((int)pageSize) || ((reply.Get<byte>("flags") & 1) != 0) != readOnly)
                throw PageWire.Invalid("invalid negotiated page-service limits");
            return new(stream, reply);
        }
        catch { await stream.DisposeAsync().ConfigureAwait(false); throw; }
    }
    internal static PageWireMessage Message(byte code, uint requestId, params (string Name, object Value)[] fields) =>
        new(code, requestId, fields.ToDictionary(f => f.Name, f => f.Value, StringComparer.Ordinal));

    internal async ValueTask<PageReadSnapshot> BeginReadAsync(byte[] incarnation, ulong lastTransactionId, CancellationToken cancellationToken = default)
    {
        var reply = await RequestAsync(2, 0x42, [("incarnation", incarnation), ("last_txid", lastTransactionId)], cancellationToken).ConfigureAwait(false);
        return Snapshot(reply);
    }
    internal async ValueTask<byte[][]> ReadPagesAsync(ulong transactionId, uint[] indices, CancellationToken cancellationToken = default)
    {
        if (indices.Length > MaxReadPages) throw new ArgumentOutOfRangeException(nameof(indices));
        var reply = await RequestAsync(3, 0x43, [("txid", transactionId), ("indices", indices)], cancellationToken).ConfigureAwait(false);
        var pages = reply.Get<byte[][]>("pages");
        if (pages.Length != indices.Length) { Fail(PageWire.Invalid("read reply count differs")); ThrowFailed(); }
        return pages;
    }
    internal ValueTask EndReadAsync(ulong transactionId, CancellationToken cancellationToken = default) =>
        SendAsync(4, [("txid", transactionId)], cancellationToken);
    internal async ValueTask<PageWriteSnapshot> BeginWriteAsync(byte[] incarnation, ulong lastTransactionId, ulong lockTimeoutMilliseconds,
        bool relaxed = false, CancellationToken cancellationToken = default)
    {
        var reply = await RequestAsync(5, 0x45, [("incarnation", incarnation), ("last_txid", lastTransactionId),
            ("lock_timeout_ms", lockTimeoutMilliseconds), ("flags", (byte)(relaxed ? 1 : 0))], cancellationToken).ConfigureAwait(false);
        return new(Snapshot(reply), reply.Get<bool>("reuse_grant"), reply.Get<PageWireDigest[]>("pending"), reply.Get<uint[]>("reuse"));
    }
    internal ValueTask AbortAsync(CancellationToken cancellationToken = default) => SendAsync(9, [], cancellationToken);
    internal async ValueTask<byte> CommitStatusAsync(ulong transactionId, ulong digest, CancellationToken cancellationToken = default)
    {
        var reply = await RequestAsync(0x0a, 0x4a, [("txid", transactionId), ("digest", digest)], cancellationToken).ConfigureAwait(false);
        var status = reply.Get<byte>("status");
        if (status > 2) { Fail(PageWire.Invalid("invalid commit status")); ThrowFailed(); }
        return status;
    }
    internal async ValueTask<byte> RequestOwnershipAsync(CancellationToken cancellationToken = default)
    {
        var reply = await RequestAsync(0x0b, 0x4b, [], cancellationToken).ConfigureAwait(false);
        var status = reply.Get<byte>("status");
        if (status > 1) { Fail(PageWire.Invalid("invalid ownership status")); ThrowFailed(); }
        return status;
    }

    // Hold the send gate for the entire delta: no independent read request can
    // enter between commit_begin and commit_end. Caller resolves a lost reply by
    // reconnecting and asking commit_status; we never retry a commit blindly.
    internal async ValueTask<ulong> CommitAsync(StorageCommitPlan plan, CancellationToken cancellationToken = default, Action? commitEndWritten = null)
    {
        if (plan.Meta.PageSize != PageSize) throw new ArgumentException("wrong commit page size");
        var manifest = plan.Manifest.Select(p => new PageWirePage(p.Page, p.Bytes)).ToArray();
        var begin = Message(6, 1, ("txid", plan.Meta.TransactionId), ("meta", plan.MetaBytes), ("manifest_pages", manifest));
        // Preflight the single begin frame before acquiring or sending anything.
        byte[] beginBytes;
        try { beginBytes = PageWire.Encode(begin, PageSize); }
        catch (SqlException error) when (error.SqlState == SqlStates.ProtocolViolation) { throw new SqlException(SqlStates.ProgramLimitExceeded, "commit manifest does not fit one page-service frame"); }
        _ = beginBytes;
        await requestSlots.WaitAsync(cancellationToken).ConfigureAwait(false);
        var request = new Pending(0x48);
        var acquired = false;
        try
        {
            await sendGate.WaitAsync(cancellationToken).ConfigureAwait(false); acquired = true; ThrowFailed();
            await WriteLockedAsync(6, begin.Fields, cancellationToken).ConfigureAwait(false);
            var batchSize = (PageWire.MaximumFrameLength - 9) / (PageSize + 4);
            for (var from = 0; from < plan.Body.Length; from += batchSize)
            {
                var batch = plan.Body.Skip(from).Take(batchSize).Select(p => new PageWirePage(p.Page, p.Bytes)).ToArray();
                await WriteLockedAsync(7, new Dictionary<string, object> { ["pages"] = batch }, cancellationToken).ConfigureAwait(false);
            }
            await RegisterAndWriteAsync(8, request, new Dictionary<string, object>(), cancellationToken).ConfigureAwait(false);
            commitEndWritten?.Invoke();
        }
        catch (Exception error) { if (acquired) Fail(error); ReleaseSlot(request); throw; }
        finally { if (acquired) sendGate.Release(); }
        var reply = await AwaitReplyAsync(request, cancellationToken).ConfigureAwait(false);
        var transactionId = reply.Get<ulong>("txid");
        if (transactionId != plan.Meta.TransactionId) { Fail(PageWire.Invalid("commit acknowledgement differs")); ThrowFailed(); }
        return transactionId;
    }

    private static PageReadSnapshot Snapshot(PageWireMessage reply)
    {
        var invalidate = reply.Get<byte>("invalidate"); var indices = reply.Get<uint[]>("indices");
        if (invalidate > 1 || indices.Length > 65536 || (invalidate == 1 && indices.Length != 0)) throw PageWire.Invalid("invalid cache invalidation");
        for (var i = 1; i < indices.Length; i++) if (indices[i] <= indices[i - 1]) throw PageWire.Invalid("unsorted invalidation");
        return new(reply.Get<byte[]>("incarnation"), reply.Get<ulong>("txid"), invalidate != 0, indices, reply.Get<byte[]>("meta"));
    }
    private async ValueTask<PageWireMessage> RequestAsync(byte code, byte expectedCode, (string Name, object Value)[] fields, CancellationToken cancellationToken)
    {
        await requestSlots.WaitAsync(cancellationToken).ConfigureAwait(false);
        var request = new Pending(expectedCode); var acquired = false;
        try
        {
            await sendGate.WaitAsync(cancellationToken).ConfigureAwait(false); acquired = true; ThrowFailed();
            await RegisterAndWriteAsync(code, request, Message(code, 1, fields).Fields, cancellationToken).ConfigureAwait(false);
        }
        catch (Exception error) { if (acquired) Fail(error); ReleaseSlot(request); throw; }
        finally { if (acquired) sendGate.Release(); }
        return await AwaitReplyAsync(request, cancellationToken).ConfigureAwait(false);
    }
    private async ValueTask<PageWireMessage> AwaitReplyAsync(Pending request, CancellationToken cancellationToken)
    {
        try { return await request.Completion.Task.WaitAsync(cancellationToken).ConfigureAwait(false); }
        catch (OperationCanceledException)
        {
            // A canceled begin may still have created a remote pin or acquired
            // the writer gate. Disconnecting makes the owner release both.
            Fail(Lost()); throw;
        }
    }
    private async ValueTask SendAsync(byte code, (string Name, object Value)[] fields, CancellationToken cancellationToken)
    {
        await sendGate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try { ThrowFailed(); await WriteLockedAsync(code, Message(code, 1, fields).Fields, cancellationToken).ConfigureAwait(false); }
        catch (Exception error) { Fail(error); throw; }
        finally { sendGate.Release(); }
    }
    private uint AllocateId()
    {
        var id = nextRequestId; nextRequestId = id == uint.MaxValue ? 1 : id + 1;
        if (pending.ContainsKey(id)) throw PageWire.Invalid("request-id space exhausted");
        return id;
    }
    private async ValueTask WriteLockedAsync(byte code, IReadOnlyDictionary<string, object> fields, CancellationToken cancellationToken)
    { await PageWire.WriteAsync(stream, new(code, AllocateId(), fields), PageSize, cancellationToken).ConfigureAwait(false); }
    private async ValueTask RegisterAndWriteAsync(byte code, Pending request, IReadOnlyDictionary<string, object> fields, CancellationToken cancellationToken)
    {
        var id = AllocateId();
        if (!pending.TryAdd(id, request)) throw new InvalidOperationException("duplicate request id");
        try { await PageWire.WriteAsync(stream, new(code, id, fields), PageSize, cancellationToken).ConfigureAwait(false); }
        catch
        {
            // The caller still owns the slot if registration fails to return.
            pending.TryRemove(id, out _); throw;
        }
    }
    private async Task ReadRepliesAsync()
    {
        try
        {
            while (true)
            {
                var reply = await PageWire.ReadAsync(stream, PageSize, stopping.Token).ConfigureAwait(false) ?? throw Lost();
                if (reply.Code == 0x7e)
                {
                    Notice?.Invoke(reply.Get<byte>("kind"), reply.Get<ulong>("connection_id")); continue;
                }
                if (!pending.TryRemove(reply.RequestId, out var request)) throw PageWire.Invalid("reply names no outstanding request");
                ReleaseSlot(request);
                if (reply.Code == 0x7f)
                {
                    var error = RemoteError(reply); request.Completion.TrySetException(error);
                    if ((reply.Get<byte>("flags") & 1) != 0) throw error;
                }
                else if (reply.Code != request.ExpectedCode)
                {
                    var error = PageWire.Invalid("unexpected reply code"); request.Completion.TrySetException(error); throw error;
                }
                else request.Completion.TrySetResult(reply);
            }
        }
        catch (Exception error) { Fail(error); }
    }
    private static SqlException RemoteError(PageWireMessage reply) => new(reply.Get<string>("sqlstate"), reply.Get<string>("message") +
        (reply.Get<string>("detail").Length == 0 ? "" : ": " + reply.Get<string>("detail")));
    private static SqlException Lost() => new(SqlStates.ConnectionFailure, "page-service connection lost");
    private void ThrowFailed() { if (Volatile.Read(ref failed) is { } error) throw error; }
    private void Fail(Exception error)
    {
        var failure = error is SqlException ? error : new SqlException(SqlStates.ConnectionFailure, "page-service connection lost: " + error.Message);
        if (Interlocked.CompareExchange(ref failed, failure, null) is not null) return;
        stopping.Cancel(); stream.Dispose();
        foreach (var pair in pending)
            if (pending.TryRemove(pair.Key, out var request)) { ReleaseSlot(request); request.Completion.TrySetException(failure); }
    }
    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref disposed, 1) != 0) return;
        Fail(Lost()); await reader.ConfigureAwait(false);
        await stream.DisposeAsync().ConfigureAwait(false);
        // Callers may still be unwinding canceled waits; semaphores remain valid
        // until those references are collected instead of racing Dispose().
        stopping.Dispose();
    }
}
