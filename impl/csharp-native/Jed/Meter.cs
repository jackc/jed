namespace Jed;

internal sealed class Meter(Session session, long limit, CancellationToken cancellation)
{
    internal long Accrued { get; private set; }
    private string? firstLimit;

    internal void Charge(long units)
    {
        if (units < 0) throw new InvalidOperationException("negative cost charge");
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
