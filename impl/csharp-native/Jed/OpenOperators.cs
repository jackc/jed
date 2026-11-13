namespace Jed;

internal static class OpenOperators
{
    internal static bool TryResolve(Expression expression, Resolved left, Resolved right, out Resolved result)
    {
        result = null!;
        if (expression.Op is not ("||" or "@>" or "<@" or "&&" or "-|-" or "<<" or ">>" or "&<" or "&>" or "+" or "-" or "*")) return false;
        if (expression.Op == "||" && (left.Type.Kind == SqlKind.Array || right.Type.Kind == SqlKind.Array))
        {
            var name = left.Type.Kind == SqlKind.Array && right.Type.Kind == SqlKind.Array || left.Type.Kind == SqlKind.Array && right.Type == SqlType.Unknown || right.Type.Kind == SqlKind.Array && left.Type == SqlType.Unknown ? "array_cat" : left.Type.Kind == SqlKind.Array ? "array_append" : "array_prepend";
            return OpenResolve.TryCall(expression with { Op = "call", Text = name, Args = [new("bound", "", [], Binding: left), new("bound", "", [], Binding: right)] }, out result);
        }
        if (left.Type.Kind == SqlKind.Range || right.Type.Kind == SqlKind.Range)
        {
            var range = left.Type.Kind == SqlKind.Range ? left.Type : right.Type;
            if (expression.Op is "+" or "-" or "*")
            {
                if (left.Type == SqlType.Unknown) left = new("constant", range, SqlValue.Null(range), []);
                if (right.Type == SqlType.Unknown) right = new("constant", range, SqlValue.Null(range), []);
                if (left.Type != range || right.Type != range) throw new SqlException(SqlStates.UndefinedFunction, "range operands must have the same type");
                result = new("range_set", range, default, [left, right], expression.Op); return true;
            }
            if (expression.Op is not ("@>" or "<@" or "&&" or "-|-" or "<<" or ">>" or "&<" or "&>")) return false;
            var elementRight = expression.Op == "@>" && right.Type.Kind != SqlKind.Range;
            var elementLeft = expression.Op == "<@" && left.Type.Kind != SqlKind.Range;
            left = Resolve.Adapt(expression.Args[0], left, elementLeft ? range.Element! : range);
            right = Resolve.Adapt(expression.Args[1], right, elementRight ? range.Element! : range);
            if (elementRight ? Resolve.Common(right.Type, range.Element!) != range.Element : right.Type != range && right.Type != SqlType.Unknown) throw Errors.Type("incompatible range operand");
            if (elementLeft ? Resolve.Common(left.Type, range.Element!) != range.Element : left.Type != range && left.Type != SqlType.Unknown) throw Errors.Type("incompatible range operand");
            result = new("open_predicate", SqlType.Boolean, default, [left, right], expression.Op); return true;
        }
        if (expression.Op is "@>" or "<@" or "&&")
        {
            if (left.Type.Kind != SqlKind.Array || right.Type.Kind != SqlKind.Array || left.Type != right.Type) throw new SqlException(SqlStates.UndefinedFunction, "array operands must have the same type");
            result = new("open_predicate", SqlType.Boolean, default, [left, right], expression.Op); return true;
        }
        return false;
    }
    internal static Resolved Quantified(Expression expression)
    {
        var array = Resolve.Expr(expression.Args[1]);
        if (array.Type.Kind != SqlKind.Array) throw Errors.Type("ANY/ALL requires an array");
        var operand = Resolve.Expr(expression.Args[0], array.Type.Element);
        _ = Resolve.Common(operand.Type, array.Type.Element!);
        return new("quantified", SqlType.Boolean, default, [operand, array], expression.Text);
    }
    internal static SqlValue QuantifiedValue(string text, SqlValue left, SqlValue right, Meter meter)
    {
        if (right.IsNull) return SqlValue.Null(SqlType.Boolean);
        return QuantifiedValues(text, left, right.AsArray().Values, meter);
    }
    internal static SqlValue QuantifiedValues(string text, SqlValue left, IEnumerable<SqlValue> values, Meter meter)
    {
        var split = text.IndexOf(' '); var operation = text[..split]; var all = text[(split + 1)..] == "all"; var unknown = false;
        foreach (var element in values)
        {
            meter.Charge(Costs.OperatorEval); meter.Guard();
            if (left.IsNull || element.IsNull) { unknown = true; continue; }
            SqlValue comparison;
            if (left.Type.Kind == SqlKind.Composite) comparison = OpenExpression.Compare(operation, left, element);
            else
            {
                var order = Evaluate.Compare(left, element, meter);
                comparison = SqlValue.Boolean(operation switch { "=" => order == 0, "<>" or "!=" => order != 0, "<" => order < 0, "<=" => order <= 0, ">" => order > 0, ">=" => order >= 0, _ => throw Errors.Type("invalid quantified comparison") });
            }
            if (comparison.IsNull) unknown = true;
            else if (comparison.AsBoolean() != all) return SqlValue.Boolean(!all);
        }
        return unknown ? SqlValue.Null(SqlType.Boolean) : SqlValue.Boolean(all);
    }
}
