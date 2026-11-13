namespace Jed;

internal sealed class Meter(Session session, long limit, CancellationToken cancellation, QueryOptions? options = null)
{
    internal CancellationToken Cancellation => cancellation;
    internal long Accrued { get; private set; }
    private SequenceContext? sequences;
    internal SequenceContext Sequences => sequences ??= new(session.SequenceStateSnapshot(), this);
    internal TemporalContext Temporal { get; } = session.NewTemporalContext(options);
    private string? firstLimit;
    private long scalarBytes;
    private readonly long scalarLimit = options?.MaxScalarBytes is > 0 ? options.MaxScalarBytes : 64L * 1024 * 1024;
    private int suppressed;
    internal CostScope SuppressCost() { suppressed++; return new(this); }
    internal readonly struct CostScope(Meter meter) : IDisposable
    {
        public void Dispose() => meter.suppressed--;
    }
    internal void ScalarOutput(long bytes)
    {
        if (bytes < 0 || bytes > 0x3fffffff) throw new SqlException(SqlStates.ProgramLimitExceeded, "scalar result too large");
        Charge(bytes * Costs.ScalarByte); Guard();
        ScalarReserve(bytes);
    }
    internal void ScalarReserve(long bytes)
    {
        if (bytes < 0 || bytes > scalarLimit - scalarBytes)
            throw new SqlException(SqlStates.ScalarMemoryLimitExceeded, "scalar memory allowance exceeded");
        scalarBytes += bytes;
    }

    internal void Charge(long units)
    {
        if (units < 0) throw new InvalidOperationException("negative cost charge");
        if (suppressed != 0) return;
        if (firstLimit is null)
        {
            var statementHit = limit > 0 && units >= limit - Accrued;
            var lifetimeHit = session.LifetimeMaxCost > 0 && units >= session.LifetimeMaxCost - session.LifetimeCost;
            if (statementHit) firstLimit = SqlStates.CostLimitExceeded;
            if (lifetimeHit && (!statementHit || session.LifetimeMaxCost - session.LifetimeCost < limit - Accrued))
                firstLimit = SqlStates.SessionCostLimitExceeded;
        }
        Accrued = Add(Accrued, units);
        session.LifetimeCost = Add(session.LifetimeCost, units);
    }
    internal static long Add(long current, long units) => units > long.MaxValue - current ? long.MaxValue : current + units;
    internal void Guard()
    {
        if (firstLimit is not null) throw new SqlException(firstLimit, "cost ceiling reached");
        if (limit > 0 && Accrued >= limit) throw new SqlException(SqlStates.CostLimitExceeded, "cost ceiling reached");
        if (session.LifetimeMaxCost > 0 && session.LifetimeCost >= session.LifetimeMaxCost)
            throw new SqlException(SqlStates.SessionCostLimitExceeded, "session cost ceiling reached");
    }
    internal void Checkpoint(CancellationToken additional = default)
    {
        if (cancellation.IsCancellationRequested) throw new SqlCanceledException(cancellation);
        if (additional.IsCancellationRequested) throw new SqlCanceledException(additional);
        Guard();
    }
}
