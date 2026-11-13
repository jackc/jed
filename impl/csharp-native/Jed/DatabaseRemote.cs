using System.Diagnostics;
using System.Net.Sockets;
using System.Security.Cryptography;

namespace Jed;

public sealed partial class Database
{
    /// <summary>Open a page client through a host-provided duplex stream. The factory is called again after transport loss.</summary>
    public static async Task<Database> OpenRemoteAsync(Func<CancellationToken, ValueTask<Stream>> connect,
        DatabaseFileOptions? options = null, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(connect); options ??= new();
        if (options.CacheBytes < 0) throw new ArgumentOutOfRangeException(nameof(options));
        if (options.Locking != FileLocking.Auto) throw new ArgumentException("remote clients require automatic locking", nameof(options));
        var credential = options.Credential?.Value.WireCopy() ?? new PageWireCredential(0, []);
        PageClientConnection? client = null; StoragePageCodec? codec = null; Database? database = null;
        async ValueTask<PageClientConnection> Connect(CancellationToken token) =>
            await PageClientConnection.ConnectStreamAsync(await connect(token).ConfigureAwait(false), options.ReadOnly, credential, token).ConfigureAwait(false);
        try
        {
            client = await Connect(cancellationToken).ConfigureAwait(false);
            var read = await client.BeginReadAsync(new byte[16], 0, cancellationToken).ConfigureAwait(false);
            try
            {
                var store = new PageClientStore(client, read);
                var served = await UnlockClientAsync(store, read.Meta, options, null, false, cancellationToken).ConfigureAwait(false); codec = served.Codec;
                var pager = new StoragePager(store, served.Meta.PageSize, options.CacheBytes, codec);
                var catalog = await ReadServedCatalogAsync(pager, served, cancellationToken).ConfigureAwait(false);
                var state = new FileState(null, "", options) { Client = client, Reconnect = Connect, RemoteCredential = credential };
                database = new(pager, new(served.Meta, 0, 0, [], [], []) { Encrypted = served.Encrypted }, catalog) { fileState = state, keyBlock = served.KeyBlock };
            }
            finally { await EndRemoteReadAsync(client, read.TransactionId).ConfigureAwait(false); }
            database.ObserveClient(client);
            database.SetWorkMem(options.WorkMem == 0 ? DefaultWorkMem : options.WorkMem);
            database.ScratchHost = new(options.ScratchEncryption == ScratchEncryption.On || database.ownedCodec is not null, options.ScratchEntropy, devices: options.ScratchDevices);
            return database;
        }
        catch
        {
            if (client is not null) await client.DisposeAsync().ConfigureAwait(false);
            codec?.Dispose(); CryptographicOperations.ZeroMemory(credential.Bytes); throw;
        }
    }

    /// <summary>Serve one page-service stream, relaying to the live owner or becoming a yielding owner.</summary>
    public static async Task ServePageStreamAsync(string path, Stream stream, CancellationToken cancellationToken = default)
    {
        PageWireMessage? hello = null; PageWireCredential? credential = null;
        FileClaim? claim = null; FileLockBundle? bundle = null; Database? ownerDatabase = null;
        var forwarded = false;
        try
        {
            hello = await PageWire.ReadAsync(stream, cancellationToken: cancellationToken).ConfigureAwait(false);
            if (hello is null) return;
            if (hello.Code != 1) throw PageWire.Invalid("first request must be hello");
            credential = hello.Get<PageWireCredential>("credential");
            var fields = new Dictionary<string, object>(hello.Fields, StringComparer.Ordinal) { ["credential"] = new PageWireCredential(0, []) };
            var stripped = hello with { Fields = fields };
            claim = FileClaim.Acquire(path, false);
            bundle = await FileLockBundle.PrepareAsync(claim, 5000, cancellationToken).ConfigureAwait(false);
            var elapsed = Stopwatch.StartNew();
            while (true)
            {
                cancellationToken.ThrowIfCancellationRequested();
                if (elapsed.ElapsedMilliseconds >= 5000) throw new SqlException(SqlStates.ObjectInUse, "stream owner lock timeout");
                if (bundle.TryBecomeOwner())
                {
                    using var unlocking = credential.Kind switch
                    {
                        0 => null, 1 => DatabaseCredential.FromPassphrase(credential.Bytes), 2 => DatabaseCredential.FromKey(credential.Bytes),
                        _ => throw PageWire.Invalid("invalid stream credential")
                    };
                    var options = new DatabaseFileOptions { Ownership = FileOwnership.Yield, Credential = unlocking,
                        ReadOnly = (hello.Get<byte>("flags") & 1) != 0, ShutdownGraceMilliseconds = 0 };
                    var transferredClaim = claim; var transferredBundle = bundle; claim = null; bundle = null;
                    ownerDatabase = await OpenFileCoreAsync(path, options, false, 8192, cancellationToken, transferredClaim, transferredBundle).ConfigureAwait(false);
                    CryptographicOperations.ZeroMemory(credential.Bytes);
                    forwarded = true;
                    await ownerDatabase.fileState!.Server!.ServeStreamAsync(new PrefixedStream(stream, PageWire.Encode(stripped))).ConfigureAwait(false);
                    return;
                }
                var record = bundle.ReadOwner();
                if (record is { Serving: true } && record.Uid == LinuxFileHost.EffectiveUid)
                {
                    var socket = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
                    try
                    {
                        await socket.ConnectAsync(new UnixDomainSocketEndPoint(record.Endpoint), cancellationToken).ConfigureAwait(false);
                    }
                    catch (SocketException) { socket.Dispose(); await Task.Delay(10, cancellationToken).ConfigureAwait(false); continue; }
                    await using var endpoint = new NetworkStream(socket, ownsSocket: true);
                    await PageWire.WriteAsync(endpoint, stripped, cancellationToken: cancellationToken).ConfigureAwait(false);
                    forwarded = true; CryptographicOperations.ZeroMemory(credential.Bytes);
                    bundle.Dispose(); bundle = null; claim.Dispose(); claim = null;
                    using var stop = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
                    var upstream = stream.CopyToAsync(endpoint, 65536, stop.Token);
                    var downstream = endpoint.CopyToAsync(stream, 65536, stop.Token);
                    await Task.WhenAny(upstream, downstream).ConfigureAwait(false); stop.Cancel();
                    try { await Task.WhenAll(upstream, downstream).ConfigureAwait(false); }
                    catch (Exception error) when (error is OperationCanceledException or IOException or SocketException) { }
                    return;
                }
                if (elapsed.ElapsedMilliseconds >= 5000) throw new SqlException(SqlStates.ObjectInUse, "stream owner lock timeout");
                await Task.Delay(10, cancellationToken).ConfigureAwait(false);
            }
        }
        catch (Exception error) when (!forwarded && error is not OperationCanceledException)
        {
            var sql = error as SqlException ?? new SqlException(SqlStates.IoError, error.Message);
            await PageWire.WriteAsync(stream, PageClientConnection.Message(0x7f, hello?.RequestId ?? 0,
                ("sqlstate", sql.SqlState), ("message", sql.Message), ("detail", ""), ("flags", (byte)1)), cancellationToken: cancellationToken).ConfigureAwait(false);
        }
        finally
        {
            if (credential is not null) CryptographicOperations.ZeroMemory(credential.Bytes);
            if (ownerDatabase is not null) await ownerDatabase.DisposeAsync().ConfigureAwait(false);
            bundle?.Dispose(); claim?.Dispose();
        }
    }

    private sealed class PrefixedStream(Stream stream, byte[] prefix) : Stream
    {
        private int position;
        public override bool CanRead => true; public override bool CanWrite => true; public override bool CanSeek => false;
        public override long Length => throw new NotSupportedException();
        public override long Position { get => throw new NotSupportedException(); set => throw new NotSupportedException(); }
        public override void Flush() => stream.Flush();
        public override Task FlushAsync(CancellationToken token) => stream.FlushAsync(token);
        public override int Read(byte[] buffer, int offset, int count) => throw new NotSupportedException();
        public override async ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken token = default)
        {
            if (position < prefix.Length) { var count = Math.Min(buffer.Length, prefix.Length - position); prefix.AsMemory(position, count).CopyTo(buffer); position += count; return count; }
            return await stream.ReadAsync(buffer, token).ConfigureAwait(false);
        }
        public override void Write(byte[] buffer, int offset, int count) => stream.Write(buffer, offset, count);
        public override ValueTask WriteAsync(ReadOnlyMemory<byte> buffer, CancellationToken token = default) => stream.WriteAsync(buffer, token);
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        protected override void Dispose(bool disposing) { if (disposing) stream.Dispose(); base.Dispose(disposing); }
        public override ValueTask DisposeAsync() => stream.DisposeAsync();
    }
}
