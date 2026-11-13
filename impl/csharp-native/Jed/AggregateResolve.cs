namespace Jed;

internal sealed record AggregatePlan(string Name, Resolved? Argument, SqlType Type, bool Distinct, Resolved? Filter, Resolved[]? Extra = null, SortPlan[]? Order = null, SortPlan[]? WithinOrder = null, Resolved[]? Direct = null, CollationDerivation? Collation = null);
internal sealed record GroupPlan(Resolved[] Keys, AggregatePlan[] Aggregates, Resolved? Having, int[][]? Sets = null, int[][]? GroupingColumns = null);

internal static class AggregateResolve
{
    internal static bool IsAggregate(Expression expression) => expression.Window is null && (expression.Op == "ordered_aggregate" || expression.Op is "call" or "call_distinct" &&
        (AggregateMoments.Single(expression.Text) || AggregateMoments.Pair(expression.Text) || AggregateJson.IsJson(expression.Text) || expression.Text is "count" or "sum" or "avg" or "min" or "max" or "bool_and" or "bool_or" or "every" or "array_agg" or "string_agg" or "bit_and" or "bit_or" or "bit_xor"));
    private static bool Contains(Expression expression) => IsAggregate(expression) || expression.Op == "call" && expression.Text == "grouping" || expression.Args.Any(Contains) || expression.Window is { } window && (window.Partition.Any(Contains) || window.Order.Any(key => Contains(key.Expression)));
    internal static (SelectSyntax Syntax, GroupPlan? Plan) Prepare(SelectSyntax syntax, BoundColumn[] columns)
    {
        var keys = (syntax.GroupBy ?? []).Select(key =>
        {
            if (key.Op == "integer")
            {
                if (!int.TryParse(key.Text, out var ordinal) || ordinal < 1 || ordinal > syntax.Items.Length)
                    throw new SqlException(SqlStates.InvalidColumnReference, "GROUP BY position outside select list");
                return syntax.Items[ordinal - 1].Expression;
            }
            if (key.Op == "name" && !columns.Any(c => c.Name == key.Text))
            {
                var aliases = syntax.Items.Where(item => item.Alias == key.Text).ToArray();
                if (aliases.Length > 1) throw new SqlException(SqlStates.AmbiguousColumn, "ambiguous GROUP BY alias");
                if (aliases.Length == 1) return aliases[0].Expression;
            }
            return Resolve.Bind(key, columns);
        }).ToArray();
        var grouped = syntax.GroupingSets is not null || keys.Length != 0 || syntax.Having is not null || syntax.Items.Any(i => Contains(i.Expression)) || (syntax.DistinctOn ?? []).Any(Contains) ||
            (syntax.Order ?? []).Any(o => Contains(o.Expression)) || (syntax.Windows?.Values.AsEnumerable() ?? []).Any(window => window.Partition.Any(Contains) || window.Order.Any(key => Contains(key.Expression)));
        if (!grouped) return (syntax, null);
        var sets = syntax.GroupingSets;
        if (!syntax.GroupingExpansion && keys.Length != 0)
        {
            // Only actual base relations carry this identity; derived rows cannot
            // inherit a primary-key proof from their projected names or values.
            var expanded = keys.ToList();
            var seen = new HashSet<object>();
            for (var i = 0; i < columns.Length; i++)
            {
                var source = columns[i].BaseSource;
                if (source is null || columns[i].Outer is not null || !seen.Add(source)) continue;
                var primary = Enumerable.Range(0, columns.Length).Where(slot => ReferenceEquals(columns[slot].BaseSource, source) && columns[slot].PrimaryKey).ToArray();
                if (primary.Length == 0 || primary.Any(slot => !keys.Any(key => key.Binding is { Op: "column" } bound && bound.Slot == slot))) continue;
                for (var slot = 0; slot < columns.Length; slot++)
                    if (ReferenceEquals(columns[slot].BaseSource, source) && !expanded.Any(key => key.Binding is { Op: "column" } bound && bound.Slot == slot))
                        expanded.Add(new("bound", columns[slot].Name, [], Binding: new("column", columns[slot].Type, default, [], Slot: slot, Collation: columns[slot].Collation)));
            }
            keys = expanded.ToArray();
            if (sets is not null) sets = [Enumerable.Range(0, keys.Length).ToArray()];
        }
        var resolvedKeys = keys.Select(k => Resolve.Expr(k)).ToArray();
        foreach (var key in resolvedKeys) { Resolve.RequireOrderable(key.Type); CollationRules.Require(key, true); }
        var aggregates = new List<AggregatePlan>();
        var groupingCalls = new List<Expression>();
        void FindGrouping(Expression expression)
        {
            if (expression.Op == "call" && expression.Text == "grouping")
            {
                if (expression.Args.Length == 0 || expression.Args.Any(arg => arg.Op == "star") || expression.Window is not null) throw Errors.Syntax("invalid GROUPING call");
                if (expression.Args.Length > 31) throw new SqlException(SqlStates.ProgramLimitExceeded, "GROUPING accepts at most 31 arguments");
                var bound = Resolve.Bind(expression, columns);
                if (!groupingCalls.Any(call => Same(call, bound))) groupingCalls.Add(bound);
            }
            foreach (var arg in expression.Args) FindGrouping(arg);
            if (expression.Window is { } window) { foreach (var key in window.Partition) FindGrouping(key); foreach (var key in window.Order) FindGrouping(key.Expression); }
        }
        foreach (var item in syntax.Items) FindGrouping(item.Expression);
        if (syntax.Having is { } havingExpression) FindGrouping(havingExpression);
        foreach (var key in syntax.Order ?? []) FindGrouping(key.Expression);
        foreach (var window in (syntax.Windows ?? []).Values)
        {
            foreach (var key in window.Partition) FindGrouping(key);
            foreach (var key in window.Order) FindGrouping(key.Expression);
        }
        var groupingColumns = groupingCalls.Select(call => call.Args.Select(argument =>
        {
            var bound = Resolve.Bind(argument, columns); var slot = Array.FindIndex(keys, key => Same(bound, key));
            if (slot < 0 || Resolve.Expr(bound).Op == "outer") throw new SqlException(SqlStates.GroupingError, "GROUPING argument is not a local grouping expression");
            return slot;
        }).ToArray()).ToArray();
        Expression Rewrite(Expression expression)
        {
            for (var i = 0; i < keys.Length; i++)
                if (Same(expression, keys[i])) return new("bound", expression.Text, [], Binding: new("column", resolvedKeys[i].Type, default, [], Slot: i, Collation: resolvedKeys[i].Collation));
            if (expression.Op == "call" && expression.Text == "grouping")
            {
                var index = groupingCalls.FindIndex(call => Same(call, expression));
                return new("bound", "grouping", [], Binding: new("column", SqlType.I32, default, [], Slot: keys.Length + index));
            }
            if (IsAggregate(expression))
            {
                if (expression.Op == "ordered_aggregate")
                {
                    var orderedName = expression.Text; var hypothetical = AggregateOrdered.Hypothetical(orderedName);
                    var orders = expression.AggregateOrder ?? [];
                    if (!AggregateOrdered.Known(orderedName) || orders.Length == 0 ||
                        (hypothetical ? expression.Args.Length != orders.Length : orders.Length != 1 || expression.Args.Length != (orderedName == "mode" ? 0 : 1)))
                        throw new SqlException(SqlStates.UndefinedFunction, "invalid ordered-set aggregate arguments");
                    if (expression.Args.Any(Contains) || orders.Any(key => Contains(key.Expression))) throw new SqlException(SqlStates.GroupingError, "nested aggregate");
                    if (expression.Args.Any(WindowResolve.Contains) || orders.Any(key => WindowResolve.Contains(key.Expression))) throw new SqlException(SqlStates.GroupingError, "window inside aggregate");
                    var within = orders.Select(key => new SortPlan(Resolve.Expr(key.Expression), key.Descending, key.NullsFirst ?? key.Descending)).ToArray();
                    foreach (var key in within) { Resolve.RequireOrderable(key.Expression.Type); CollationRules.Require(key.Expression, true); }
                    var directExpressions = expression.Args.Select(Rewrite).ToArray();
                    var direct = directExpressions.Select(arg => Resolve.Expr(arg)).ToArray();
                    var resultType = within[0].Expression.Type;
                    if (hypothetical)
                    {
                        for (var d = 0; d < direct.Length; d++)
                        {
                            try { direct[d] = Resolve.Adapt(directExpressions[d], direct[d], within[d].Expression.Type); }
                            catch (SqlException error) when (error.SqlState == SqlStates.DatatypeMismatch) { throw new SqlException(SqlStates.UndefinedFunction, "hypothetical argument type mismatch"); }
                        }
                        resultType = orderedName is "rank" or "dense_rank" ? SqlType.I64 : SqlType.F64;
                    }
                    else if (orderedName != "mode")
                    {
                        if (direct[0].Type == SqlType.Unknown) direct[0] = Resolve.Adapt(directExpressions[0], direct[0], SqlType.F64);
                        var fractionType = direct[0].Type.Kind == SqlKind.Array ? direct[0].Type.Element! : direct[0].Type;
                        if (!fractionType.IsNumeric()) throw new SqlException(SqlStates.UndefinedFunction, "percentile fraction must be numeric");
                        if (orderedName == "percentile_cont")
                        {
                            if (!resultType.IsNumeric() && resultType != SqlType.Interval) throw new SqlException(SqlStates.UndefinedFunction, "continuous percentile requires numeric or interval input");
                            if (resultType != SqlType.Interval) resultType = SqlType.F64;
                        }
                        if (direct[0].Type.Kind == SqlKind.Array) resultType = SqlType.Array(resultType);
                    }
                    var orderedFilter = expression.Predicate is null ? null : Resolve.Expr(expression.Predicate);
                    if (orderedFilter is not null && orderedFilter.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("aggregate FILTER requires boolean");
                    var orderedSlot = keys.Length + groupingCalls.Count + aggregates.Count;
                    var orderedCollation = CollationRules.Collatable(resultType) ? within[0].Expression.Collation : null;
                    aggregates.Add(new(orderedName, null, resultType, false, orderedFilter, WithinOrder: within, Direct: direct, Collation: orderedCollation));
                    return new("bound", orderedName, [], Binding: new("column", resultType, default, [], Slot: orderedSlot, Collation: orderedCollation));
                }
                if (expression.Args.Length != (expression.Text == "string_agg" || AggregateMoments.Pair(expression.Text) || AggregateJson.Object(expression.Text) ? 2 : 1)) throw new SqlException(SqlStates.UndefinedFunction, "aggregate requires one argument");
                if (expression.Args.Any(WindowResolve.Contains) || (expression.AggregateOrder ?? []).Any(key => WindowResolve.Contains(key.Expression))) throw new SqlException(SqlStates.GroupingError, "window inside aggregate");
                var argument = expression.Args[0].Op == "star" ? null : Resolve.Expr(expression.Args[0]);
                var name = expression.Text;
                if (argument is null && (expression.AggregateOrder is { Length: > 0 } || expression.Op == "call_distinct")) throw Errors.Syntax("star aggregate cannot use DISTINCT or ORDER BY");
                if (argument is null && name != "count") throw new SqlException(SqlStates.UndefinedFunction, "star requires count");
                var type = argument?.Type ?? SqlType.Unknown;
                if (name is "min" or "max" || expression.Op == "call_distinct") Resolve.RequireOrderable(type);
                if (name is "sum" or "avg" && !type.IsNumeric()) throw new SqlException(SqlStates.UndefinedFunction, "numeric aggregate argument required");
                if (name is "bool_and" or "bool_or" or "every" && type != SqlType.Boolean) throw new SqlException(SqlStates.UndefinedFunction, "boolean aggregate argument required");
                if (name is "bit_and" or "bit_or" or "bit_xor" && !type.IsInteger()) throw new SqlException(SqlStates.UndefinedFunction, "bitwise aggregate requires integer");
                if (name == "array_agg" && type == SqlType.Unknown) throw new SqlException(SqlStates.AmbiguousFunction, "array_agg argument type is ambiguous");
                if (name == "string_agg" && type == SqlType.Unknown)
                {
                    var second = Resolve.Expr(expression.Args[1]);
                    type = second.Type == SqlType.Unknown ? SqlType.Text : second.Type;
                    argument = Resolve.Adapt(expression.Args[0], argument!, type);
                }
                if (name == "string_agg" && type.Kind is not (SqlKind.Text or SqlKind.Bytea)) throw new SqlException(SqlStates.UndefinedFunction, "string_agg requires text or bytea");
                var extra = expression.Args.Skip(1).Select(arg => Resolve.Expr(arg, name == "string_agg" ? type : SqlType.Unknown)).ToArray();
                if ((AggregateMoments.Single(name) || AggregateMoments.Pair(name)) && (!type.IsNumeric() || extra.Any(arg => !arg.Type.IsNumeric())))
                    throw new SqlException(SqlStates.UndefinedFunction, "statistical aggregate requires numeric arguments");
                if (name == "string_agg" && (type.Kind is not (SqlKind.Text or SqlKind.Bytea) || extra[0].Type != type)) throw new SqlException(SqlStates.UndefinedFunction, "string_agg requires matching text or bytea inputs");
                var inputOrder = (expression.AggregateOrder ?? []).Select(key => new SortPlan(Resolve.Expr(key.Expression), key.Descending, key.NullsFirst ?? key.Descending)).ToArray();
                foreach (var key in inputOrder) { Resolve.RequireOrderable(key.Expression.Type); CollationRules.Require(key.Expression, true); }
                if (expression.Op == "call_distinct" && inputOrder.Any(key => argument is null || !Resolve.Equivalent(key.Expression, argument) && !extra.Any(arg => Resolve.Equivalent(arg, key.Expression))))
                    throw new SqlException(SqlStates.InvalidColumnReference, "DISTINCT aggregate order key must be an argument");
                var result = name switch
                {
                    _ when AggregateJson.IsJson(name) => name.StartsWith("jsonb", StringComparison.Ordinal) ? SqlType.Jsonb : SqlType.Json,
                    "regr_count" => SqlType.I64,
                    _ when AggregateMoments.Pair(name) => SqlType.F64,
                    _ when AggregateMoments.Single(name) => type.IsFloat() ? SqlType.F64 : SqlType.Decimal,
                    "array_agg" => type.Kind == SqlKind.Array ? type : SqlType.Array(type == SqlType.Unknown ? SqlType.Text : type),
                    "count" => SqlType.I64,
                    "sum" when type.Kind is SqlKind.I16 or SqlKind.I32 => SqlType.I64,
                    "sum" or "avg" when !type.IsFloat() => SqlType.Decimal,
                    "avg" => type,
                    _ => type
                };
                var filter = expression.Predicate is null ? null : Resolve.Expr(expression.Predicate);
                if (filter is not null && filter.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("aggregate FILTER requires boolean");
                var localColumn = false; var outerColumn = false;
                void References(Resolved node)
                {
                    localColumn |= node.Op == "column"; outerColumn |= node.Op == "outer";
                    foreach (var child in node.Args) References(child);
                }
                if (argument is not null) References(argument);
                if (filter is not null) References(filter);
                foreach (var item in extra) References(item);
                foreach (var item in inputOrder) References(item.Expression);
                if (outerColumn && !localColumn) throw new SqlException(SqlStates.GroupingError, "outer-level aggregate is not supported");
                var slot = keys.Length + groupingCalls.Count + aggregates.Count;

                var collation = CollationRules.Collatable(result) ? argument?.Collation : null;
                if (name is "min" or "max" || expression.Op == "call_distinct") CollationRules.Require(collation, expression.Op == "call_distinct");
                aggregates.Add(new(name, argument, result, expression.Op == "call_distinct", filter, extra, inputOrder, Collation: collation));
                return new("bound", name, [], Binding: new("column", result, default, [], Slot: slot, Collation: collation));
            }
            if (expression.Binding?.Subquery is { } cell) cell.Group(resolvedKeys);
            if (expression.Binding is { } binding && (binding.Op == "outer" || Evaluate.Constant(binding))) return expression;
            if (expression.Op == "bound" || expression.Op == "name") throw new SqlException(SqlStates.GroupingError, "column must be grouped or aggregated");
            return expression with
            {
                Args = expression.Args.Select(Rewrite).ToArray(),
                Window = expression.Window is null ? null : expression.Window with
                {
                    Partition = expression.Window.Partition.Select(Rewrite).ToArray(),
                    Order = expression.Window.Order.Select(key => key with { Expression = Rewrite(key.Expression) }).ToArray()
                }
            };

        }
        foreach (var window in (syntax.Windows ?? []).Values)
        {
            var beforeValidation = aggregates.Count;
            foreach (var key in window.Partition) _ = Rewrite(key);
            foreach (var key in window.Order) _ = Rewrite(key.Expression);
            aggregates.RemoveRange(beforeValidation, aggregates.Count - beforeValidation);
        }
        var items = syntax.Items.Select(item => item with { Expression = Rewrite(item.Expression) }).ToArray();
        var having = syntax.Having is null ? null : PredicatePlan.Order(Resolve.Expr(Rewrite(Resolve.Bind(syntax.Having, columns))));
        if (having is not null && having.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("HAVING requires boolean");
        var order = (syntax.Order ?? []).Select(item =>
        {
            // A computed output used as an ORDER key is independently resolved;
            // its aggregates fold separately, just as an explicit ORDER expression.
            int? outputIndex = null;
            if (item.Expression.Op == "integer" && int.TryParse(item.Expression.Text, out var ordinal) && ordinal > 0 && ordinal <= items.Length) outputIndex = ordinal - 1;
            if (item.Expression.Op == "name" && !item.Expression.Text.Contains('.'))
            {
                var matches = Enumerable.Range(0, items.Length).Where(i => (items[i].Alias ?? items[i].Expression.Text) == item.Expression.Text).ToArray();
                if (matches.Length == 1) outputIndex = matches[0];
            }
            if (outputIndex is { } at && Contains(syntax.Items[at].Expression))
                return item with { Expression = Rewrite(syntax.Items[at].Expression), OutputIndex = at };
            if (item.Expression.Op == "integer" || item.Expression.Op == "name" && !item.Expression.Text.Contains('.') && items.Any(i => (i.Alias ?? i.Expression.Text) == item.Expression.Text)) return item;
            return item with { Expression = Rewrite(Resolve.Bind(item.Expression, columns)) };
        }).ToArray();
        var distinctOn = syntax.DistinctOn?.Select(key =>
        {
            if (key.Op == "integer" || key.Op == "name" && items.Any(item => (item.Alias ?? item.Expression.Text) == key.Text)) return key;
            var bound = Resolve.Bind(key, columns);
            for (var i = 0; i < (syntax.Order?.Length ?? 0); i++)
                if (SyntaxIdentity.Same(bound, Resolve.Bind(syntax.Order![i].Expression, columns))) return order[i].Expression;
            for (var i = 0; i < syntax.Items.Length; i++)
                if (SyntaxIdentity.Same(bound, syntax.Items[i].Expression)) return items[i].Expression;
            return Rewrite(bound);
        }).ToArray();
        return (syntax with { Items = items, Order = order, DistinctOn = distinctOn }, new(resolvedKeys, aggregates.ToArray(), having, sets, groupingColumns));
    }
    private static bool Same(Expression left, Expression right) => left.Op == right.Op &&
        (left.Op == "bound" ? Resolve.Equivalent(left.Binding!, right.Binding!) : left.Text == right.Text) &&
        left.Args.Length == right.Args.Length && left.Args.Zip(right.Args).All(pair => Same(pair.First, pair.Second));
}
