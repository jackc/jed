using System.Globalization;

namespace Jed;

internal sealed record Resolved(string Op, SqlType Type, SqlValue Constant, Resolved[] Args, string Text = "", TypeSpec? Target = null, FunctionFact? Function = null, int Slot = -1, SubqueryCell? Subquery = null, OuterBinding? Outer = null, Resolved[]? GrowthOperands = null)
{
    internal bool NeedsAsync { get; } = Op is "column" or "outer" or "correlated" or "in_correlated" or "quantified_correlated" || Args.Any(argument => argument.NeedsAsync);
}
internal sealed record Projection(Resolved Expression, string Name);
internal sealed record SortPlan(Resolved Expression, bool Descending, bool NullsFirst);
internal sealed record SelectPlan(Projection[] Projections, Resolved? Filter, long? Limit, long Offset, bool Empty,
    SortPlan[]? Order = null, bool Distinct = false, GroupPlan? Group = null, bool StreamDistinct = false, bool ChargeRows = true, WindowStage? Windows = null, bool WindowPrefix = false);
internal sealed record BoundColumn(string Name, SqlType Type, string? Qualifier = null, bool Hidden = false, OuterBinding? Outer = null, int Level = 0);

internal static partial class Resolve
{
    private static Resolved Constant(SqlValue value) => new("constant", value.Type, value, []);
    private static Resolved Node(string op, SqlType type, params Resolved[] args) => new(op, type, default, args);
    private static bool Adaptable(Expression expression) => expression.Op is "integer" or "decimal" or "text" or "null" or "array" or "row" or "parameter";

    internal static SelectPlan Select(SelectSyntax syntax, BoundColumn[]? columns = null)
    {
        columns ??= [];
        if (syntax.SetQuery && syntax.Order is { } setOrder)
            foreach (var key in setOrder)
            {
                if (key.Expression.Op == "name" && key.Expression.Text.Contains('.')) throw new SqlException(SqlStates.UndefinedTable, "set result has no relation qualifier");
                if (key.Expression.Op is not ("integer" or "name")) throw Errors.Unsupported("set ORDER BY requires result column names");
            }
        var expanded = new List<SelectItem>();
        foreach (var item in syntax.Items)
        {
            if (item.Expression.Op != "star") { expanded.Add(item); continue; }
            if (columns.Length == 0) throw Errors.Syntax("SELECT * with no tables specified");
            if (item.Expression.Text.Length != 0 && !columns.Any(column => column.Outer is null && column.Qualifier == item.Expression.Text))
                throw new SqlException(SqlStates.UndefinedTable, "relation qualifier does not exist");
            for (var slot = 0; slot < columns.Length; slot++)
            {
                var column = columns[slot];
                if (column.Outer is null && (item.Expression.Text.Length == 0 ? !column.Hidden : item.Expression.Text == column.Qualifier))
                    expanded.Add(new(new("bound", column.Name, [], Binding: new("column", column.Type, default, [], Slot: slot)), null));
            }
        }
        syntax = syntax with { Items = expanded.Select(i => i with { Expression = Bind(i.Expression, columns) }).ToArray(),
            Filter = syntax.Filter is null ? null : Bind(syntax.Filter, columns) };
        syntax = WindowResolve.Expand(syntax, columns);
        var grouping = AggregateResolve.Prepare(syntax, columns);
        var windows = WindowResolve.Prepare(grouping.Syntax, columns, grouping.Plan);
        syntax = windows.Syntax;
        var projections = syntax.Items.Select(item => new Projection(Expr(item.Expression), item.Alias ?? OutputName(item.Expression))).ToArray();
        var filter = syntax.Filter is null ? null : Expr(syntax.Filter);
        if (filter is not null && filter.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown))
            throw Errors.Type("WHERE requires boolean");
        var order = (syntax.Order ?? []).Select(item =>
        {
            Resolved expression;
            if (item.Expression.Op == "integer")
            {
                if (!int.TryParse(item.Expression.Text, out var ordinal) || ordinal < 1 || ordinal > projections.Length)
                    throw new SqlException(SqlStates.InvalidColumnReference, "ORDER BY position outside select list");
                expression = projections[ordinal - 1].Expression;
            }
            else
            {
                var named = item.Expression.Op == "name" ? projections.Where(p => p.Name == item.Expression.Text).ToArray() : [];
                if (named.Length > 1) throw new SqlException(SqlStates.AmbiguousColumn, "ambiguous ORDER BY name");
                expression = named.Length == 1 ? named[0].Expression : Expr(Bind(item.Expression, columns));
            }
            return new SortPlan(expression, item.Descending, item.NullsFirst ?? item.Descending);
        }).ToArray();
        if (syntax.Distinct && order.Any(key => !projections.Any(p => Equivalent(p.Expression, key.Expression))))
            throw new SqlException(SqlStates.InvalidColumnReference, "DISTINCT ORDER BY expression must appear in select list");
        return new(projections, filter, syntax.Limit, syntax.Offset, filter is not null && EmptyFilter(filter), order, syntax.Distinct, grouping.Plan, ChargeRows: !syntax.SetQuery, Windows: windows.Stage);
    }
    internal static bool Equivalent(Resolved left, Resolved right) => left.Op == right.Op && left.Type == right.Type &&
        left.Slot == right.Slot && left.Outer == right.Outer && ReferenceEquals(left.Subquery, right.Subquery) && left.Text == right.Text && (left.Op != "constant" || left.Constant.ToString() == right.Constant.ToString()) &&
        left.Args.Length == right.Args.Length && left.Args.Zip(right.Args).All(pair => Equivalent(pair.First, pair.Second));

    internal static Expression Bind(Expression expression, BoundColumn[] columns)
    {
        if (expression.Op == "name")
        {
            var dot = expression.Text.LastIndexOf('.');
            var name = dot < 0 ? expression.Text : expression.Text[(dot + 1)..];
            var qualifier = dot < 0 ? null : expression.Text[..dot];
            var matches = columns.Select((column, slot) => (column, slot)).Where(pair => pair.column.Name == name &&
                (qualifier is null ? !pair.column.Hidden : pair.column.Qualifier == qualifier)).ToArray();
            if (matches.Length > 0) { var level = matches.Min(match => match.column.Level); matches = matches.Where(match => match.column.Level == level).ToArray(); }
            if (qualifier is not null && !columns.Any(column => column.Qualifier == qualifier)) throw new SqlException(SqlStates.UndefinedTable, $"relation does not exist: {qualifier}");
            if (matches.Length == 0) throw new SqlException(SqlStates.UndefinedColumn, $"column does not exist: {expression.Text}");
            if (matches.Length > 1) throw new SqlException(SqlStates.AmbiguousColumn, $"ambiguous column: {expression.Text}");
            if (matches[0].column.Outer is { } reference) reference.Frame.Reference(reference.Slot);
            return expression with { Op = "bound", Binding = matches[0].column.Outer is { } outer ? new("outer", matches[0].column.Type, default, [], Outer: outer) : new("column", matches[0].column.Type, default, [], Slot: matches[0].slot) };
        }
        return expression with { Args = expression.Args.Select(arg => Bind(arg, columns)).ToArray(), Predicate = expression.Predicate is null ? null : Bind(expression.Predicate, columns), AggregateOrder = expression.AggregateOrder?.Select(order => order with { Expression = Bind(order.Expression, columns) }).ToArray() };
    }
    private static string OutputName(Expression expression) => expression.Op switch
    {
        "call" => expression.Text,
        "extract" => "extract",
        "cast" or "subscript" => OutputName(expression.Args[0]),
        "name" or "bound" => expression.Text.Split('.')[^1],
        _ => "?column?"
    };

    // planner.md §3: a literal FALSE/NULL conjunct proves the relation empty.
    // Resolve every operand before this rewrite so it cannot hide type errors.
    internal static bool EmptyFilter(Resolved expression) =>
        expression.Op == "initplan" && expression.Subquery!.Initialized && (expression.Subquery.Value.IsNull || expression.Type == SqlType.Boolean && !expression.Subquery.Value.AsBoolean()) ||
        expression.Op == "in_init" && expression.Subquery!.Initialized && expression.Subquery.Values.Count == 0 ||
        expression.Op == "constant" && (expression.Constant.IsNull || expression.Type == SqlType.Boolean && !expression.Constant.AsBoolean()) ||
        expression.Op == "and" && expression.Args.Any(EmptyFilter);

    internal static Resolved Expr(Expression expression, SqlType? hint = null)
    {
        hint ??= SqlType.Unknown;
        switch (expression.Op)
        {
            case "parameter": return expression.Parameter?.Resolve(hint) ?? throw new SqlException(SqlStates.UndefinedParameter, "unbound parameter");
            case "at_time_zone":
            {
                var source = Expr(expression.Args[0]); var zone = Expr(expression.Args[1], SqlType.Text);
                if (source.Type.Kind is not (SqlKind.Timestamp or SqlKind.Timestamptz) || zone.Type != SqlType.Text) throw new SqlException(SqlStates.UndefinedFunction, "AT TIME ZONE requires timestamp and text");
                return new("at_time_zone", source.Type == SqlType.Timestamp ? SqlType.Timestamptz : SqlType.Timestamp, default, [source, zone]);
            }
            case "quantified": return OpenOperators.Quantified(expression);
            case "array": case "row": case "subscript": case "field": return OpenExpression.Resolve(expression, hint);
            case "initplan": case "bound": return expression.Binding!;
            case "quantified_subquery":
            {
                var input = Expr(expression.Args[0]); var values = Expr(expression.Args[1]);
                try { _ = Common(input.Type, values.Type); }
                catch (SqlException) { throw new SqlException(SqlStates.UndefinedFunction, "quantified operands are not comparable"); }
                return new(values.Subquery!.IsCorrelated ? "quantified_correlated" : "quantified_init", SqlType.Boolean, default, [input], expression.Text, Subquery: values.Subquery);
            }
            case "in_subquery":
            {
                var input = Expr(expression.Args[0]); var values = Expr(expression.Args[1]);
                _ = Common(input.Type, values.Type);
                return new(values.Subquery!.IsCorrelated ? "in_correlated" : "in_init", SqlType.Boolean, default, [input], Subquery: values.Subquery);
            }
            case "extract":
            {
                var source = Expr(expression.Args[0]);
                TemporalFunctions.ValidateExtract(expression.Text, source.Type);
                return new("extract", SqlType.Decimal, default, [source], expression.Text);
            }
            case "integer":
                if (hint.IsFloat()) return Constant(SqlValue.Float(FloatValue.Parse(expression.Text, hint == SqlType.F32), hint));
                if (!long.TryParse(expression.Text, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var integer))
                    throw Errors.Overflow();
                return Constant(hint == SqlType.Decimal ? SqlValue.Decimal(ExactDecimal.FromInt64(integer)) : SqlValue.Integer(integer, hint.IsInteger() ? hint : SqlType.I64));
            case "decimal": return Constant(hint.IsFloat() ? SqlValue.Float(FloatValue.Parse(expression.Text, hint == SqlType.F32), hint) : SqlValue.Decimal(ExactDecimal.Parse(expression.Text)));
            case "boolean": return Constant(SqlValue.Boolean(expression.Text == "true"));
            case "text":
                if (hint == SqlType.Date && TemporalText.IsClockDate(expression.Text)) return new("stable_date", SqlType.Date, default, [Constant(SqlValue.Text(expression.Text))]);
                return Constant(hint == SqlType.Unknown ? SqlValue.Text(expression.Text) : ParseLiteral(expression.Text, hint, expression.Context));
            case "null": return Constant(SqlValue.Null(hint));
            case "name": throw new SqlException(SqlStates.UndefinedColumn, $"column does not exist: {expression.Text}");
            case "cast":
            {
                var targetSpec = expression.Type ?? SqlTypes.ParseSpec(expression.Text);
                var target = targetSpec.Type;
                var source = expression.Args[0];
                if (source.Op == "null" && target.Kind is SqlKind.Array or SqlKind.Composite or SqlKind.Range) return Constant(SqlValue.Null(target));
                if (source.Op == "text" && target == SqlType.Date && TemporalText.IsClockDate(source.Text)) return new("stable_date", target, default, [Constant(SqlValue.Text(source.Text))]);
                if (source.Op == "text" && target == SqlType.Interval) return Constant(SqlValue.Interval(IntervalInput.Parse(source.Text, targetSpec.IntervalFields, targetSpec.Precision)));
                if (source.Op == "text" && target.Kind != SqlKind.Composite) return Constant(ApplyModifier(ParseLiteral(source.Text, target, expression.Context), targetSpec));
                if (source.Op is "row" or "array")
                {
                    var constructed = Expr(source, target);
                    return new("cast", target, default, [constructed], Target: targetSpec);
                }
                var inner = Expr(source, source.Op == "parameter" ? target : target == SqlType.Boolean ? SqlType.I32 : target.IsFloat() && source.Op is "integer" or "decimal" ? target : SqlType.Unknown);
                if (target.Kind == SqlKind.Range && inner.Type == target) return inner;
                if (!CanCast(inner.Type, target)) throw Errors.Type($"cannot cast {inner.Type.Name()} to {target.Name()}");
                // Text identity is folded; numeric casts remain runtime nodes even
                // for constants, preserving overflow timing and exact costs.
                if (target == SqlType.Text && inner.Type == SqlType.Text && targetSpec.Length is null) return inner;
                return new("cast", target, default, [inner], Target: targetSpec);
            }
            case "call": case "call_distinct":
                if (AggregateResolve.IsAggregate(expression)) throw new SqlException(SqlStates.GroupingError, "aggregate outside grouping context");
                if (expression.Op == "call_distinct" || expression.Predicate is not null) throw new SqlException(SqlStates.WrongObjectType, "scalar function cannot have aggregate decorations");
                return FunctionResolve.Call(expression);
            case "case":
            {
                var args = new Resolved[expression.Args.Length];
                var results = new List<Expression>();
                for (var i = 0; i < args.Length - 1; i += 2)
                {
                    args[i] = Expr(expression.Args[i]); BooleanOperand(args[i]); results.Add(expression.Args[i + 1]);
                }
                results.Add(expression.Args[^1]);
                var unified = Unify(results.ToArray());
                for (var i = 0; i < unified.Length - 1; i++) args[i * 2 + 1] = unified[i];
                args[^1] = unified[^1];
                return Node("case", unified[0].Type, args);
            }
            case "in":
            {
                Resolved? combined = null;
                for (var i = 1; i < expression.Args.Length; i++)
                {
                    var equal = Expr(new("=", "", [expression.Args[0], expression.Args[i]]));
                    combined = combined is null ? equal : Node("or", SqlType.Boolean, combined, equal);
                }
                return combined ?? throw Errors.Syntax("empty IN list");
            }
            case "is_boolean":
            {
                var inner = Expr(expression.Args[0]); BooleanOperand(inner);
                return new("is_boolean", SqlType.Boolean, default, [inner], expression.Text);
            }
            case "neg": case "pos":
            {
                var inner = Expr(expression.Args[0], hint);
                if (!inner.Type.IsNumeric() && inner.Type != SqlType.Interval && inner.Type != SqlType.Unknown) throw Errors.Type("unary arithmetic requires numeric input");
                return expression.Op == "pos" ? inner : Node("neg", inner.Type, inner);
            }
            case "not":
            {
                var inner = Expr(expression.Args[0]);
                BooleanOperand(inner);
                return Node("not", SqlType.Boolean, inner);
            }
            case "is_null": case "is_not_null":
                return Node(expression.Op, SqlType.Boolean, Expr(expression.Args[0]));
            case "and": case "or":
            {
                var left = Expr(expression.Args[0], SqlType.Boolean);
                var right = Expr(expression.Args[1], SqlType.Boolean);
                BooleanOperand(left); BooleanOperand(right);
                return Node(expression.Op, SqlType.Boolean, left, right);
            }
            default:
            {
                Resolved left, right;
                try { (left, right) = Pair(expression.Args[0], expression.Args[1], expression.Op); }
                catch (SqlException error) when (error.SqlState == SqlStates.CannotCoerce && expression.Op is "=" or "<>" or "<" or "<=" or ">" or ">=" or "distinct" or "not_distinct" && expression.Args.Any(argument => argument.Op == "row"))
                { throw Errors.Type("incompatible row comparison fields"); }
                if (JsonResolve.TryBinary(expression, left, right, out var jsonOperator)) return jsonOperator;
                if (OpenOperators.TryResolve(expression, left, right, out var openOperator)) return openOperator;
                if (expression.Op is "*" or "/" && left.Type == SqlType.Interval && expression.Args[1].Op == "null") right = Constant(SqlValue.Null(SqlType.I64));
                if (expression.Op == "*" && right.Type == SqlType.Interval && expression.Args[0].Op == "null") left = Constant(SqlValue.Null(SqlType.I64));
                var arithmetic = expression.Op is "+" or "-" or "*" or "/" or "%" or "^";
                var temporalType = TemporalExpression.Result(expression.Op, left.Type, right.Type);
                if (temporalType is not null) return Node(expression.Op, temporalType, left, right);
                if (expression.Op is "=" or "<>" or "!=" or "<" or "<=" or ">" or ">=" or "distinct" or "not_distinct" && left.Type.Kind == SqlKind.Array && right.Type.Kind == SqlKind.Array &&
                    left.Type.Element != right.Type.Element && !(left.Type.Element!.IsInteger() && right.Type.Element!.IsInteger())) throw Errors.Type("incompatible array comparison types");
                var common = Common(left.Type, right.Type);
                if (common.Kind is SqlKind.Array or SqlKind.Composite)
                {
                    if (left.Type != common) left = Node("promote", common, left);
                    if (right.Type != common) right = Node("promote", common, right);
                }
                if (common == SqlType.F64)
                {
                    if (left.Type == SqlType.F32) left = Node("float_widen", common, left);
                    if (right.Type == SqlType.F32) right = Node("float_widen", common, right);
                }
                if (arithmetic && !common.IsNumeric() && common != SqlType.Unknown)
                    throw Errors.Type("arithmetic requires numeric operands");
                if (expression.Op is "||" or "like" && common.Kind is not (SqlKind.Text or SqlKind.Bytea or SqlKind.Unknown)) throw Errors.Type("text operands required");
                return Node(expression.Op, arithmetic || expression.Op == "||" ? common : SqlType.Boolean, left, right);
            }
        }
    }

    internal static Resolved Adapt(Expression source, Resolved resolved, SqlType hint)
    {
        if (source.Op is "integer" or "decimal" or "text" or "null" or "parameter") return Expr(source, hint);
        if (source.Op == "array" && hint.Kind == SqlKind.Array && resolved.Op == "array")
        {
            var args = source.Args.Select((argument, index) => Adapt(argument, resolved.Args[index], resolved.Text == "nested" ? hint : hint.Element!)).ToArray();
            return resolved with { Type = hint, Args = args };
        }
        if (source.Op == "row" && hint.Kind == SqlKind.Composite && resolved.Op == "row" && source.Args.Length == hint.Fields.Count)
            return resolved with { Type = hint, Args = source.Args.Select((argument, index) => Adapt(argument, resolved.Args[index], hint.Fields[index].Type)).ToArray() };
        return resolved;
    }
    internal static (Resolved, Resolved) Pair(Expression left, Expression right, string? operation = null)
    {
        if (operation is "like" or "ilike" or "~" or "~*" or "!~" or "!~*") return (Expr(left, SqlType.Text), Expr(right, SqlType.Text));
        if (operation == "||" && left.Op == "parameter" && right.Op == "parameter") return (Expr(left, SqlType.Text), Expr(right, SqlType.Text));
        if (left.Op == "parameter" && Adaptable(right))
        {
            var sibling = Expr(right); var target = right.Op == "parameter" || sibling.Type == SqlType.Unknown ? SqlType.I64 : sibling.Type;
            return (Expr(left, target), right.Op == "parameter" ? Expr(right, target) : sibling);
        }
        // Resolve a concrete sibling once, then adapt only literal operands.
        if (Adaptable(left) && (!Adaptable(right) || left.Op == "integer" && right.Op == "decimal"))
        {
            var resolvedRight = Expr(right);
            var target = JsonResolve.OperandHint(operation, resolvedRight.Type, true) ?? (operation == "+" && left.Op is "text" or "null" && resolvedRight.Type.Kind is SqlKind.Timestamp or SqlKind.Timestamptz ? SqlType.Interval : resolvedRight.Type);
            return (Expr(left, target), resolvedRight);
        }
        var resolvedLeft = Expr(left);
        var rightTarget = JsonResolve.OperandHint(operation, resolvedLeft.Type, false) ?? (operation == "+" && right.Op is "text" or "null" && resolvedLeft.Type.Kind is SqlKind.Timestamp or SqlKind.Timestamptz ? SqlType.Interval : resolvedLeft.Type);
        return (resolvedLeft, Expr(right, rightTarget));
    }
    internal static SqlType Common(SqlType left, SqlType right)
    {
        if (left == SqlType.Unknown) return right;
        if (right == SqlType.Unknown) return left;
        if (left == right) return left;
        if (left.Kind is SqlKind.Array or SqlKind.Composite or SqlKind.Range || right.Kind is SqlKind.Array or SqlKind.Composite or SqlKind.Range) return OpenValue.Common(left, right);
        if (left.IsInteger() && right.IsInteger()) return SqlTypes.Fact(left).Rank >= SqlTypes.Fact(right).Rank ? left : right;
        if (left == SqlType.Decimal && right.IsInteger() || right == SqlType.Decimal && left.IsInteger()) return SqlType.Decimal;
        if (left.IsFloat() && right.IsFloat()) return SqlType.F64;
        throw Errors.Type($"incompatible operands: {left.Name()} and {right.Name()}");
    }
    internal static Resolved[] Unify(Expression[] expressions)
    {
        var cache = new Resolved?[expressions.Length];
        var common = SqlType.Unknown;
        for (var i = 0; i < expressions.Length; i++)
        {
            if (Adaptable(expressions[i])) continue;
            cache[i] = Expr(expressions[i]); common = Common(common, cache[i]!.Type);
        }
        var values = expressions.Select((expression, index) => cache[index] ?? Expr(expression, common)).ToArray();
        common = values.Aggregate(common, (type, value) => Common(type, value.Type));
        if (common == SqlType.Unknown) common = SqlType.Text;
        for (var i = 0; i < values.Length; i++) if (expressions[i].Op == "parameter" && common != SqlType.Unknown) values[i] = Expr(expressions[i], common);
        return values.Select(v => v.Type == common ? v : Node(v.Type == SqlType.F32 && common == SqlType.F64 ? "float_widen" : "promote", common, v)).ToArray();
    }
    private static void BooleanOperand(Resolved value)
    {
        if (value.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("logical operation requires boolean operands");
    }
    internal static bool CanCast(SqlType from, SqlType to) => from == SqlType.Unknown || from == to || from == SqlType.Text && to.Kind is SqlKind.Json or SqlKind.Jsonb or SqlKind.Jsonpath || from.Kind is SqlKind.Json or SqlKind.Jsonb && to.Kind is SqlKind.Json or SqlKind.Jsonb || CastCatalog.Pairs.Contains((from, to)) || OpenValue.CanCast(from, to);

    private static SqlValue ParseLiteral(string text, SqlType target, TemporalContext? context) => context is null ? ParseText(text, target) : TemporalText.Parse(text, target, context);
    internal static SqlValue ParseText(string text, SqlType target)
    {
        if (target == SqlType.Jsonpath) return SqlValue.Jsonpath(SqlJsonPath.Parse(text));
        if (target.Kind is SqlKind.Array or SqlKind.Range or SqlKind.Composite) return OpenInput.Parse(text, target);
        if (target == SqlType.Json) return SqlValue.Json(text);
        if (target == SqlType.Jsonb) return SqlValue.Jsonb(JsonInput.Parse(text));
        if (target == SqlType.Text) return SqlValue.Text(text);
        if (target == SqlType.Decimal) return SqlValue.Decimal(ExactDecimal.Parse(text));
        if (target == SqlType.Date) return SqlValue.Date(TemporalInput.ParseDate(text));
        if (target.Kind is SqlKind.Timestamp or SqlKind.Timestamptz) return SqlValue.Timestamp(TemporalInput.ParseTimestamp(text, target == SqlType.Timestamptz), target == SqlType.Timestamptz);
        if (target == SqlType.Interval) return SqlValue.Interval(IntervalInput.Parse(text));
        if (target.IsFloat()) return SqlValue.Float(FloatValue.Parse(text, target == SqlType.F32), target);
        if (target == SqlType.Bytea) return SqlValue.OwnedBytes(Bytea.Parse(text));
        if (target == SqlType.Uuid) return SqlValue.OwnedBytes(Uuid.Parse(text), SqlType.Uuid);
        var input = text.Trim(' ', '\t', '\r', '\n', '\f', '\v');
        if (target.IsInteger())
        {
            return SqlValue.Integer(IntegerInput.ParseText(input, target), target);
        }
        if (target == SqlType.Boolean)
        {
            var lower = TextEncoding.FoldName(input);
            if (lower == "1") return SqlValue.Boolean(true);
            if (lower == "0") return SqlValue.Boolean(false);
            if (lower.Length > 0)
            {
                var yes = "true".StartsWith(lower, StringComparison.Ordinal) || "yes".StartsWith(lower, StringComparison.Ordinal) || "on".StartsWith(lower, StringComparison.Ordinal);
                var no = "false".StartsWith(lower, StringComparison.Ordinal) || "no".StartsWith(lower, StringComparison.Ordinal) || "off".StartsWith(lower, StringComparison.Ordinal);
                if (yes != no) return SqlValue.Boolean(yes);
            }
            throw new SqlException(SqlStates.InvalidTextRepresentation, "invalid boolean input");
        }
        throw Errors.Unsupported($"text input for {target.Name()}");
    }

    internal static SqlValue Cast(SqlValue value, SqlType target)
    {
        if (value.IsNull) return SqlValue.Null(target);
        if (value.Type == target) return value;
        if (value.Type.Kind is SqlKind.Array or SqlKind.Composite or SqlKind.Range || target.Kind is SqlKind.Array or SqlKind.Composite or SqlKind.Range) return OpenValue.Cast(value, target);
        if (value.Type == SqlType.Json && target == SqlType.Jsonb) return SqlValue.Jsonb(JsonInput.Parse(value.AsJsonText()));
        if (value.Type == SqlType.Jsonb && target == SqlType.Json) return SqlValue.Json(value.AsJsonb().ToString());
        if (target == SqlType.Text) return SqlValue.Text(value.ToString());
        if (value.Type == SqlType.Text) return ParseText(value.AsText(), target);
        if (value.Type == SqlType.Date && target.Kind is SqlKind.Timestamp or SqlKind.Timestamptz)
            return SqlValue.Timestamp(TemporalValue.DateToTimestamp(value.AsDate()), target == SqlType.Timestamptz);
        if (value.Type.Kind is SqlKind.Timestamp or SqlKind.Timestamptz)
        {
            if (target == SqlType.Date) return SqlValue.Date(TemporalValue.TimestampToDate(value.AsTimestamp()));
            if (target.Kind is SqlKind.Timestamp or SqlKind.Timestamptz) return SqlValue.Timestamp(value.AsTimestamp(), target == SqlType.Timestamptz);
        }
        if (target.IsFloat())
        {
            var floating = value.Type.IsFloat() ? value.AsFloat() : value.Type.IsInteger() ?
                FloatValue.FromInteger(value.AsInteger(), target == SqlType.F32) : FloatValue.Parse(value.AsDecimal().ToString(), target == SqlType.F32);
            return SqlValue.Float(floating, target);
        }
        if (value.Type.IsFloat())
        {
            if (target == SqlType.Decimal) return SqlValue.Decimal(FloatValue.ToDecimal(value.AsFloat()));
            if (target.IsInteger()) return SqlValue.Integer(FloatValue.ToInteger(value.AsFloat()), target);
        }
        if (value.Type.Kind is SqlKind.Bytea or SqlKind.Uuid && target.Kind is SqlKind.Bytea or SqlKind.Uuid)
        {
            if (target == SqlType.Uuid && value.ByteSpan.Length != 16) throw new SqlException(SqlStates.InvalidParameterValue, "UUID requires 16 bytes");
            return SqlValue.OwnedBytes(value.AsBytes(), target);
        }
        if (target == SqlType.Decimal && value.Type.IsInteger()) return SqlValue.Decimal(ExactDecimal.FromInt64(value.AsInteger()));
        if (value.Type == SqlType.Decimal && target.IsInteger()) return SqlValue.Integer(value.AsDecimal().ToInt64(), target);
        if (target == SqlType.Boolean) return SqlValue.Boolean(value.AsInteger() != 0);
        if (value.Type == SqlType.Boolean) return SqlValue.Integer(value.AsBoolean() ? 1 : 0, target);
        return SqlValue.Integer(value.AsInteger(), target);
    }
    internal static SqlValue ApplyModifier(SqlValue value, TypeSpec spec)
    {
        if (value.IsNull) return value;
        if (value.Type == SqlType.Interval) return SqlValue.Interval(IntervalInput.Adjust(value.AsInterval(), spec.IntervalFields, spec.Precision));
        if (value.Type.Kind is SqlKind.Timestamp or SqlKind.Timestamptz && spec.Precision is {} timePrecision)
            return SqlValue.Timestamp(TemporalValue.RoundTimestamp(value.AsTimestamp(), timePrecision), value.Type == SqlType.Timestamptz);
        if (value.Type == SqlType.Decimal && spec.Precision is {} precision)
            return SqlValue.Decimal(value.AsDecimal().Coerce(precision, spec.Scale ?? 0));
        if (value.Type == SqlType.Text && spec.Length is {} length)
            return SqlValue.Text(string.Concat(value.AsText().EnumerateRunes().Take(length).Select(r => r.ToString())));
        return value;
    }
}
