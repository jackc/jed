namespace Jed;

internal static partial class Evaluate
{
    private static SqlValue LiteralMembership(Resolved expression, SqlValue input, Meter meter)
    {
        // The flat representation preserves the left-deep OR chain's charges:
        // all n-1 OR nodes precede comparisons, which stop at the first TRUE.
        var unknown = input.IsNull;
        foreach (var item in expression.Args.Skip(1))
        {
            meter.Charge(Costs.OperatorEval); meter.Guard();
            if (input.IsNull || item.Constant.IsNull) unknown = true;
            else if (Compare(input, item.Constant, meter) == 0) return SqlValue.Boolean(true);
        }
        return unknown ? SqlValue.Null(SqlType.Boolean) : SqlValue.Boolean(false);
    }
}
