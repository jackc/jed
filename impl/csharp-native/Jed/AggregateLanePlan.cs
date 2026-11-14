using System.Runtime.CompilerServices;

namespace Jed;

// memory.md §5.4 and cost.md §6.1 make lane admission and rejected attempts
// observable contracts. This is separate from predicate pushdown's no-error gate.
internal static class AggregateLanePlan
{
    internal static bool RequiresAttempt(SelectPlan plan, Meter meter) => meter.HasCostCeiling ||
        plan.Group!.Aggregates.Any(aggregate => aggregate.Argument is { Op: not "column" });

    private static bool Scalar(SqlType type) => type.Kind is not
        (SqlKind.Array or SqlKind.Composite or SqlKind.Range or SqlKind.Json or SqlKind.Jsonb or SqlKind.Jsonpath);

    internal static bool DiscardSafe(Resolved? expression)
    {
        if (expression is null) return true;
        if (expression.Op == "in_init" && expression.Subquery is { Initialized: true, PerRow: false, IsCorrelated: false } membership)
        {
            var left = expression.Args[0].Type; var right = membership.RowType;
            // Folded exact/promoted scalar points retain membership bookkeeping;
            // ordinary folded values have no state to rewind (cost.md §6.1).
            var scalar = left.Kind is not (SqlKind.Array or SqlKind.Composite or SqlKind.Range or SqlKind.Unknown);
            var points = scalar && left == right || AccessConstant.NumericFamilies(left, right);
            return !points && DiscardSafe(expression.Args[0]);
        }
        if (expression.HostFunction is not null || expression.Subquery is not null || expression.Outer is not null ||
            expression.Regex is not null || expression.Similar is not null || expression.Json is not null) return false;
        if (expression.Op == "column") return true;
        if (expression.Op == "constant") return expression.ParameterIndex > 0 || expression.Constant.IsNull || Scalar(expression.Type);
        if (expression.Op is "cast" or "promote" or "float_widen")
            return expression.Type.Kind is not (SqlKind.Array or SqlKind.Composite or SqlKind.Range) &&
                expression.Args[0].Type.Kind is not (SqlKind.Array or SqlKind.Composite or SqlKind.Range) && DiscardSafe(expression.Args[0]);
        if (expression.Op == "call")
        {
            // Array/range/JSON and variadic calls have distinct nodes in the
            // neutral allowlist even though this evaluator shares its call node.
            if (expression.Function is not { Volatility: not "volatile" } || !Scalar(expression.Type) ||
                expression.Args.Any(arg => !Scalar(arg.Type)) ||
                expression.Text is "num_nulls" or "num_nonnulls" or "concat" or "concat_ws" or "format") return false;
            return expression.Args.All(DiscardSafe);
        }
        return expression.Op is "not" or "and" or "or" or "neg" or "+" or "-" or "*" or "/" or "%" or "^" or "||" or
            "=" or "<>" or "!=" or "<" or "<=" or ">" or ">=" or "distinct" or "not_distinct" or "is_null" or "is_not_null" or
            "like" or "ilike" or "case" or "coalesce" or "nullif" or "greatest" or "least" or "extract" or "at_time_zone" or "stable_date" or "in_list" or "is_boolean" or "collate"
            && expression.Args.All(DiscardSafe);
    }

    internal static async IAsyncEnumerable<SqlValue[]> Filtered(IAsyncEnumerable<SqlValue[]> source, Resolved filter, Meter meter,
        [EnumeratorCancellation] CancellationToken token, bool settleErrors, ExplainNode? observation)
    {
        long folded = 0;
        await using var input = source.GetAsyncEnumerator(token);
        while (await input.MoveNextAsync().ConfigureAwait(false))
        {
            var row = input.Current;
            bool keep;
            try { keep = await QueryExecution.KeepsAsync(filter, row, meter, observation).ConfigureAwait(false); }
            catch (SqlException) when (settleErrors)
            {
                // A non-attempt lane may have evaluated volatile filters. Do not
                // replay them: settle the whole bound's scan and refund folds.
                while (await input.MoveNextAsync().ConfigureAwait(false)) { }
                meter.Refund(folded);
                throw;
            }
            if (!keep) continue;
            var before = meter.Accrued;
            yield return row;
            folded = Meter.Add(folded, meter.Accrued - before);
        }
    }

    internal static async IAsyncEnumerable<SqlValue[]> AcceptedSource(IAsyncEnumerable<SqlValue[]> source, Attempt attempt,
        [EnumeratorCancellation] CancellationToken token)
    {
        await foreach (var row in source.WithCancellation(token).ConfigureAwait(false))
        { attempt.Accept(); yield return row; }
        attempt.Accept();
    }

    internal sealed class Attempt(Meter meter)
    {
        internal bool Accepted { get; private set; }
        internal void Accept()
        {
            if (Accepted) return;
            meter.Guard(); Accepted = true;
        }
    }
}
