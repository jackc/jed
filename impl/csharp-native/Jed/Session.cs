using System.Collections.ObjectModel;

namespace Jed;

/// <summary>Experimental scalar-only session. Concurrent operations are rejected.</summary>
public sealed class Session : IAsyncDisposable
{
    private int active;
    private bool disposed;
    public long LifetimeCost { get; internal set; }
    public long LifetimeMaxCost { get; }

    public Session(long lifetimeMaxCost = 0)
    {
        if (lifetimeMaxCost < 0) throw new ArgumentOutOfRangeException(nameof(lifetimeMaxCost));
        LifetimeMaxCost = lifetimeMaxCost;
    }

    /// <summary>Resolve now; evaluate lazily as the returned cursor is drained.</summary>
    public async Task<Rows> QueryAsync(string sql, long maxCost = 0, CancellationToken cancellationToken = default)
    {
        // The resident-only slice has nothing to suspend on. Keep an async method
        // so OperationCanceledException produces a canceled Task while retaining
        // its structured error. Storage will await real admission/page work here.
        await Task.CompletedTask.ConfigureAwait(false);
        using var operation = Enter();
        ArgumentNullException.ThrowIfNull(sql);
        if (maxCost < 0) throw new ArgumentOutOfRangeException(nameof(maxCost));
        if (cancellationToken.IsCancellationRequested) throw new SqlCanceledException(cancellationToken);
        var meter = new Meter(this, maxCost, cancellationToken);
        meter.Checkpoint();
        var plan = Resolve.Select(new Parser(sql).Parse());
        return new Rows(this, plan, meter);
    }

    internal Operation Enter()
    {
        if (Interlocked.CompareExchange(ref active, 1, 0) != 0)
            throw new SqlException(SqlStates.ObjectInUse, "session operation already in progress");
        if (disposed) { Volatile.Write(ref active, 0); throw new ObjectDisposedException(nameof(Session)); }
        return new Operation(this);
    }
    internal readonly struct Operation(Session session) : IDisposable
    {
        public void Dispose() => Volatile.Write(ref session.active, 0);
    }
    public ValueTask DisposeAsync()
    {
        if (disposed) return ValueTask.CompletedTask;
        using var operation = Enter();
        disposed = true;
        return ValueTask.CompletedTask;
    }
}

public sealed record Column(string Name, SqlType Type);

/// <summary>An owned row; values remain valid after advancing or closing the cursor.</summary>
public sealed class Row
{
    private readonly SqlValue[] values;
    internal Row(SqlValue[] values) { this.values = values; }
    public int Count => values.Length;
    public SqlValue this[int index] => index >= 0 && index < values.Length ? values[index] : throw new SqlException(SqlStates.UndefinedColumn, "column index out of range");
}

/// <summary>A single-pass asynchronous stream with exact cost available after drain or failure.</summary>
public sealed class Rows : IAsyncEnumerable<Row>, IAsyncDisposable
{
    private readonly Session session;
    private readonly SelectPlan plan;
    private readonly Meter meter;
    private bool finished;
    private bool disposed;
    private int enumerated;
    private int active;
    public ReadOnlyCollection<Column> Columns { get; }
    public long Cost => meter.Accrued;

    internal Rows(Session session, SelectPlan plan, Meter meter)
    {
        this.session = session; this.plan = plan; this.meter = meter;
        Columns = Array.AsReadOnly(plan.Projections.Select(p => new Column(p.Name, p.Expression.Type)).ToArray());
    }

    public async ValueTask<Row?> ReadAsync(CancellationToken cancellationToken = default)
    {
        await ValueTask.CompletedTask.ConfigureAwait(false);
        // Admission failures must not modify a cursor another operation owns.
        using var operation = session.Enter();
        using var cursorOperation = Enter();
        ObjectDisposedException.ThrowIf(disposed, this);
        try
        {
            if (finished) return null;
            meter.Checkpoint(cancellationToken);
            finished = true;
            if (plan.Limit == 0 || plan.Empty) return null;
            if (plan.Filter is not null)
            {
                var filter = Evaluate.Expr(plan.Filter, meter, true);
                if (filter.IsNull || !filter.AsBoolean()) { meter.Checkpoint(cancellationToken); return null; }
            }
            if (plan.Offset > 0) { meter.Checkpoint(cancellationToken); return null; }
            meter.Checkpoint(cancellationToken);
            meter.Charge(Costs.RowProduced);
            var values = new SqlValue[plan.Projections.Length];
            for (var i = 0; i < values.Length; i++) values[i] = Evaluate.Expr(plan.Projections[i].Expression, meter);
            meter.Checkpoint(cancellationToken);
            return new Row(values);
        }
        catch (Exception error)
        {
            finished = true;
            if (error is SqlException sql) sql.Cost = Cost;
            if (error is SqlCanceledException canceled) canceled.Cost = Cost;
            throw;
        }
    }

    public async IAsyncEnumerator<Row> GetAsyncEnumerator(CancellationToken cancellationToken = default)
    {
        if (Interlocked.Exchange(ref enumerated, 1) != 0) throw new InvalidOperationException("cursor can only be enumerated once");
        try
        {
            while (await ReadAsync(cancellationToken).ConfigureAwait(false) is { } row) yield return row;
        }
        finally { await DisposeAsync().ConfigureAwait(false); }
    }

    public ValueTask DisposeAsync()
    {
        // No asynchronous resources yet; storage will retain its own awaitable
        // release path. Disposal is idempotent and does not evaluate unread rows.
        if (Volatile.Read(ref disposed)) return ValueTask.CompletedTask;
        using var operation = Enter();
        disposed = true;
        return ValueTask.CompletedTask;
    }

    private CursorOperation Enter()
    {
        if (Interlocked.CompareExchange(ref active, 1, 0) != 0)
            throw new SqlException(SqlStates.ObjectInUse, "cursor operation already in progress");
        return new CursorOperation(this);
    }
    private readonly struct CursorOperation(Rows rows) : IDisposable
    {
        public void Dispose() => Volatile.Write(ref rows.active, 0);
    }
}
