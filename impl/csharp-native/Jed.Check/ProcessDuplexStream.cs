using System.Diagnostics;

// The actor's transport process is a host seam. Credentials stay inside the
// page hello; only the database path is supplied to the child process.
internal sealed class ProcessDuplexStream : Stream
{
    private readonly Stream input;
    private readonly Stream output;
    private readonly Process? process;
    private readonly Task? errors;
    private int disposed;
    internal ProcessDuplexStream(Stream input, Stream output) { this.input = input; this.output = output; }
    private ProcessDuplexStream(Process process) : this(process.StandardOutput.BaseStream, process.StandardInput.BaseStream)
    {
        this.process = process;
        errors = DrainErrors(process.StandardError);
    }
    internal static ValueTask<Stream> Connect(string path, CancellationToken token)
    {
        token.ThrowIfCancellationRequested();
        var executable = Environment.GetEnvironmentVariable("JED_PAGES_ACTOR") ?? throw new InvalidOperationException("missing stream actor executable");
        var start = new ProcessStartInfo(executable) { UseShellExecute = false, RedirectStandardInput = true, RedirectStandardOutput = true, RedirectStandardError = true };
        start.ArgumentList.Add("pages"); start.ArgumentList.Add(path);
        var process = Process.Start(start) ?? throw new IOException("stream actor failed to start");
        return ValueTask.FromResult<Stream>(new ProcessDuplexStream(process));
    }
    private static async Task DrainErrors(StreamReader source)
    {
        var buffer = new char[4096];
        while (await source.ReadAsync(buffer).ConfigureAwait(false) is var count && count != 0)
            await Console.Error.WriteAsync(buffer.AsMemory(0, count)).ConfigureAwait(false);
    }
    public override bool CanRead => true; public override bool CanWrite => true; public override bool CanSeek => false;
    public override long Length => throw new NotSupportedException();
    public override long Position { get => throw new NotSupportedException(); set => throw new NotSupportedException(); }
    public override void Flush() => output.Flush();
    public override Task FlushAsync(CancellationToken token) => output.FlushAsync(token);
    public override int Read(byte[] buffer, int offset, int count) => input.Read(buffer, offset, count);
    public override async ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken token = default)
    {
        // Console pipe implementations can retain an OS read after cancellation.
        // The connection must still unwind to release ownership; the pending
        // read owns this buffer until the host closes/exits the transport.
        return await input.ReadAsync(buffer, token).AsTask().WaitAsync(token).ConfigureAwait(false);
    }
    public override void Write(byte[] buffer, int offset, int count) => output.Write(buffer, offset, count);
    public override ValueTask WriteAsync(ReadOnlyMemory<byte> buffer, CancellationToken token = default) => output.WriteAsync(buffer, token);
    public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
    public override void SetLength(long value) => throw new NotSupportedException();
    protected override void Dispose(bool disposing)
    {
        if (disposing && Interlocked.Exchange(ref disposed, 1) == 0)
        {
            input.Dispose(); output.Dispose();
            if (process is not null) { try { if (!process.HasExited) process.Kill(entireProcessTree: true); } catch (InvalidOperationException) { } process.Dispose(); }
        }
        base.Dispose(disposing);
    }
    public override async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref disposed, 1) != 0) return;
        await input.DisposeAsync().ConfigureAwait(false); await output.DisposeAsync().ConfigureAwait(false);
        if (process is not null)
        {
            try { if (!process.HasExited) process.Kill(entireProcessTree: true); } catch (InvalidOperationException) { }
            await process.WaitForExitAsync().ConfigureAwait(false);
            if (errors is not null) await errors.ConfigureAwait(false);
            process.Dispose();
        }
        GC.SuppressFinalize(this);
    }
}
