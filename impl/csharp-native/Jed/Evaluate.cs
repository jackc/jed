namespace Jed;

internal static partial class Evaluate
{
    internal static SqlValue Expr(Resolved expression, Meter meter, bool filter = false, SqlValue[]? row = null, bool entered = false)
    {
        if (!entered) meter.Guard();
        if (expression.Op == "outer") return expression.Outer!.Value;
        if (expression.Op == "initplan") return expression.Subquery!.Value;
        if (expression.Op == "in_init") return SubqueryExecution.In(expression, meter, row);
        if (expression.Op == "constant") return expression.Constant;
        if (expression.Op == "collate") return Expr(expression.Args[0], meter, filter, row);
        if (expression.Op == "column") return row is null ? throw new InvalidOperationException("column outside row") : row[expression.Slot];
        if (expression.Op == "promote") return TemporalText.Cast(Expr(expression.Args[0], meter, row: row), expression.Type, meter);
        if (!entered && !(expression.Op == "not" && expression.Text == "predicate")) meter.Charge(expression.Op == "in_list" ? (expression.Args.Length - 2) * Costs.OperatorEval : expression.HostFunction?.Cost ?? expression.Function?.Cost ?? Costs.OperatorEval);
        if (expression.Op == "host")
        {
            meter.Guard();
            var arguments = expression.Args.Select(argument => Expr(argument, meter, row: row)).ToArray();
            return HostFunctionExecution.Call(expression, arguments, meter, row);
        }
        if (expression.Op == "in_list") return LiteralMembership(expression, Expr(expression.Args[0], meter, row: row), meter);
        if (expression.Op == "quantified_init") return OpenOperators.QuantifiedValues(expression.Text, Expr(expression.Args[0], meter, row: row), expression.Subquery!.Values, meter, expression.Collation);
        if (expression.Op is "array" or "row" or "field" or "subscript") return OpenExpression.Evaluate(expression, meter, row);
        if (expression.Op == "float_widen") return Resolve.Cast(Expr(expression.Args[0], meter, row: row), expression.Type);
        if (expression.Op is "json_sql" or "is_json") return JsonSql.Evaluate(expression, meter, row);
        if (expression.Similar is not null) return SimilarFunctions.Call(expression, expression.Args.Select(argument => Expr(argument, meter, row: row)).ToArray(), meter);
        if (expression.Regex is not null)
        {
            var values = expression.Args.Select(argument => Expr(argument, meter, row: row)).ToArray();
            return RegexFunctions.Call(expression, values, meter);
        }
        if (expression.Op == "call")
        {
            var args = expression.Args.Select(a => Expr(a, meter, row: row)).ToArray();
            if (expression.ArrayArguments)
            {
                if (args[^1].IsNull && expression.Text != "format") return SqlValue.Null(expression.Type);
                args = args.Take(args.Length - 1).Concat(args[^1].IsNull ? [] : args[^1].AsArray().Values).ToArray();
            }
            if (expression.Function!.Nulls == "propagates" && args.Any(a => a.IsNull)) return SqlValue.Null(expression.Type);
            if (expression.Function.Variadic && expression.Text is "json_build_array" or "jsonb_build_array" or "json_build_object" or "jsonb_build_object") args = args[0].AsArray().Values.ToArray();
            var result = expression.Text == "width_bucket_array" ? OpenFunctions.WidthBucket(args, meter, expression.Collation) : ScalarFunctions.Call(expression.Text, args, expression.Type, meter);
            var grows = expression.Text is "encode" or "overlay" or "quote_literal" or "quote_nullable" or "quote_ident" or "like_escape" or "concat" or "similar_to_escape" or "array_append" or "array_prepend" or "array_cat" or "to_char" or "json_build_array" or "jsonb_build_array" or "json_build_object" or "jsonb_build_object" or "to_json" or "to_jsonb" or "array_to_json" or "row_to_json" or "json_object" or "jsonb_object" or "jsonb_set" or "jsonb_set_lax" or "jsonb_insert" or "jsonb_path_query_array" or "jsonb_path_query_array_tz" or "json_scalar" or "json_serialize";
            if (grows && !(expression.Text == "jsonb_set_lax" && args[2].IsNull && !args[4].IsNull && args[4].AsText() is "delete_key" or "return_target")) Growth(expression.GrowthOperands ?? expression.Args, result, meter);
            return result;
        }
        if (expression.Op == "case")
        {
            for (var i = 0; i < expression.Args.Length - 1; i += 2)
            {
                var condition = Expr(expression.Args[i], meter, row: row);
                if (!condition.IsNull && condition.AsBoolean()) return Expr(expression.Args[i + 1], meter, row: row);
            }
            return Expr(expression.Args[^1], meter, row: row);
        }
        if (expression.Op is "coalesce" or "greatest" or "least")
        {
            var best = SqlValue.Null(expression.Type);
            foreach (var argument in expression.Args)
            {
                var value = Expr(argument, meter, row: row);
                if (value.IsNull) continue;
                if (expression.Op == "coalesce") return value;
                if (best.IsNull || (expression.Op == "greatest" ? CollationRules.Compare(value, best, expression.Collation, meter) > 0 : CollationRules.Compare(value, best, expression.Collation, meter) < 0)) best = value;
            }
            return best;
        }
        var left = Expr(expression.Args[0], meter, filter && expression.Op == "and", row);
        switch (expression.Op)
        {
            case "stable_date": return TemporalText.Date(left.AsText(), meter);
            case "extract": return TemporalFunctions.Extract(expression.Text, left, meter);
            case "cast":
                var cast = TemporalText.Cast(left, expression.Type, meter);
                if ((expression.Type == SqlType.Text && (left.Type.Kind == SqlKind.Array || left.Type == SqlType.Bytea || left.Type == SqlType.Jsonb)) || expression.Type == SqlType.Json && left.Type == SqlType.Jsonb || left.Type.Kind == SqlKind.Array && left.Type.Element == SqlType.Bytea && expression.Type.Kind == SqlKind.Array && expression.Type.Element == SqlType.Text)
                    Growth(expression.GrowthOperands ?? expression.Args, cast, meter);
                if (!left.IsNull && left.Type == SqlType.Text && expression.Type == SqlType.Jsonpath)
                { meter.Charge(System.Text.Encoding.UTF8.GetByteCount(left.AsText()) * Costs.JsonpathCompile); meter.Guard(); }
                return Resolve.ApplyModifier(cast, expression.Target!);
            case "is_boolean":
                var condition = expression.Text.EndsWith("unknown", StringComparison.Ordinal) ? left.IsNull :
                    !left.IsNull && left.AsBoolean() == expression.Text.EndsWith("true", StringComparison.Ordinal);
                return SqlValue.Boolean(expression.Text.StartsWith("not_", StringComparison.Ordinal) ? !condition : condition);
            case "is_null": return SqlValue.Boolean(left.IsNull || left.Type.Kind == SqlKind.Composite && left.AsComposite().Fields.All(field => field.IsNull));
            case "is_not_null": return SqlValue.Boolean(!left.IsNull && (left.Type.Kind != SqlKind.Composite || left.AsComposite().Fields.All(field => !field.IsNull)));
            case "not": return left.IsNull ? SqlValue.Null(SqlType.Boolean) : SqlValue.Boolean(!left.AsBoolean());
            case "neg":
                if (left.IsNull) return SqlValue.Null(expression.Type);
                if (left.Type == SqlType.Interval) return SqlValue.Interval(left.AsInterval().Negate());
                if (left.Type == SqlType.Decimal) return SqlValue.Decimal(left.AsDecimal().Negate());
                if (left.Type.IsFloat()) return SqlValue.Float(-left.AsFloat(), left.Type);
                try { return SqlValue.Integer(checked(-left.AsInteger()), expression.Type); }
                catch (OverflowException) { throw Errors.Overflow(); }
            case "and":
                if ((!left.IsNull && !left.AsBoolean()) || (filter && left.IsNull)) return left;
                break;
            case "or":
                if (!left.IsNull && left.AsBoolean()) return left;
                break;
        }
        var right = Expr(expression.Args[1], meter, filter && expression.Op == "and", row);
        if (expression.Op == "nullif") return !left.IsNull && !right.IsNull && Compare(left, right, meter) == 0 ? SqlValue.Null(expression.Type) : left;
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
        if (expression.Op == "at_time_zone") return TemporalFunctions.AtTimeZone(left, right, meter);
        if (expression.Op == "quantified") return OpenOperators.QuantifiedValue(expression.Text, left, right, meter, expression.Collation);
        if (expression.Op == "json_path_binary") return JsonPathFunctions.Operator(expression.Text, left, right, meter);
        if (expression.Op == "json_binary")
        {
            var result = JsonOps.Binary(expression.Text, left, right);
            if (expression.Text == "||" || JsonOps.ExtractionBuilds(expression.Text, left, right)) Growth(expression.GrowthOperands ?? expression.Args, result, meter);
            return result;
        }
        if (expression.Op == "open_predicate") return OpenFunctions.Predicate(expression.Text, left, right);
        if (expression.Op == "range_set") return OpenFunctions.RangeSet(expression.Text, left, right);
        if (left.IsNull || right.IsNull) return SqlValue.Null(expression.Type);
        if (TemporalExpression.Result(expression.Op, left.Type, right.Type) is not null)
            return TemporalExpression.Binary(expression.Op, left, right, expression.Type, meter);
        if (expression.Op == "||")
        {
            var result = expression.Type == SqlType.Bytea ? SqlValue.OwnedBytes(left.AsBytes().Concat(right.AsBytes()).ToArray()) : SqlValue.Text(left.AsText() + right.AsText());
            Growth(expression.GrowthOperands ?? expression.Args, result, meter); return result;
        }
        if (expression.Op == "like") return SqlValue.Boolean(TextFunctions.Like(left.AsText(), right.AsText()));
        if (expression.Op == "ilike") return SqlValue.Boolean(TextFunctions.Like(UnicodeData.Fold(left.AsText(), false, meter.Unicode, false), UnicodeData.Fold(right.AsText(), false, meter.Unicode, false)));
        if (expression.Op is "+" or "-" or "*" or "/" or "%" or "^")
        {
            if (expression.Type.IsFloat()) return SqlValue.Float(expression.Op == "^" ? FloatValue.Power(left.AsFloat(), right.AsFloat()) :
                FloatValue.Binary(expression.Op, left.AsFloat(), right.AsFloat(), expression.Type == SqlType.F32), expression.Type);
            if (expression.Type == SqlType.Decimal)
            {
                var aDecimal = Resolve.Cast(left, SqlType.Decimal).AsDecimal();
                var bDecimal = Resolve.Cast(right, SqlType.Decimal).AsDecimal();
                meter.Charge(aDecimal.DecimalWork(bDecimal, expression.Op)); meter.Guard();
                return SqlValue.Decimal(expression.Op switch
                {
                    "+" => aDecimal.Add(bDecimal), "-" => aDecimal.Subtract(bDecimal), "*" => aDecimal.Multiply(bDecimal),
                    "/" => aDecimal.Divide(bDecimal), "%" => aDecimal.Remainder(bDecimal), _ => throw new InvalidOperationException()
                });
            }
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
        if (left.Type.Kind == SqlKind.Composite) return OpenExpression.Compare(expression.Op, left, right);
        var order = expression.Op is "<" or "<=" or ">" or ">=" ? CollationRules.Compare(left, right, expression.Collation, meter) : Compare(left, right, meter);
        return SqlValue.Boolean(expression.Op switch
        {
            "=" => order == 0, "<>" or "!=" => order != 0,
            "<" => order < 0, "<=" => order <= 0, ">" => order > 0, ">=" => order >= 0,
            _ => throw new InvalidOperationException($"unresolved operation {expression.Op}")
        });
    }

    internal static bool Constant(Resolved expression) => !expression.ParameterUnbound && expression.Op is not ("column" or "outer" or "correlated" or "in_correlated" or "quantified_correlated" or "quantified_init" or "subquery" or "parameter_host" or "host" or "stable_date" or "initplan" or "in_init") &&
        !(expression.Op == "call" && expression.Text is "now" or "clock_timestamp" or "current_date" or "nextval" or "currval" or "setval" or "lastval" or "uuidv4" or "gen_random_uuid" or "uuidv7" or "current_setting") && expression.Args.All(Constant);
    internal static void Growth(Resolved[] operands, SqlValue result, Meter meter)
    {
        if (result.IsNull || operands.All(Constant)) return;
        meter.Charge(OpenValue.PayloadBytes(result) * Costs.ScalarByte); meter.Guard();
    }

    internal static int Compare(SqlValue left, SqlValue right, Meter? meter = null)
    {
        if (left.Type == SqlType.Jsonb) return JsonOps.Compare(left.AsJsonb(), right.AsJsonb());
        if (left.Type.Kind is SqlKind.Array or SqlKind.Composite or SqlKind.Range) return OpenValue.Compare(left, right);
        if (left.Type.IsFloat() && right.Type.IsFloat()) return FloatValue.Compare(left.AsFloat(), right.AsFloat());
        if (left.Type == SqlType.Decimal || right.Type == SqlType.Decimal)
        {
            var aDecimal = Resolve.Cast(left, SqlType.Decimal).AsDecimal();
            var bDecimal = Resolve.Cast(right, SqlType.Decimal).AsDecimal();
            meter?.Charge(aDecimal.DecimalWork(bDecimal, "compare")); meter?.Guard();
            return aDecimal.CompareTo(bDecimal);
        }
        if (left.Type == SqlType.Date) return left.AsDate().CompareTo(right.AsDate());
        if (left.Type.Kind is SqlKind.Timestamp or SqlKind.Timestamptz) return left.AsTimestamp().CompareTo(right.AsTimestamp());
        if (left.Type == SqlType.Interval) return left.AsInterval().CompareTo(right.AsInterval());
        if (left.Type.IsInteger()) return left.AsInteger().CompareTo(right.AsInteger());
        if (left.Type == SqlType.Boolean) return left.AsBoolean().CompareTo(right.AsBoolean());
        if (left.Type.Kind is SqlKind.Bytea or SqlKind.Uuid)
        {
            if (left.Type == SqlType.Bytea) { meter?.Charge(Math.Max(0, Math.Min(left.ByteSpan.Length, right.ByteSpan.Length) - 1)); meter?.Guard(); }
            return left.ByteSpan.SequenceCompareTo(right.ByteSpan);
        }
        var a = left.AsText(); var b = right.AsText();
        var length = Math.Min(a.EnumerateRunes().Count(), b.EnumerateRunes().Count());
        meter?.Charge(Math.Max(0, length - 1) * Costs.VarlenCompare);
        meter?.Guard();
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
