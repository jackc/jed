using Jed;

// Writes wait until their bytes are consumed. The separate quiescence barrier
// waits for both protocol readers, including work after an ACK, before a new step.
internal sealed class SimulationNetwork
{
    private readonly object gate = new();
    private readonly List<Link> links = [];
    private int frames, disconnectAt;
    internal bool Disconnected { get; private set; }
    internal void BeginStep(int nth = 0)
    { lock (gate) { frames = 0; disconnectAt = nth; Disconnected = false; } }
    private TaskCompletionSource changed = Signal();
    private static TaskCompletionSource Signal() => new(TaskCreationOptions.RunContinuationsAsynchronously);
    private void Pulse() { var previous = changed; changed = Signal(); previous.TrySetResult(); }
    internal (Stream Client, Stream Server) Connect(int client, int owner, string path)
    {
        lock (gate)
        {
            var link = new Link(this, client, owner, path); links.Add(link); Pulse();
            return (link.Client, link.Server);
        }
    }
    internal void Serve(Stream stream, Task serving)
    { lock (gate) { var endpoint = (Endpoint)stream; endpoint.Link.Serving = serving; _ = Finished(serving); } }
    private async Task Finished(Task task)
    { try { await task.ConfigureAwait(false); } finally { lock (gate) Pulse(); } }
    internal bool Disconnect(int client, string path)
    {
        lock (gate)
        {
            var found = false;
            foreach (var link in links.Where(link => !link.Ended && link.ClientProcess == client && link.Path == path))
            { found = true; link.End(); }
            return found;
        }
    }
    internal void Kill(int process)
    { lock (gate) foreach (var link in links.Where(link => link.ClientProcess == process || link.OwnerProcess == process)) link.End(); }
    internal async Task SettleAsync(CancellationToken token = default)
    {
        while (true)
        {
            Task wait;
            lock (gate)
            {
                if (links.All(link => link.Settled)) return;
                wait = changed.Task;
            }
            await wait.WaitAsync(token).ConfigureAwait(false);
        }
    }
    private sealed class Link
    {
        internal readonly SimulationNetwork Network;
        internal readonly int ClientProcess, OwnerProcess;
        internal readonly string Path;
        internal readonly Endpoint Client, Server;
        internal Task? Serving;
        internal bool Ended;
        internal bool Settled => Ended ? (Serving?.IsCompleted ?? true) && Client.ReadEnded : Client.Waiting && Server.Waiting;
        internal Link(SimulationNetwork network, int client, int owner, string path)
        { Network = network; ClientProcess = client; OwnerProcess = owner; Path = path; Client = new(this); Server = new(this); Client.Peer = Server; Server.Peer = Client; }
        internal void End()
        {
            if (Ended) return;
            Ended = true; Client.FailWrites(); Server.FailWrites(); Client.Wake(); Server.Wake(); Network.Pulse();
        }
    }
    private sealed class Endpoint(Link link) : Stream
    {
        internal readonly Link Link = link;
        internal Endpoint Peer = null!;
        internal bool Waiting, ReadEnded;
        private readonly Queue<byte[]> queued = [];
        private readonly List<TaskCompletionSource> consumed = [];
        private int offset;
        private TaskCompletionSource ready = Signal();
        internal void Wake() { var old = ready; ready = Signal(); old.TrySetResult(); }
        internal void FailWrites()
        { foreach (var receipt in consumed) receipt.TrySetException(new IOException("simulated connection lost")); consumed.Clear(); }
        public override bool CanRead => true;
        public override bool CanWrite => true;
        public override bool CanSeek => false;
        public override long Length => throw new NotSupportedException();
        public override long Position { get => throw new NotSupportedException(); set => throw new NotSupportedException(); }
        public override async ValueTask<int> ReadAsync(Memory<byte> destination, CancellationToken token = default)
        {
            if (destination.Length == 0) return 0;
            while (true)
            {
                Task wait;
                lock (Link.Network.gate)
                {
                    if (Link.Ended) { ReadEnded = true; Waiting = false; Link.Network.Pulse(); return 0; }
                    if (queued.TryPeek(out var bytes))
                    {
                        Waiting = false; var count = Math.Min(destination.Length, bytes.Length - offset);
                        bytes.AsMemory(offset, count).CopyTo(destination); offset += count;
                        if (offset == bytes.Length)
                        {
                            queued.Dequeue(); offset = 0;
                            if (queued.Count == 0)
                            {
                                foreach (var receipt in consumed) receipt.TrySetResult();
                                consumed.Clear();
                            }
                        }
                        Link.Network.Pulse(); return count;
                    }
                    Waiting = true;
                    foreach (var receipt in consumed) receipt.TrySetResult();
                    consumed.Clear(); Link.Network.Pulse(); wait = ready.Task;
                }
                await wait.WaitAsync(token).ConfigureAwait(false);
            }
        }
        public override async ValueTask WriteAsync(ReadOnlyMemory<byte> source, CancellationToken token = default)
        {
            if (source.Length == 0) return;
            Task receipt;
            lock (Link.Network.gate)
            {
                if (Link.Ended) throw new IOException("simulated connection lost");
                if (++Link.Network.frames == Link.Network.disconnectAt)
                { Link.Network.Disconnected = true; Link.End(); throw new IOException("simulated frame disconnect"); }
                var completion = Signal(); Peer.queued.Enqueue(source.ToArray()); Peer.consumed.Add(completion);
                Peer.Waiting = false; Peer.Wake(); Link.Network.Pulse(); receipt = completion.Task;
            }
            await receipt.WaitAsync(token).ConfigureAwait(false);
        }
        public override int Read(byte[] buffer, int offset, int count) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
        public override void Flush() { }
        public override Task FlushAsync(CancellationToken token) => Task.CompletedTask;
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        protected override void Dispose(bool disposing)
        {
            if (disposing) lock (Link.Network.gate)
            {
                ReadEnded = true; Link.End();
                Link.Network.Pulse();
            }
            base.Dispose(disposing);
        }
    }
}
