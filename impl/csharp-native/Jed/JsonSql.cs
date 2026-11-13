namespace Jed;

internal static class JsonSql
{
    internal static Resolved Resolve(Expression expression)
    {
        var spec = expression.Json!; var name = expression.Text;
        if (expression.Op == "is_json")
        {
            var predicateInput = Jed.Resolve.Expr(expression.Args[0], SqlType.Text);
            if (predicateInput.Type.Kind is not (SqlKind.Text or SqlKind.Json or SqlKind.Jsonb)) throw Errors.Type("IS JSON requires text or JSON");
            return new("is_json", SqlType.Boolean, default, [predicateInput], Json: spec);
        }
        var query = name is "json_value" or "json_query" or "json_exists";
        var target = SqlTypes.ParseSpec(spec.Returning ?? (name == "json_exists" ? "boolean" : name == "json_query" ? "jsonb" : name == "json" ? "json" : "text"));
        if (name == "json_query" && target.Type.Kind is not (SqlKind.Json or SqlKind.Jsonb)) throw Errors.Unsupported("JSON_QUERY non-JSON return type");
        if (name == "json" && target.Type.Kind is not (SqlKind.Json or SqlKind.Jsonb)) throw Errors.Type("invalid JSON return type");
        if (name == "json_serialize" && target.Type.Kind is not (SqlKind.Text or SqlKind.Bytea)) throw Errors.Type("invalid JSON serialization type");
        var inputHint = query ? SqlType.Jsonb : name == "json_serialize" ? SqlType.Json : SqlType.Text;
        var input = Jed.Resolve.Expr(expression.Args[0], inputHint);
        if (input.Type.Kind is not (SqlKind.Text or SqlKind.Json or SqlKind.Jsonb)) throw Errors.Type("SQL/JSON context requires text or JSON");
        var args = new List<Resolved> { input };
        if (query) args.Add(Jed.Resolve.Expr(expression.Args[1], SqlType.Jsonpath));
        var mainCount = spec.MainCount(name);
        for (var i = 2; i < mainCount; i++)
        {
            var value = Jed.Resolve.Expr(expression.Args[i]); var format = spec.PassingFormats![i - 2];
            if (format == "" && value.Type.Kind is SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz) throw Errors.Unsupported("datetime JSON path passing value");
            if (format is "utf8" or "utf16" or "utf32" && value.Type != SqlType.Bytea) throw Errors.Type("JSON ENCODING requires bytea");
            if (format is "utf16" or "utf32") throw new SqlException("22023", "invalid source encoding name");
            args.Add(value);
        }
        for (var i = mainCount; i < expression.Args.Length; i++)
        {
            ValidateDefault(expression.Args[i], true);
            var value = expression.Args[i].Op == "text" ? Jed.Resolve.Expr(new("cast", target.Type.Name(), [expression.Args[i]], Type: target)) : Jed.Resolve.Expr(expression.Args[i], target.Type);
            if (value.Type != SqlType.Jsonb && value.Type != SqlType.Boolean && !Jed.Resolve.CanCast(value.Type, target.Type)) throw new SqlException("42846", "cannot cast JSON behavior expression");
            args.Add(value);
        }
        return new("json_sql", target.Type, default, args.ToArray(), name, Target: target, Json: spec);
    }
    private static void ValidateDefault(Expression expression, bool top)
    {
        var subquery = expression.Op.Contains("subquery", StringComparison.Ordinal) || expression.Op == "initplan" || expression.Binding?.Subquery is not null;
        if (top && (subquery || expression.Op is "parameter" or "case" or "coalesce" or "and" or "or" or "not")) throw Errors.Type("invalid JSON default expression");
        if (expression.Op is "parameter") throw new SqlException("42P02", "parameter in JSON default");
        if (subquery) throw Errors.Unsupported("subquery in JSON default");
        if (expression.Op == "column" || expression.Binding?.Op is "column" or "outer" || expression.Op == "name") throw Errors.Type("column in JSON default");
        if (expression.Window is not null || expression.Op is "call_distinct" or "ordered_aggregate" || AggregateResolve.IsAggregate(expression)) throw new SqlException("42803", "aggregate in JSON default");
        foreach (var arg in expression.Args) ValidateDefault(arg, false);
    }
    internal static SqlValue Evaluate(Resolved expression, Meter meter, SqlValue[]? row)
    {
        var count = expression.Json!.MainCount(expression.Text); var args = new SqlValue[count];
        for (var i = 0; i < count; i++)
        {
            if (i == 2 && (args[0].IsNull || args[1].IsNull)) return SqlValue.Null(expression.Type);
            args[i] = Jed.Evaluate.Expr(expression.Args[i], meter, row: row);
        }
        var outcome = Execute(expression, args, meter);
        return outcome.DefaultIndex < 0 ? outcome.Value : DefaultValue(Jed.Evaluate.Expr(expression.Args[outcome.DefaultIndex], meter, row: row), expression, meter);
    }
    internal static async ValueTask<SqlValue> EvaluateAsync(Resolved expression, Meter meter, SqlValue[]? row)
    {
        var count = expression.Json!.MainCount(expression.Text); var args = new SqlValue[count];
        for (var i = 0; i < count; i++)
        {
            if (i == 2 && (args[0].IsNull || args[1].IsNull)) return SqlValue.Null(expression.Type);
            args[i] = await Jed.Evaluate.ExprAsync(expression.Args[i], meter, row: row).ConfigureAwait(false);
        }
        var outcome = Execute(expression, args, meter);
        return outcome.DefaultIndex < 0 ? outcome.Value : DefaultValue(await Jed.Evaluate.ExprAsync(expression.Args[outcome.DefaultIndex], meter, row: row).ConfigureAwait(false), expression, meter);
    }
    private readonly record struct Outcome(SqlValue Value, int DefaultIndex = -1);
    private static Outcome Execute(Resolved expression, SqlValue[] args, Meter meter)
    {
        var spec = expression.Json!; var name = expression.Text;
        if (args[0].IsNull || args.Length > 1 && args[1].IsNull) return new(SqlValue.Null(expression.Type));
        if (expression.Op == "is_json")
        {
            var valid = false;
            try
            {
                var node = args[0].Type == SqlType.Jsonb ? args[0].AsJsonb() : JsonInput.Parse(args[0].ToString(), spec.Unique, canonical: false);
                valid = (spec.Kind switch { "scalar" => JsonOps.Scalar(node), "array" => node.Kind == SqlJsonKind.Array, "object" => node.Kind == SqlJsonKind.Object, _ => true }) && (!spec.Unique || Unique(node));
            }
            catch (SqlException error) when (error.SqlState.StartsWith("22", StringComparison.Ordinal)) { }
            return new(SqlValue.Boolean(spec.Negated ? !valid : valid));
        }
        if (name == "json")
        {
            var source = args[0].ToString(); var node = JsonInput.Parse(source, spec.Unique || expression.Type == SqlType.Jsonb, canonical: false);
            if (spec.Unique && !Unique(node)) throw new SqlException("22030", "duplicate JSON object key");
            return new(expression.Type == SqlType.Json ? SqlValue.Json(source) : SqlValue.Jsonb(JsonInput.Parse(source)));
        }
        if (name == "json_serialize")
        {
            var source = args[0].ToString(); var result = expression.Type == SqlType.Bytea ? SqlValue.Bytes(TextEncoding.Utf8.GetBytes(source)) : SqlValue.Text(source);
            Jed.Evaluate.Growth(expression.Args, result, meter); return new(result);
        }
        var document = args[0].Type == SqlType.Jsonb ? args[0].AsJsonb() : JsonInput.Parse(args[0].ToString());
        var bindings = new List<JsonPathBinding>();
        for (var i = 2; i < args.Length; i++)
        {
            var key = spec.PassingNames![i - 2]; var value = Passing(args[i], spec.PassingFormats![i - 2], meter);
            if (!bindings.Any(binding => binding.Name == key)) bindings.Add(new(key, value, i - 1));
        }
        return Query(expression, document, args[1].AsJsonpath(), bindings.ToArray(), args.Length - 2, meter);
    }
    internal static SqlValue Column(Resolved expression, SqlJsonNode document, SqlJsonPath path, JsonPathBinding[] bindings, int baseCount, Meter meter)
    {
        var outcome = Query(expression, document, path, bindings, baseCount, meter, false);
        return outcome.DefaultIndex < 0 ? outcome.Value : DefaultValue(Jed.Evaluate.Expr(expression.Args[outcome.DefaultIndex], meter), expression, meter);
    }
    private static Outcome Query(Resolved expression, SqlJsonNode document, SqlJsonPath path, JsonPathBinding[] bindings, int baseCount, Meter meter, bool chargeItems = true)
    {
        var spec = expression.Json!; var name = expression.Text;
        try
        {
            var outcome = new JsonPathRun(path, document, SqlJsonNode.Object([]), meter, baseCount, bindings).Run(false, name == "json_exists", chargeItems);
            if (name == "json_exists") return new(SqlValue.Boolean(outcome.Items.Count != 0));
            var items = outcome.Items;
            if (name == "json_query" && (spec.Wrapper == "with" || spec.Wrapper == "conditional" && items.Count > 1))
            {
                var wrapped = SqlValue.Jsonb(SqlJsonNode.Array(items));
                var result = expression.Type == SqlType.Json ? SqlValue.Json(wrapped.ToString()) : wrapped;
                Jed.Evaluate.Growth(expression.Args.Take(spec.MainCount(name)).ToArray(), result, meter); return new(result);
            }
            if (items.Count == 0) return Behavior(expression, false, new("22035", "no SQL/JSON item"));
            if (items.Count != 1) throw new SqlException("22034", "more than one SQL/JSON item");
            var node = items[0];
            if (name == "json_query") return new(expression.Type == SqlType.Json ? SqlValue.Json(node.ToString()) : SqlValue.Jsonb(node));
            if (!JsonOps.Scalar(node)) throw new SqlException("2203F", "SQL/JSON scalar required");
            try { return new(Jed.Resolve.ApplyModifier(JsonRecord.Convert(node, expression.Type, meter.Temporal), expression.Target!)); }
            catch (SqlException error) when (error.SqlState is not ("22007" or "22008") && error.SqlState.StartsWith("22", StringComparison.Ordinal)) { throw new SqlException("2203A", error.Message); }
        }
        catch (SqlException error) when (error.SqlState != "22035" && error.SqlState.StartsWith("22", StringComparison.Ordinal)) { return Behavior(expression, true, error); }
    }
    private static Outcome Behavior(Resolved expression, bool error, SqlException cause)
    {
        var spec = expression.Json!; var behavior = error ? spec.Error : spec.Empty; var index = error ? spec.ErrorIndex : spec.EmptyIndex;
        if (behavior == "error") throw cause;
        if (behavior == "default") return new(default, index);
        if (behavior is "null" or "unknown") return new(SqlValue.Null(expression.Type));
        var node = behavior switch { "true" => SqlJsonNode.Boolean(true), "false" => SqlJsonNode.Boolean(false), "empty_object" => SqlJsonNode.Object([]), _ => SqlJsonNode.Array([]) };
        return new(expression.Type == SqlType.Json ? SqlValue.Json(node.ToString()) : expression.Type == SqlType.Jsonb ? SqlValue.Jsonb(node) : expression.Type == SqlType.Boolean ? SqlValue.Boolean(behavior == "true") : SqlValue.Text(node.ToString()));
    }
    private static SqlValue DefaultValue(SqlValue value, Resolved expression, Meter meter)
    {
        if (value.IsNull) return SqlValue.Null(expression.Type);
        if (value.Type == expression.Type) return Jed.Resolve.ApplyModifier(value, expression.Target!);
        if (value.Type == SqlType.Jsonb && expression.Type == SqlType.Json) return SqlValue.Json(value.AsJsonb().ToString());
        if (value.Type == SqlType.Jsonb || value.Type == SqlType.Boolean && expression.Type != SqlType.I32)
        {
            try { return Jed.Resolve.ApplyModifier(JsonRecord.Convert(value.Type == SqlType.Jsonb ? value.AsJsonb() : SqlJsonNode.Boolean(value.AsBoolean()), expression.Type, meter.Temporal), expression.Target!); }
            catch (SqlException error) { throw new SqlException("42804", "could not coerce JSON behavior expression: " + error.Message); }
        }
        return Jed.Resolve.ApplyModifier(TemporalText.Cast(value, expression.Type, meter), expression.Target!);
    }
    internal static SqlJsonNode Passing(SqlValue value, string format, Meter meter)
    {
        if (value.IsNull) return SqlJsonNode.Null;
        if (format.Length != 0 && value.Type is { Kind: SqlKind.Text or SqlKind.Bytea })
        {
            meter.Charge(Costs.OperatorEval); meter.Guard();
            string text;
            try { text = value.Type == SqlType.Text ? value.AsText() : WireReader.Utf8(value.ByteSpan); }
            catch (SqlException error) when (error.SqlState == "XX001") { throw new SqlException("22021", "invalid UTF8 byte sequence"); }
            if (text.Contains('\0')) throw new SqlException("22021", "invalid UTF8 character"); return JsonInput.Parse(text);
        }
        if (format == "" && value.Type.IsFloat())
        {
            if (!double.IsFinite(value.AsFloat())) throw Errors.Unsupported("non-finite JSON path passing value");
            var number = FloatValue.ToDecimal(value.AsFloat()); var precision = (value.Type == SqlType.F32 ? 6 : 15) - (number.IsZero ? 0 : number.Digits - number.Scale);
            return SqlJsonNode.Numeric(JsonPathRun.RoundSignificant(number, precision).TrimScale());
        }
        meter.Charge(TemporalRender.ZoneCount(value) * Costs.Timezone); meter.Guard(); return JsonImage.Node(value, meter.Temporal);
    }
    internal static bool Unique(SqlJsonNode node)
    {
        if (node.Kind == SqlJsonKind.Array) return node.Elements.All(Unique);
        if (node.Kind != SqlJsonKind.Object) return true;
        var names = new HashSet<string>(StringComparer.Ordinal);
        foreach (var member in node.Members) if (!names.Add(member.Key) || !Unique(member.Value)) return false;
        return true;
    }
}
