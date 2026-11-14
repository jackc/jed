using Jed;

internal static class MeterAttemptTests
{
    internal static async Task Run()
    {
        foreach (var statementLimit in new[] { 9L, 10L, 11L })
        {
            await using var session = new Session(10);
            var meter = new Meter(session, statementLimit, default, new() { MaxQueryMemoryBytes = 100, MaxScalarBytes = 20 });
            meter.Charge(2); meter.ScalarReserve(3); meter.Memory.HoldPending(7);
            var mark = meter.Mark();
            meter.Memory.Reserve(30); meter.ScalarReserve(17); meter.Charge(20);
            string? discarded = null;
            try { meter.Guard(); } catch (SqlException failure) { discarded = failure.SqlState; }
            Corpus.Equal(statementLimit <= 10 ? SqlStates.CostLimitExceeded : SqlStates.SessionCostLimitExceeded,
                discarded, "attempt records first crossed cost ceiling");
            meter.Memory.Release(30);
            meter.Rewind(mark);
            Corpus.Equal(2L, meter.Accrued, "rewind statement cost");
            Corpus.Equal(2L, session.LifetimeCost, "rewind session cumulative cost");
            Corpus.Equal(7L, meter.Memory.Used, "rewind memory balance retains pending writes");
            Corpus.Equal(7L, meter.Memory.Peak, "rewind excludes rejected attempt peak");
            Corpus.Equal(0, meter.Memory.ReleaseUnderflows, "attempt cleanup releases before rewind");
            meter.ScalarReserve(17); // succeeds only if the discarded allowance was returned
            meter.Guard(); // first-ceiling state must have been returned too
            meter.Charge(7);
            string? replay = null;
            try { meter.Guard(); } catch (SqlException failure) { replay = failure.SqlState; }
            Corpus.Equal(statementLimit == 9 ? SqlStates.CostLimitExceeded : null, replay, "replay uses restored ceiling state");
        }
        await using (var session = new Session())
        {
            var meter = new Meter(session, 0, default, new() { MaxQueryMemoryBytes = 100 });
            var mark = meter.Mark(); // the memory account does not exist yet
            meter.Memory.Reserve(20); meter.Memory.Release(20); meter.Rewind(mark);
            Corpus.Equal(0L, meter.Memory.Used, "new account rewinds to zero");
            Corpus.Equal(0L, meter.Memory.Peak, "new account discards attempted peak");
            meter.Charge(23); meter.Refund(7);
            Corpus.Equal(16L, meter.Accrued, "filter error refunds only speculative fold cost");
            Corpus.Equal(16L, session.LifetimeCost, "filter refund also settles lifetime cost");
        }
        Console.WriteLine("PASS attempt meter rewind: statement/session ceilings, scalar allowance, pending balance and peak");
    }
}
