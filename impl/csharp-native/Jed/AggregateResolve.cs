namespace Jed;

internal sealed record AggregatePlan(string Name, Resolved? Argument, SqlType Type, bool Distinct, Resolved? Filter, Resolved[]? Extra = null, SortPlan[]? Order = null);
internal sealed record GroupPlan(Resolved[] Keys, AggregatePlan[] Aggregates, Resolved? Having);

internal static class AggregateResolve
{
    internal static bool IsAggregate(Expression expression) => expression.Window is null && expression.Op is "call" or "call_distinct" &&
        expression.Text is "count" or "sum" or "avg" or "min" or "max" or "bool_and" or "bool_or" or "every" or "array_agg" or "string_agg" or "bit_and" or "bit_or" or "bit_xor";
    private static bool Contains(Expression expression) => IsAggregate(expression) || expression.Args.Any(Contains) || expression.Window is { } window && (window.Partition.Any(Contains) || window.Order.Any(key => Contains(key.Expression)));
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
        var grouped = keys.Length != 0 || syntax.Having is not null || syntax.Items.Any(i => Contains(i.Expression)) ||
            (syntax.Order ?? []).Any(o => Contains(o.Expression));
        if (!grouped) return (syntax, null);
        var resolvedKeys = keys.Select(k => Resolve.Expr(k)).ToArray();
        var aggregates = new List<AggregatePlan>();
        Expression Rewrite(Expression expression)
        {
            for (var i = 0; i < keys.Length; i++)
                if (Same(expression, keys[i])) return new("bound", expression.Text, [], Binding: new("column", resolvedKeys[i].Type, default, [], Slot: i));
            if (IsAggregate(expression))
            {
                if (expression.Args.Length != (expression.Text == "string_agg" ? 2 : 1)) throw new SqlException(SqlStates.UndefinedFunction, "aggregate requires one argument");
                if (expression.Args.Any(WindowResolve.Contains)) throw new SqlException(SqlStates.GroupingError, "window inside aggregate");
                var argument = expression.Args[0].Op == "star" ? null : Resolve.Expr(expression.Args[0]);
                var name = expression.Text;
                if (argument is null && name != "count") throw new SqlException(SqlStates.UndefinedFunction, "star requires count");
                var type = argument?.Type ?? SqlType.Unknown;
                if (name is "sum" or "avg" && !type.IsNumeric()) throw new SqlException(SqlStates.UndefinedFunction, "numeric aggregate argument required");
                if (name is "bool_and" or "bool_or" or "every" && type != SqlType.Boolean) throw new SqlException(SqlStates.UndefinedFunction, "boolean aggregate argument required");
                if (name is "bit_and" or "bit_or" or "bit_xor" && !type.IsInteger()) throw new SqlException(SqlStates.UndefinedFunction, "bitwise aggregate requires integer");
                var extra = expression.Args.Skip(1).Select(arg => Resolve.Expr(arg, type)).ToArray();
                if (name == "string_agg" && (type.Kind is not (SqlKind.Text or SqlKind.Bytea) || extra[0].Type != type)) throw new SqlException(SqlStates.UndefinedFunction, "string_agg requires matching text or bytea inputs");
                var inputOrder = (expression.AggregateOrder ?? []).Select(key => new SortPlan(Resolve.Expr(key.Expression), key.Descending, key.NullsFirst ?? key.Descending)).ToArray();
                if (expression.Op == "call_distinct" && inputOrder.Any(key => argument is null || !Resolve.Equivalent(key.Expression, argument) && !extra.Any(arg => Resolve.Equivalent(arg, key.Expression))))
                    throw new SqlException(SqlStates.InvalidColumnReference, "DISTINCT aggregate order key must be an argument");
                var result = name switch
                {
                    "array_agg" => type.Kind == SqlKind.Array ? type : SqlType.Array(type == SqlType.Unknown ? SqlType.Text : type),
                    "count" => SqlType.I64,
                    "sum" when type.Kind is SqlKind.I16 or SqlKind.I32 => SqlType.I64,
                    "sum" or "avg" when !type.IsFloat() => SqlType.Decimal,
                    "avg" => SqlType.F64,
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
                var slot = keys.Length + aggregates.Count;

                aggregates.Add(new(name, argument, result, expression.Op == "call_distinct", filter, extra, inputOrder));
                return new("bound", name, [], Binding: new("column", result, default, [], Slot: slot));
            }
            if (expression.Binding?.Subquery is { } cell) cell.Group(resolvedKeys);
            if (expression.Binding?.Op == "outer") return expression;
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
            foreach (var key in window.Partition) _ = Rewrite(key);
            foreach (var key in window.Order) _ = Rewrite(key.Expression);
        }
        var items = syntax.Items.Select(item => item with { Expression = Rewrite(item.Expression) }).ToArray();
        var having = syntax.Having is null ? null : Resolve.Expr(Rewrite(Resolve.Bind(syntax.Having, columns)));
        if (having is not null && having.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("HAVING requires boolean");
        var order = (syntax.Order ?? []).Select(item =>
        {
            // Output aliases and ordinal positions are resolved by the ordinary SELECT resolver.
            if (item.Expression.Op == "integer" || item.Expression.Op == "name" && items.Any(i => (i.Alias ?? i.Expression.Text) == item.Expression.Text)) return item;
            return item with { Expression = Rewrite(Resolve.Bind(item.Expression, columns)) };
        }).ToArray();
        return (syntax with { Items = items, Order = order }, new(resolvedKeys, aggregates.ToArray(), having));
    }
    private static bool Same(Expression left, Expression right) => left.Op == right.Op &&
        (left.Op == "bound" ? Resolve.Equivalent(left.Binding!, right.Binding!) : left.Text == right.Text) &&
        left.Args.Length == right.Args.Length && left.Args.Zip(right.Args).All(pair => Same(pair.First, pair.Second));
}
