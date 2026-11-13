using System.Net.Sockets;
using Jed;

internal static class PageClientTests
{
    internal static async Task Run()
    {
        var path = Path.Combine(Path.GetTempPath(), "jed-csharp-wire-" + Guid.NewGuid().ToString("N") + ".sock");
        using var listener = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
        listener.Bind(new UnixDomainSocketEndPoint(path)); listener.Listen(1);
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(10));
        var pendingWriteSeen = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var pendingCancelSeen = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var meta = new byte[4096]; var page = Enumerable.Repeat((byte)0xab, 4096).ToArray(); var incarnation = new byte[16];
        async Task Serve()
        {
            using var socket = await listener.AcceptAsync(timeout.Token);
            await using var stream = new NetworkStream(socket, ownsSocket: false);
            var hello = (await PageWire.ReadAsync(stream, cancellationToken: timeout.Token))!;
            Corpus.Equal((byte)1, hello.Code, "hello first"); Corpus.Equal((byte)0, hello.Get<PageWireCredential>("credential").Kind, "local credential stripped");
            await PageWire.WriteAsync(stream, PageClientConnection.Message(0x41, hello.RequestId, ("protocol", (ushort)1),
                ("format_version", StorageFormat.Version), ("page_size", 4096u), ("connection_id", 1ul), ("flags", (byte)0), ("max_read_pages", 1024u)), cancellationToken: timeout.Token);
            var write = (await PageWire.ReadAsync(stream, 4096, timeout.Token))!;
            Corpus.Equal((byte)5, write.Code, "writer waits"); pendingWriteSeen.SetResult();
            var read = (await PageWire.ReadAsync(stream, 4096, timeout.Token))!;
            Corpus.Equal((byte)2, read.Code, "read passes waiting writer");
            await PageWire.WriteAsync(stream, PageClientConnection.Message(0x42, read.RequestId, ("incarnation", incarnation), ("txid", 7ul),
                ("invalidate", (byte)1), ("indices", Array.Empty<uint>()), ("meta", meta)), 4096, timeout.Token);
            var pages = (await PageWire.ReadAsync(stream, 4096, timeout.Token))!;
            Corpus.Equal((byte)3, pages.Code, "page read passes waiting writer");
            await PageWire.WriteAsync(stream, PageClientConnection.Message(0x43, pages.RequestId, ("pages", new byte[][] { page })), 4096, timeout.Token);
            await PageWire.WriteAsync(stream, PageClientConnection.Message(0x45, write.RequestId, ("incarnation", incarnation), ("txid", 7ul),
                ("invalidate", (byte)0), ("indices", Array.Empty<uint>()), ("reuse_grant", true), ("meta", meta),
                ("pending", Array.Empty<PageWireDigest>()), ("reuse", Array.Empty<uint>())), 4096, timeout.Token);
            var end = (await PageWire.ReadAsync(stream, 4096, timeout.Token))!;
            Corpus.Equal((byte)4, end.Code, "end_read has no reply");
            var abort = (await PageWire.ReadAsync(stream, 4096, timeout.Token))!;
            Corpus.Equal((byte)9, abort.Code, "abort has no reply");
            var canceledRead = (await PageWire.ReadAsync(stream, 4096, timeout.Token))!;
            Corpus.Equal((byte)2, canceledRead.Code, "cancelable read begin"); pendingCancelSeen.SetResult();
            Corpus.Equal<PageWireMessage?>(null, await PageWire.ReadAsync(stream, 4096, timeout.Token), "cancellation disconnects and releases remote pins");
        }
        var serving = Serve();
        try
        {
            await using var client = await PageClientConnection.ConnectLocalAsync(path, cancellationToken: timeout.Token);
            var write = client.BeginWriteAsync(incarnation, 0, 0, cancellationToken: timeout.Token).AsTask();
            await pendingWriteSeen.Task.WaitAsync(timeout.Token);
            var read = await client.BeginReadAsync(incarnation, 0, timeout.Token);
            Corpus.Equal(7ul, read.TransactionId, "read version"); Corpus.Equal(false, write.IsCompleted, "out-of-order reply matched by id");
            var pages = await client.ReadPagesAsync(read.TransactionId, [2], timeout.Token);
            Corpus.Equal(Convert.ToHexString(page), Convert.ToHexString(pages[0]), "page payload");
            Corpus.Equal(true, (await write).ReuseGrant, "writer grant");
            await client.EndReadAsync(read.TransactionId, timeout.Token); await client.AbortAsync(timeout.Token);
            using var cancellation = new CancellationTokenSource();
            var abandoned = client.BeginReadAsync(incarnation, 7, cancellation.Token).AsTask();
            await pendingCancelSeen.Task.WaitAsync(timeout.Token); cancellation.Cancel();
            try { _ = await abandoned; throw new InvalidDataException("cancellation did not stop wait"); }
            catch (OperationCanceledException) { }
            await serving;
        }
        finally { listener.Dispose(); File.Delete(path); }
        Console.WriteLine("PASS page-client host transport: real Unix socket, negotiated limits, out-of-order writer/read replies, pin messages and cancellation disconnect");
    }
}
