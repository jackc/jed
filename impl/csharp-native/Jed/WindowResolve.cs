namespace Jed;

internal sealed record WindowPlan(string Name, Resolved[] Args, SqlType Type, Resolved[] Partition, SortPlan[] Order,
    WindowFrameSyntax? Frame, AggregatePlan? Aggregate = null);
internal sealed record WindowStage(int InputWidth, WindowPlan[] Plans);

internal static class WindowResolve
{
    private static readonly HashSet<string> Names = new(["row_number", "rank", "dense_rank", "percent_rank", "cume_dist", "ntile", "lag", "lead", "first_value", "last_value", "nth_value"], StringComparer.Ordinal);
    internal static bool Contains(Expression expression) => expression.Window is not null || expression.Args.Any(Contains);
    internal static SelectSyntax Expand(SelectSyntax syntax, BoundColumn[] columns)
    {
        var named = new Dictionary<string, WindowSyntax>(StringComparer.Ordinal);
        WindowSyntax ExpandWindow(WindowSyntax window)
        {
            if (window.Name is null) return window;
            if (!named.TryGetValue(window.Name, out var previous)) throw new SqlException(SqlStates.UndefinedObject, "window does not exist");
            if (window.Reference) return previous;
            if (window.Partition.Length != 0) throw Invalid("window extension cannot add PARTITION BY");
            if (window.Order.Length != 0 && previous.Order.Length != 0) throw Invalid("window extension cannot override ORDER BY");
            if (previous.Frame is not null) throw Invalid("cannot copy a window with a frame clause");
            return window with { Name = null, Partition = previous.Partition, Order = window.Order.Length == 0 ? previous.Order : window.Order };
        }
        foreach (var (name, definition) in syntax.Windows ?? [])
        {
            var window = ExpandWindow(definition);
            if (window.Partition.Any(Contains) || window.Order.Any(key => Contains(key.Expression))) throw Invalid("window function in window key");
            window = window with
            {
                Partition = window.Partition.Select(key => Resolve.Bind(key, columns)).ToArray(),
                Order = window.Order.Select(key => key with { Expression = Resolve.Bind(key.Expression, columns) }).ToArray()
            };
            named.Add(name, window);
        }

        Expression ExpandExpression(Expression expression)
        {
            if (expression.Window is null && expression.Op == "call" && Names.Contains(expression.Text)) throw new SqlException(SqlStates.WrongObjectType, "window function requires OVER");
            if (expression.Window is { } window)
            {
                if (expression.Args.Any(Contains) || expression.Predicate is { } predicate && Contains(predicate)) throw Invalid("nested window function");
                window = ExpandWindow(window);
                if (window.Partition.Any(Contains) || window.Order.Any(key => Contains(key.Expression))) throw Invalid("window function in window key");
                window = window with
                {
                    Partition = window.Partition.Select(key => Resolve.Bind(key, columns)).ToArray(),
                    Order = window.Order.Select(key => key with { Expression = Resolve.Bind(key.Expression, columns) }).ToArray()
                };
                expression = expression with { Window = window };
            }
            return expression with { Args = expression.Args.Select(ExpandExpression).ToArray() };
        }
        if (syntax.Filter is { } filter && Contains(filter) || syntax.Having is { } having && Contains(having) || (syntax.GroupBy ?? []).Any(Contains))
            throw Invalid("window function in prohibited query clause");
        return syntax with
        {
            Windows = named,
            Items = syntax.Items.Select(item => item with { Expression = ExpandExpression(item.Expression) }).ToArray(),
            Order = syntax.Order?.Select(key => key with { Expression = ExpandExpression(key.Expression) }).ToArray()
        };
    }
    internal static (SelectSyntax Syntax, WindowStage? Stage) Prepare(SelectSyntax syntax, BoundColumn[] columns, GroupPlan? grouping)
    {
        var plans = new List<WindowPlan>();
        var width = grouping is null ? columns.Count(column => column.Outer is null) : grouping.Keys.Length + grouping.Aggregates.Length;
        Expression Rewrite(Expression expression)
        {
            if (expression.Window is not { } window) return expression with { Args = expression.Args.Select(Rewrite).ToArray() };
            if (expression.AggregateOrder is { Length: > 0 }) throw Errors.Unsupported("aggregate ORDER BY in window function");
            if (expression.Op == "call_distinct") throw Errors.Unsupported("DISTINCT window aggregate");
            var args = expression.Args.Where(arg => arg.Op != "star").Select(arg => Resolve.Expr(Resolve.Bind(arg, columns))).ToArray();
            var partition = window.Partition.Select(key => Resolve.Expr(key)).ToArray();
            var order = window.Order.Select(key => new SortPlan(Resolve.Expr(key.Expression), key.Descending, key.NullsFirst ?? key.Descending)).ToArray();
            static bool Outer(Resolved expression) => expression.Op == "outer" || expression.Args.Any(Outer);
            if (partition.Any(Outer) || order.Any(key => Outer(key.Expression)))
                throw Errors.Unsupported("correlated window key");
            SqlType type; AggregatePlan? aggregate = null;
            if (Names.Contains(expression.Text))
            {
                var count = args.Length;
                var valid = expression.Text switch
                {
                    "ntile" or "first_value" or "last_value" => count == 1,
                    "nth_value" => count == 2,
                    "lag" or "lead" => count is >= 1 and <= 3,
                    _ => count == 0
                };
                if (!valid) throw new SqlException(SqlStates.UndefinedFunction, "wrong window argument count");
                type = expression.Text switch
                {
                    "percent_rank" or "cume_dist" => SqlType.F64,
                    "lag" or "lead" or "first_value" or "last_value" or "nth_value" => args[0].Type,
                    _ => SqlType.I64
                };
                var offsetSlot = expression.Text == "ntile" ? 0 : expression.Text is "lag" or "lead" or "nth_value" ? 1 : -1;
                if (offsetSlot >= 0 && args.Length > offsetSlot && !args[offsetSlot].Type.IsInteger() && args[offsetSlot].Type != SqlType.Unknown)
                    throw Errors.Type("window position requires integer");
                if (args.Length == 3)
                {
                    args[2] = Resolve.Adapt(expression.Args[2], args[2], type);
                    if (Resolve.Common(type, args[2].Type) != type) throw Errors.Type("window default does not fit value type");
                }
                if (expression.Predicate is not null) throw Errors.Unsupported("FILTER requires an aggregate window");
            }
            else
            {
                var call = expression with { Window = null };
                if (!AggregateResolve.IsAggregate(call)) throw new SqlException(SqlStates.UndefinedFunction, "function is not a window function");
                var prepared = AggregateResolve.Prepare(new([new(call, null)], null, null, 0), columns);
                aggregate = prepared.Plan!.Aggregates[0]; type = aggregate.Type;
            }
            if (window.Frame is { } frame)
            {
                if (frame.Start.Kind == "unbounded_following" || frame.End.Kind == "unbounded_preceding" ||
                    frame.Start.Kind == "current" && frame.End.Kind == "preceding" || frame.Start.Kind == "following" && frame.End.Kind is "preceding" or "current")
                    throw Invalid("invalid window frame bounds");
                if (frame.Mode == "groups" && order.Length == 0) throw Invalid("GROUPS requires ORDER BY");
                if (frame.Mode == "range" && (frame.Start.Offset is not null || frame.End.Offset is not null))
                {
                    if (order.Length != 1) throw Invalid("RANGE offset requires exactly one order key");
                    if (order[0].Expression.Type.IsInteger() && new[] { frame.Start, frame.End }.Any(bound => bound.Offset is not null && !Resolve.Expr(bound.Offset).Type.IsInteger())) throw Errors.Unsupported("integer RANGE key requires integer offset");
                    if (!order[0].Expression.Type.IsNumeric()) throw Errors.Unsupported("RANGE offsets require numeric ordering key");
                }
            }
            var slot = width + plans.Count; plans.Add(new(expression.Text, args, type, partition, order, window.Frame, aggregate));
            return new("bound", expression.Text, [], Binding: new("column", type, default, [], Slot: slot));
        }
        var result = syntax with
        {
            Items = syntax.Items.Select(item => item with { Expression = Rewrite(item.Expression) }).ToArray(),
            Order = syntax.Order?.Select(key => key with { Expression = Rewrite(key.Expression) }).ToArray()
        };
        return (result, plans.Count == 0 ? null : new(width, plans.ToArray()));
    }
    private static SqlException Invalid(string message) => new(SqlStates.WindowingError, message);
}
