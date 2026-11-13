using System.Runtime.CompilerServices;

namespace Jed;

internal static class RegexSetFunctions
{
    internal static Resolved Resolve(Expression expression)
    {
        if (expression.Args.Length is not (2 or 3)) throw new SqlException(SqlStates.UndefinedFunction, "invalid regex set function arguments");
        var args = expression.Args.Select(argument => Jed.Resolve.Expr(argument, SqlType.Text)).ToArray();
        if (args.Any(argument => argument.Type != SqlType.Text)) throw new SqlException(SqlStates.UndefinedFunction, "regex set function requires text");
        var type = expression.Text == "regexp_matches" ? SqlType.Array(SqlType.Text) : SqlType.Text;
        var function = new FunctionFact(expression.Text, [], type.Name(), "propagates", 1, false, [], []);
        return CollationRules.Derive(RegexResolve.Prepare(new("call", type, default, args, expression.Text, Function: function), expression.Context?.Unicode));
    }
    internal static async IAsyncEnumerable<SqlValue[]> Rows(Resolved expression, Meter meter, [EnumeratorCancellation] CancellationToken cancellation)
    {
        meter.Charge(Costs.OperatorEval); meter.Checkpoint(cancellation);
        var values = new SqlValue[expression.Args.Length];
        for (var i = 0; i < values.Length; i++) values[i] = await Evaluate.ExprAsync(expression.Args[i], meter).ConfigureAwait(false);
        var result = new List<SqlValue>(); _ = RegexFunctions.Call(expression, values, meter, result);
        foreach (var value in result) { meter.Charge(Costs.GeneratedRow); meter.Checkpoint(cancellation); yield return [value]; }
    }
}
