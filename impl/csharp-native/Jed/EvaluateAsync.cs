namespace Jed;

internal static partial class Evaluate
{
    internal static ValueTask<SqlValue> ExprAsync(Resolved expression, Meter meter, bool filter = false, SqlValue[]? row = null)
    {
        if (expression.Op is "column" or "outer" or "constant")
        {
            meter.Guard();
            var value = expression.Op == "constant" ? expression.Constant : expression.Op == "outer" ? expression.Outer!.Value : row![expression.Slot];
            return value.ResolveAsync(meter.Cancellation);
        }
        return expression.NeedsAsync ? PendingExpression(expression, meter, filter, row) : ValueTask.FromResult(Expr(expression, meter, filter, row));
    }

    private static async ValueTask<SqlValue> PendingExpression(Resolved expression, Meter meter, bool filter, SqlValue[]? row)
    {
        meter.Guard();
        if (expression.Op == "collate") return await ExprAsync(expression.Args[0], meter, filter, row).ConfigureAwait(false);
        if (expression.Op == "in_init") return await SubqueryExecution.InAsync(expression, meter, row).ConfigureAwait(false);
        if (expression.Op is "promote") return TemporalText.Cast(await ExprAsync(expression.Args[0], meter, row: row).ConfigureAwait(false), expression.Type, meter);
        if (expression.Op == "in_list")
        {
            meter.Charge((expression.Args.Length - 2) * Costs.OperatorEval); meter.Guard();
            var input = await ExprAsync(expression.Args[0], meter, row: row).ConfigureAwait(false);
            return LiteralMembership(expression, input, meter);
        }
        meter.Charge(expression.Function?.Cost ?? Costs.OperatorEval);
        if (expression.Op is "json_sql" or "is_json") return await JsonSql.EvaluateAsync(expression, meter, row).ConfigureAwait(false);
        if (expression.Op == "correlated")
        {
            meter.Guard();
            try { await expression.Subquery!.Execute(row).ConfigureAwait(false); return expression.Subquery.Value; }
            finally { expression.Subquery!.ReleaseConsumed(); }
        }
        if (expression.Op is "quantified_init" or "quantified_correlated")
        {
            try
            {
                if (expression.Op == "quantified_correlated") { meter.Guard(); await expression.Subquery!.Execute(row).ConfigureAwait(false); }
                var input = await ExprAsync(expression.Args[0], meter, row: row).ConfigureAwait(false);
                return OpenOperators.QuantifiedValues(expression.Text, input, expression.Subquery!.Values, meter, expression.Collation);
            }
            finally { expression.Subquery!.ReleaseConsumed(); }
        }
        if (expression.Op == "in_correlated")
        {
            meter.Guard();
            try
            {
                await expression.Subquery!.Execute(row).ConfigureAwait(false);
                return await SubqueryExecution.InAsync(expression, meter, row, correlated: true).ConfigureAwait(false);
            }
            finally { expression.Subquery!.ReleaseConsumed(); }
        }
        if (expression.Op == "case")
        {
            for (var i = 0; i < expression.Args.Length - 1; i += 2)
            {
                var condition = await ExprAsync(expression.Args[i], meter, row: row).ConfigureAwait(false);
                if (!condition.IsNull && condition.AsBoolean()) return await ExprAsync(expression.Args[i + 1], meter, row: row).ConfigureAwait(false);
            }
            return await ExprAsync(expression.Args[^1], meter, row: row).ConfigureAwait(false);
        }
        if (expression.Op == "coalesce")
        {
            foreach (var argument in expression.Args)
            {
                var value = await ExprAsync(argument, meter, row: row).ConfigureAwait(false);
                if (!value.IsNull) return value;
            }
            return SqlValue.Null(expression.Type);
        }
        if (expression.Op is "greatest" or "least")
        {
            var best = SqlValue.Null(expression.Type);
            foreach (var argument in expression.Args)
            {
                var value = await ExprAsync(argument, meter, row: row).ConfigureAwait(false);
                if (value.IsNull) continue;
                if (best.IsNull || (expression.Op == "greatest" ? CollationRules.Compare(value, best, expression.Collation, meter) > 0 : CollationRules.Compare(value, best, expression.Collation, meter) < 0)) best = value;
            }
            return best;
        }
        var args = new Resolved[expression.Args.Length];
        for (var i = 0; i < args.Length; i++)
        {
            var argument = expression.Args[i];
            if (argument.Op is "index" or "slice")
            {
                var bounds = new Resolved[argument.Args.Length];
                for (var b = 0; b < bounds.Length; b++) bounds[b] = argument.Args[b].Op == "omitted" ? argument.Args[b] :
                    ConstantResult(await ExprAsync(argument.Args[b], meter, row: row).ConfigureAwait(false));
                args[i] = argument with { Args = bounds }; continue;
            }
            var value = await ExprAsync(argument, meter, filter && expression.Op == "and", row).ConfigureAwait(false);
            args[i] = ConstantResult(value);
            if (i == 0 && expression.Op == "and" && (!value.IsNull && !value.AsBoolean() || filter && value.IsNull)) return value;
            if (i == 0 && expression.Op == "or" && !value.IsNull && value.AsBoolean()) return value;
        }
        // Child continuations are consumed exactly once. The resident kernel sees only their
        // retained values and does not recharge this already-entered expression node.
        return Expr(expression with { Args = args, GrowthOperands = expression.Args }, meter, filter, row, entered: true);
    }
    private static Resolved ConstantResult(SqlValue value) => new("constant", value.Type, value, []);
}
