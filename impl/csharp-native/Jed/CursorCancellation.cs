namespace Jed;

// Continuations capture one stable token while QueryAsync prepares the cursor.
// Later ReadAsync/WithCancellation tokens cancel that same pending I/O, rather
// than merely being polled after it completes. Each read registration is scoped
// to its movement; completing a read does not arm future reads with its token.
internal sealed class CursorCancellation : IDisposable
{
    private readonly CancellationToken original;
    private readonly CancellationTokenSource source;
    private CancellationToken consumer;
    private int references = 1;
    internal CursorCancellation(CancellationToken original)
    {
        this.original = original;
        source = original.CanBeCanceled ? CancellationTokenSource.CreateLinkedTokenSource(original) : new();
        Token = source.Token;
    }
    internal CancellationToken Token { get; }
    internal CancellationToken CanceledToken => original.IsCancellationRequested ? original : consumer.IsCancellationRequested ? consumer : Token;
    internal IDisposable Retain()
    {
        Interlocked.Increment(ref references); return new Lease(this);
    }
    internal CancellationTokenRegistration Register(CancellationToken token)
    {
        consumer = token;
        return token.UnsafeRegister(static state => ((CursorCancellation)state!).source.Cancel(), this);
    }
    public void Dispose()
    {
        if (Interlocked.Decrement(ref references) == 0) source.Dispose();
    }
    private sealed class Lease(CursorCancellation owner) : IDisposable
    {
        private CursorCancellation? retained = owner;
        public void Dispose() => Interlocked.Exchange(ref retained, null)?.Dispose();
    }
}
