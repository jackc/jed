namespace Jed;

internal static class HostFunctionResolve
{
    internal static Resolved Call(Expression expression, Func<Expression, Resolved> builtin)
    {
        var functions = expression.Context?.Environment?.Functions.Where(function =>
            function.Name == expression.Text && function.Arguments.Count == expression.Args.Length).ToArray() ?? [];
        if (functions.Length == 0 || expression.Args.Any(argument => argument.Op is "named" or "variadic")) return builtin(expression);
        var operands = expression.Args.Select(argument => Resolve.Expr(argument)).ToArray();
        if (operands.Any(argument => argument.Type == SqlType.Unknown)) return builtin(expression);
        Resolved? native = null; SqlException? nativeError = null;
        try { native = builtin(expression); }
        catch (SqlException error) when (error.SqlState is SqlStates.UndefinedFunction or SqlStates.DatatypeMismatch) { nativeError = error; }
        var nativeScore = native is null ? int.MaxValue : ScoreBuiltin(expression, operands, native);
        if (nativeScore == 0) return native!;
        var candidates = new List<(HostFunction Function, int Score)>();
        foreach (var function in functions)
        {
            var score = 0;
            for (var i = 0; i < operands.Length; i++)
            {
                var cost = Coercion(expression.Args[i], operands[i], function.Arguments[i]);
                if (cost is null) { score = int.MaxValue; break; }
                score += cost.Value;
            }
            if (score < int.MaxValue) candidates.Add((function, score));
        }
        var best = candidates.Count == 0 ? int.MaxValue : candidates.Min(candidate => candidate.Score);
        if (native is not null && nativeScore <= best) return native;
        if (best == int.MaxValue) throw nativeError ?? new SqlException(SqlStates.UndefinedFunction, "no matching host function");
        var winners = candidates.Where(candidate => candidate.Score == best).ToArray();
        if (winners.Length != 1) throw new SqlException(SqlStates.AmbiguousFunction, "ambiguous host function");
        var chosen = winners[0].Function;
        for (var i = 0; i < operands.Length; i++)
        {
            var target = chosen.Arguments[i];
            if (expression.Args[i].Op is "integer" or "decimal" && target != SqlType.Decimal)
                operands[i] = Resolve.Expr(expression.Args[i], target);
            else if (operands[i].Type != target) operands[i] = new("promote", target, default, [operands[i]]);
        }
        return new("host", chosen.Result, default, operands, chosen.Name, HostFunction: chosen);
    }
    private static int? Coercion(Expression syntax, Resolved value, SqlType target)
    {
        if (value.Type == target) return 0;
        var literal = syntax.Op == "integer" ? "integer_literal" : syntax.Op == "decimal" ? "decimal_literal" : null;
        var scores = ArgCoercionFacts.Rows.Where(row => row.To == target && (row.From == value.Type.Name() || row.From == literal)).Select(row => row.Cost).ToArray();
        if (scores.Length == 0) return null;
        if (literal is not null && target != SqlType.Decimal)
        {
            try { _ = Resolve.Expr(syntax, target); }
            catch (SqlException error) when (error.SqlState == SqlStates.NumericValueOutOfRange) { return null; }
        }
        return scores.Min();
    }
    private static int ScoreBuiltin(Expression syntax, Resolved[] initial, Resolved native)
    {
        var score = 0;
        for (var i = 0; i < initial.Length; i++)
        {
            if (native.Function is { Families.Length: > 0 } fact && FunctionResolve.Matches(initial[i].Type, fact.Families[Math.Min(i, fact.Families.Length - 1)])) continue;
            if (i >= native.Args.Length) return 0;
            var cost = Coercion(syntax.Args[i], initial[i], native.Args[i].Type);
            if (cost is null) return 1000; // Existing bare-string fallback follows numeric/host candidates.
            score += cost.Value;
        }
        return score;
    }
}
