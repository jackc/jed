namespace Jed;

internal static class Evaluate
{
    internal static SqlValue Expr(Resolved expression, Meter meter, bool filter = false)
    {
        meter.Guard();
        if (expression.Op == "constant") return expression.Constant;
        meter.Charge(Costs.OperatorEval);
        var left = Expr(expression.Args[0], meter, filter && expression.Op == "and");
        switch (expression.Op)
        {
            case "cast": return Resolve.Cast(left, expression.Type);
            case "is_null": return SqlValue.Boolean(left.IsNull);
            case "is_not_null": return SqlValue.Boolean(!left.IsNull);
            case "not": return left.IsNull ? SqlValue.Null(SqlType.Boolean) : SqlValue.Boolean(!left.AsBoolean());
            case "neg":
                if (left.IsNull) return SqlValue.Null(expression.Type);
                try { return SqlValue.Integer(checked(-left.AsInteger()), expression.Type); }
                catch (OverflowException) { throw Errors.Overflow(); }
            case "and":
                if ((!left.IsNull && !left.AsBoolean()) || (filter && left.IsNull)) return left;
                break;
            case "or":
                if (!left.IsNull && left.AsBoolean()) return left;
                break;
        }
        var right = Expr(expression.Args[1], meter, filter && expression.Op == "and");
        if (expression.Op == "and")
        {
            if (!right.IsNull && !right.AsBoolean()) return right;
            return left.IsNull || right.IsNull ? SqlValue.Null(SqlType.Boolean) : SqlValue.Boolean(true);
        }
        if (expression.Op == "or")
        {
            if (!right.IsNull && right.AsBoolean()) return right;
            return left.IsNull || right.IsNull ? SqlValue.Null(SqlType.Boolean) : SqlValue.Boolean(false);
        }
        if (expression.Op is "distinct" or "not_distinct")
        {
            var equal = left.IsNull || right.IsNull ? left.IsNull == right.IsNull : Compare(left, right, meter) == 0;
            return SqlValue.Boolean(expression.Op == "not_distinct" ? equal : !equal);
        }
        if (left.IsNull || right.IsNull) return SqlValue.Null(expression.Type);
        if (expression.Op is "+" or "-" or "*" or "/" or "%")
        {
            var a = left.AsInteger(); var b = right.AsInteger();
            if (b == 0 && expression.Op is "/" or "%") throw new SqlException(SqlStates.DivisionByZero, "division by zero");
            try
            {
                var result = expression.Op switch
                {
                    "+" => checked(a + b), "-" => checked(a - b), "*" => checked(a * b),
                    "/" => checked(a / b), "%" => b == -1 ? 0 : a % b,
                    _ => throw new InvalidOperationException()
                };
                return SqlValue.Integer(result, expression.Type);
            }
            catch (OverflowException) { throw Errors.Overflow(); }
        }
        var order = Compare(left, right, meter);
        return SqlValue.Boolean(expression.Op switch
        {
            "=" => order == 0, "<>" or "!=" => order != 0,
            "<" => order < 0, "<=" => order <= 0, ">" => order > 0, ">=" => order >= 0,
            _ => throw new InvalidOperationException($"unresolved operation {expression.Op}")
        });
    }

    private static int Compare(SqlValue left, SqlValue right, Meter meter)
    {
        if (left.Type.IsInteger()) return left.AsInteger().CompareTo(right.AsInteger());
        if (left.Type == SqlType.Boolean) return left.AsBoolean().CompareTo(right.AsBoolean());
        var a = left.AsText(); var b = right.AsText();
        var length = Math.Min(left.TextBytes, right.TextBytes);
        meter.Charge(Math.Max(0, length - 1) * Costs.VarlenCompare);
        meter.Guard();
        // Valid Unicode scalar order equals UTF-8 byte order. Iterate scalars
        // without allocating encoded copies or using UTF-16 ordinal comparison.
        var ae = a.EnumerateRunes(); var be = b.EnumerateRunes();
        while (true)
        {
            var hasA = ae.MoveNext(); var hasB = be.MoveNext();
            if (!hasA || !hasB) return hasA.CompareTo(hasB);
            var result = ae.Current.Value.CompareTo(be.Current.Value);
            if (result != 0) return result;
        }
    }
}
