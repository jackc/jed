namespace Jed;

internal static class OpenExpression
{
    internal static Resolved Resolve(Expression expression, SqlType hint, bool explicitCast = false)
    {
        if (expression.Op == "array")
        {
            var elementHint = hint.Kind == SqlKind.Array ? hint.Element! : SqlType.Unknown;
            if (expression.Args.Length == 0)
            {
                if (elementHint == SqlType.Unknown) throw new SqlException(SqlStates.IndeterminateDatatype, "empty array has no element type");
                return new("array", SqlType.Array(elementHint), default, []);
            }
            var nested = expression.Args.Any(arg => arg.Op == "array");
            var values = elementHint == SqlType.Unknown ? Jed.Resolve.Unify(expression.Args) :
                expression.Args.Select(arg => Jed.Resolve.Expr(arg, arg.Op == "array" ? hint : elementHint)).ToArray();
            nested |= values[0].Type.Kind == SqlKind.Array;
            if (nested && values.Any(value => value.Type.Kind != SqlKind.Array)) throw Errors.Type("array dimensions do not match");
            var element = elementHint != SqlType.Unknown ? elementHint : nested ? values[0].Type.Element! : values[0].Type;
            for (var i = 0; i < values.Length; i++)
            {
                var target = nested ? SqlType.Array(element) : element;
                if (values[i].Type != target)
                {
                    if (explicitCast)
                    {
                        if (!Jed.Resolve.CanCast(values[i].Type, target)) throw new SqlException(SqlStates.CannotCoerce, "array element cast is not supported");
                    }
                    else _ = Jed.Resolve.Common(values[i].Type, target);
                    values[i] = new("promote", target, default, [values[i]]);
                }
            }
            return new("array", SqlType.Array(element), default, values, nested ? "nested" : "");
        }
        if (expression.Op == "row")
        {
            if (hint.Kind == SqlKind.Composite && hint.Fields.Count != expression.Args.Length) throw new SqlException(SqlStates.CannotCoerce, "record field count mismatch");
            var fields = expression.Args.Select((arg, index) =>
            {
                var target = hint.Kind == SqlKind.Composite ? hint.Fields[index].Type : null;
                if (arg.Op == "text" && target?.Kind == SqlKind.Composite)
                    return Jed.Resolve.LiteralConstant(arg.Text, target, arg.Context);
                return explicitCast && target is not null && arg.Op is "row" or "array"
                    ? Resolve(arg, target, explicitCast: true) : Jed.Resolve.Expr(arg, target);
            }).ToArray();
            var type = hint.Kind == SqlKind.Composite ? hint : SqlType.Record(fields.Select((field, index) => new SqlField($"f{index + 1}", field.Type)));
            for (var i = 0; i < fields.Length; i++)
                if (fields[i].Type != type.Fields[i].Type)
                {
                    if (explicitCast)
                    {
                        if (!Jed.Resolve.CanCast(fields[i].Type, type.Fields[i].Type)) throw new SqlException(SqlStates.CannotCoerce, "record field type mismatch");
                    }
                    else
                    {
                        try { StatementExecution.Assignable(fields[i].Type, type.Fields[i].Type); }
                        catch (SqlException error) when (error.SqlState is SqlStates.DatatypeMismatch or SqlStates.CannotCoerce)
                        { throw new SqlException(SqlStates.CannotCoerce, "record field type mismatch"); }
                    }
                    fields[i] = new("promote", type.Fields[i].Type, default, [fields[i]]);
                }
            return new("row", type, default, fields);
        }
        var source = Jed.Resolve.Expr(expression.Args[0]);
        if (expression.Op == "field")
        {
            if (source.Type.Kind != SqlKind.Composite) throw new SqlException(SqlStates.WrongObjectType, "field access requires a record");
            var slot = Enumerable.Range(0, source.Type.Fields.Count).FirstOrDefault(i => source.Type.Fields[i].Name == expression.Text, -1);
            if (slot < 0) throw new SqlException(SqlStates.UndefinedColumn, "record field does not exist");
            return new("field", source.Type.Fields[slot].Type, default, [source], Slot: slot);
        }
        if (source.Type.Kind != SqlKind.Array) throw new SqlException(SqlStates.DatatypeMismatch, "subscript requires an array");
        var sliced = expression.Args.Skip(1).Any(arg => arg.Op == "slice");
        var dimensions = expression.Args.Skip(1).Select(arg =>
        {
            var bounds = arg.Args.Select(bound => bound.Op == "omitted" ? new Resolved("omitted", SqlType.I64, default, []) : Jed.Resolve.Expr(bound)).ToArray();
            if (bounds.Any(bound => !bound.Type.IsInteger() && bound.Type != SqlType.Unknown)) throw Errors.Type("array bounds must be integers");
            return new Resolved(arg.Op, SqlType.I64, default, bounds);
        }).ToArray();
        return new("subscript", sliced ? source.Type : source.Type.Element!, default, new[] { source }.Concat(dimensions).ToArray(), sliced ? "slice" : "");
    }
    internal static SqlValue Compare(string operation, SqlValue left, SqlValue right)
    {
        bool? Equal(SqlValue a, SqlValue b)
        {
            if (a.IsNull || b.IsNull) return null;
            if (a.Type.Kind != SqlKind.Composite) return OpenValue.CompareElement(a, b) == 0;
            var unknown = false; var x = a.AsComposite().Fields; var y = b.AsComposite().Fields;
            for (var i = 0; i < x.Count; i++) { var equal = Equal(x[i], y[i]); if (equal == false) return false; unknown |= equal is null; }
            return unknown ? null : true;
        }
        int? Order(SqlValue a, SqlValue b)
        {
            if (a.IsNull || b.IsNull) return null;
            if (a.Type.Kind != SqlKind.Composite) return OpenValue.CompareElement(a, b);
            var x = a.AsComposite().Fields; var y = b.AsComposite().Fields;
            for (var i = 0; i < x.Count; i++) { var order = Order(x[i], y[i]); if (order != 0) return order; }
            return 0;
        }
        if (operation is "=" or "<>" or "!=")
        {
            var equal = Equal(left, right); return equal is null ? SqlValue.Null(SqlType.Boolean) : SqlValue.Boolean(operation == "=" ? equal.Value : !equal.Value);
        }
        var compared = Order(left, right);
        if (compared is null) return SqlValue.Null(SqlType.Boolean);
        return SqlValue.Boolean(operation switch { "<" => compared < 0, "<=" => compared <= 0, ">" => compared > 0, ">=" => compared >= 0, _ => throw new InvalidOperationException("invalid row comparison") });
    }
    internal static SqlValue Evaluate(Resolved expression, Meter meter, SqlValue[]? row)
    {
        if (expression.Op == "row")
        {
            var fields = expression.Args.Select((arg, index) =>
            {
                var field = expression.Type.Fields[index];
                var value = Jed.Resolve.ApplyModifier(Jed.Evaluate.Expr(arg, meter, row: row), new(field.Type, field.Precision, field.Scale, field.Length, field.IntervalFields));
                if (value.IsNull && field.NotNull) throw new SqlException(SqlStates.NotNullViolation, "null composite field");
                return value;
            }).ToArray();
            return SqlValue.Composite(new(expression.Type, fields));
        }
        if (expression.Op == "array")
        {
            var values = expression.Args.Select(arg => Jed.Evaluate.Expr(arg, meter, row: row)).ToArray();
            if (expression.Text != "nested")
            {
                var result = SqlValue.Array(new(expression.Type.Element!, values)); Jed.Evaluate.Growth(expression.GrowthOperands ?? expression.Args, result, meter); return result;
            }
            if (values.Any(value => value.IsNull)) throw new SqlException(SqlStates.ArraySubscriptError, "NULL sub-array in constructor");
            var arrays = values.Select(value => value.AsArray()).ToArray();
            if (arrays.Skip(1).Any(array => !array.Dimensions.SequenceEqual(arrays[0].Dimensions) || !array.LowerBounds.SequenceEqual(arrays[0].LowerBounds)))
                throw new SqlException(SqlStates.ArraySubscriptError, "sub-array shapes do not match");
            if (arrays[0].Count == 0) return SqlValue.Array(new(expression.Type.Element!, []));
            var nestedResult = SqlValue.Array(new(expression.Type.Element!, arrays.SelectMany(array => array.Values), new[] { arrays.Length }.Concat(arrays[0].Dimensions), new[] { 1 }.Concat(arrays[0].LowerBounds)));
            Jed.Evaluate.Growth(expression.GrowthOperands ?? expression.Args, nestedResult, meter); return nestedResult;
        }
        var source = Jed.Evaluate.Expr(expression.Args[0], meter, row: row);
        if (expression.Op == "field") return source.IsNull ? SqlValue.Null(expression.Type) : source.AsComposite().Fields[expression.Slot];
        var bounds = expression.Args.Skip(1).Select(dimension => dimension.Args.Select(bound => bound.Op == "omitted" ? (SqlValue?)null : Jed.Evaluate.Expr(bound, meter, row: row)).ToArray()).ToArray();
        if (source.IsNull || bounds.Any(dimension => dimension.Any(bound => bound.HasValue && bound.Value.IsNull))) return SqlValue.Null(expression.Type);
        if (expression.Text != "slice") return source.AsArray().Element(bounds.Select(dimension => (long?)dimension[0]!.Value.AsInteger()).ToArray());
        var slices = bounds.Select((dimension, i) => expression.Args[i + 1].Op == "index" ? ((long?)1, dimension[0]?.AsInteger()) : (dimension[0]?.AsInteger(), dimension[1]?.AsInteger())).ToArray();
        return SqlValue.Array(source.AsArray().Slice(slices));
    }
}
