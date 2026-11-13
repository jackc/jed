namespace Jed;

internal sealed class Meter(Session session, long limit, CancellationToken cancellation, QueryOptions? options = null)
{
    internal CancellationToken Cancellation => cancellation;
    internal CursorCancellation? CursorCancellation { get; init; }
    internal long Accrued { get; private set; }
    internal ExplainSyntax? Explain { get; set; }
    private readonly long memoryLimit = options?.MaxQueryMemoryBytes ?? session.MaxQueryMemoryBytes;
    private QueryMemoryAccount? memory;
    internal QueryMemoryAccount Memory => memory ??= new(memoryLimit, this);
    private SequenceContext? sequences;
    internal SequenceContext Sequences => sequences ??= new(session.SequenceStateSnapshot(), this);
    internal Dictionary<string, string> Variables { get; } = session.VariableSnapshot();
    internal UuidContext Uuid { get; } = session.NewUuidContext(options);
    internal TemporalContext Temporal { get; } = session.NewTemporalContext(options);
    internal UnicodeData? Unicode => Temporal.Unicode;
    internal readonly Dictionary<RegexCall, RegexMachine> RegexMachines = new(ReferenceEqualityComparer.Instance);
    internal readonly Dictionary<SimilarCall, RegexMachine[]> SimilarMachines = new(ReferenceEqualityComparer.Instance);
    private string? firstLimit;
    private long statementOrigin;
    private HostBatchReplay? hostBatches;
    internal HostBatchReplay HostBatches => hostBatches ??= new(this);
    internal long? HostBatchHeadroom
    {
        get
        {
            long? headroom = limit > 0 ? Math.Max(0, limit - (Accrued - statementOrigin)) : null;
            if (session.LifetimeMaxCost > 0)
                headroom = Math.Min(headroom ?? long.MaxValue, Math.Max(0, session.LifetimeMaxCost - session.LifetimeCost));
            return headroom;
        }
    }
    internal SubqueryBudget BeginSubquery() => new(this);
    internal readonly struct SubqueryBudget : IDisposable
    {
        private readonly Meter meter;
        private readonly long origin;
        private readonly string? previousLimit;
        internal SubqueryBudget(Meter meter)
        {
            this.meter = meter; origin = meter.statementOrigin; previousLimit = meter.firstLimit;
            meter.statementOrigin = meter.Accrued; meter.firstLimit = null;
        }
        public void Dispose()
        {
            meter.statementOrigin = origin; meter.firstLimit = previousLimit;
            // The completed inner work accrues to its caller without a trailing
            // guard. The next caller work boundary enforces its own ceiling.
            meter.Charge(0);
        }
    }
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
            var statementHit = limit > 0 && units >= limit - (Accrued - statementOrigin);
            var lifetimeHit = session.LifetimeMaxCost > 0 && units >= session.LifetimeMaxCost - session.LifetimeCost;
            if (statementHit) firstLimit = SqlStates.CostLimitExceeded;
            if (lifetimeHit && (!statementHit || session.LifetimeMaxCost - session.LifetimeCost < limit - (Accrued - statementOrigin)))
                firstLimit = SqlStates.SessionCostLimitExceeded;
        }
        Accrued = Add(Accrued, units);
        session.LifetimeCost = Add(session.LifetimeCost, units);
    }
    internal static long Add(long current, long units) => units > long.MaxValue - current ? long.MaxValue : current + units;
    internal void Guard()
    {
        if (firstLimit is not null) throw new SqlException(firstLimit, "cost ceiling reached");
        if (limit > 0 && Accrued - statementOrigin >= limit) throw new SqlException(SqlStates.CostLimitExceeded, "cost ceiling reached");
        if (session.LifetimeMaxCost > 0 && session.LifetimeCost >= session.LifetimeMaxCost)
            throw new SqlException(SqlStates.SessionCostLimitExceeded, "session cost ceiling reached");
    }
    internal void Checkpoint(CancellationToken additional = default)
    {
        CheckCancellation(additional);
        Guard();
    }
    internal void CheckCancellation(CancellationToken additional = default)
    {
        if (cancellation.IsCancellationRequested) throw new SqlCanceledException(cancellation);
        if (additional.IsCancellationRequested) throw new SqlCanceledException(additional);
    }
}
