namespace Jed;

internal static class HostFunctionExecution
{
    internal static SqlValue Call(Resolved expression, SqlValue[] arguments, Meter meter, SqlValue[]? row)
    {
        if (arguments.Any(value => value.IsNull)) return SqlValue.Null(expression.Type);
        var function = expression.HostFunction!;
        return CheckResult(function, meter.HostBatches.TryGet(expression, row, out var prefetched)
            ? prefetched : InvokeSingle(function, arguments));
    }
    internal static SqlValue InvokeSingle(HostFunction function, IReadOnlyList<SqlValue> arguments)
    {
        var output = new List<SqlValue>(); Exception? failure = null;
        try { function.Kernel(arguments.Select(value => (IReadOnlyList<SqlValue>)new[] { value }).ToArray(), output); }
        catch (Exception error) { failure = NormalizeError(error); }
        if (output.Count > 1 || failure is not null && output.Count == 1)
            throw new SqlException(SqlStates.DataException, "host kernel did not identify a failing row");
        if (failure is not null) throw failure;
        if (output.Count != 1) throw new SqlException(SqlStates.DataException, "host kernel omitted its result");
        return output[0];
    }
    internal static Exception NormalizeError(Exception error) => error is SqlException ? error :
        new SqlException(SqlStates.DataException, $"host function failed: {error.Message}");
    internal static SqlValue CheckResult(HostFunction function, SqlValue value)
    {
        if (value.IsNull) return SqlValue.Null(function.Result);
        if (function.Result.IsInteger() && value.Type.IsInteger()) return Resolve.Cast(value, function.Result);
        if (value.Type != function.Result) throw new SqlException(SqlStates.DataException, "host function returned the wrong type");
        return value;
    }
}
