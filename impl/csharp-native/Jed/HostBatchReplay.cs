namespace Jed;

// Speculation owns no SQL work: ordinary scalar replay still charges, checks
// strict NULLs and validates each result at its original row.
internal sealed class HostBatchReplay(Meter meter)
{
    private sealed record Outcome(SqlValue Value, Exception? Error = null);
    private sealed class Site
    {
        internal readonly IReadOnlyList<SqlValue[]> Rows;
        internal readonly HashSet<Resolved> Calls;
        internal int Position;
        internal readonly Dictionary<Resolved, Dictionary<SqlValue[], Outcome>> Outcomes = new(ReferenceEqualityComparer.Instance);
        internal Site(IReadOnlyList<SqlValue[]> rows, HashSet<Resolved> calls)
        {
            Rows = rows; Calls = calls;
        }
    }
    private Site? current;
    private HostScanBatch? scan;
    internal HostScanBatch? BeginScan(SelectPlan plan, bool streaming)
    {
        if (!streaming || plan.Group is not null || plan.Windows is not null || plan.Distinct || plan.Order is { Length: > 0 }) return null;
        var candidate = new HostScanBatch(this, scan, plan);
        if (!HasEligible(candidate.Roots)) return null;
        scan = candidate; return candidate;
    }
    internal HostScanBatch? ScanFor(SelectPlan plan)
    {
        for (var current = scan; current is not null; current = current.Previous)
            if (ReferenceEquals(current.Identity, plan.HostBatchIdentity)) return current;
        return null;
    }
    internal void RestoreScan(HostScanBatch? previous) => scan = previous;
    internal static bool HasEligible(IEnumerable<Resolved> roots) => Eligible(roots).Count != 0;
    private static HashSet<Resolved> Eligible(IEnumerable<Resolved> roots)
    {
        var calls = new HashSet<Resolved>(ReferenceEqualityComparer.Instance);
        void Visit(Resolved expression)
        {
            if (expression.HostFunction is { Batched: true, Volatility: not FunctionVolatility.Volatile } && expression.Args.Length != 0 &&
                expression.Args.All(argument => argument.Op is "column" or "constant")) calls.Add(expression);
            if (expression.Op is "cast" or "promote" or "collate" or "neg" or "not" or "is_null" or "is_not_null" or
                "+" or "-" or "*" or "/" or "%" or "=" or "<>" or "!=" or "<" or "<=" or ">" or ">=")
                foreach (var argument in expression.Args) Visit(argument);
        }
        foreach (var root in roots) Visit(root);
        return calls;
    }
    internal IDisposable Begin(IEnumerable<Resolved> roots, IReadOnlyList<SqlValue[]> rows)
    {
        var calls = Eligible(roots);
        var previous = current;
        current = calls.Count == 0 ? null : new(rows, calls);
        return new Restore(this, previous);
    }
    private sealed class Restore(HostBatchReplay owner, Site? previous) : IDisposable
    { public void Dispose() => owner.current = previous; }

    internal bool TryGet(Resolved call, SqlValue[]? row, out SqlValue value)
    {
        value = default;
        if (row is null || current is not { } site || !site.Calls.Contains(call)) return false;
        var start = site.Position;
        while (start < site.Rows.Count && !ReferenceEquals(row, site.Rows[start])) start++;
        if (start == site.Rows.Count) return false;
        site.Position = start;
        if (!site.Outcomes.TryGetValue(call, out var cached))
            site.Outcomes.Add(call, cached = new(ReferenceEqualityComparer.Instance));
        if (!cached.TryGetValue(row, out var outcome))
        {
            if (!Arguments(call, row, out _)) return false;
            cached.Clear();
            var function = call.HostFunction!;
            var count = (int)Math.Min(1024L, site.Rows.Count - start);
            if (function.Cost > 0 && meter.HostBatchHeadroom is { } headroom)
                count = headroom / function.Cost >= count ? count : (int)(headroom / function.Cost) + 1;
            var selected = new List<SqlValue[]>();
            var columns = Enumerable.Range(0, call.Args.Length).Select(_ => new List<SqlValue>()).ToArray();
            for (var i = start; i < start + count; i++)
            {
                meter.CheckCancellation();
                if (!Arguments(call, site.Rows[i], out var arguments)) continue;
                selected.Add(site.Rows[i]);
                for (var column = 0; column < columns.Length; column++) columns[column].Add(arguments[column]);
            }
            var output = new List<SqlValue>(); Exception? error = null;
            try { function.Kernel(columns, output); }
            catch (Exception caught) { error = HostFunctionExecution.NormalizeError(caught); }
            if (output.Count > selected.Count || error is not null && output.Count == selected.Count)
                cached[selected[0]] = new(default, new SqlException(SqlStates.DataException, "host batch has no failing row"));
            else
            {
                for (var i = 0; i < output.Count; i++) cached.TryAdd(selected[i], new(output[i]));
                if (output.Count < selected.Count)
                    cached.TryAdd(selected[output.Count], new(default, error ?? new SqlException(SqlStates.DataException, "host batch returned too few rows")));
            }
            if (!cached.TryGetValue(row, out outcome)) return false;
        }
        if (outcome.Error is { } failure) System.Runtime.ExceptionServices.ExceptionDispatchInfo.Capture(failure).Throw();
        value = outcome.Value; return true;
    }
    private static bool Arguments(Resolved call, SqlValue[] row, out SqlValue[] arguments)
    {
        arguments = new SqlValue[call.Args.Length];
        for (var i = 0; i < arguments.Length; i++)
        {
            var argument = call.Args[i];
            // A scan can retain a decode failure until its original row guard.
            // Its empty marker has no readable columns to speculate over.
            if (argument.Op == "column" && (uint)argument.Slot >= row.Length) return false;
            var value = argument.Op == "constant" ? argument.Constant : row[argument.Slot];
            if (value.IsDeferred || value.IsNull) return false;
            arguments[i] = value;
        }
        return true;
    }
}
