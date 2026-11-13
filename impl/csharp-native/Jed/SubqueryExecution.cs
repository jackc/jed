using System.Runtime.CompilerServices;

namespace Jed;

internal sealed class SubqueryCell(PreparedRelation source, string mode, OuterFrame frame, int[] referencedSlots, Meter meter, bool rowValued = false)
{
    internal ExplainNode? Plan => source.Plan;
    private long heldBytes;
    internal void ReleaseConsumed() { if (IsCorrelated) { meter.Memory.Release(heldBytes); heldBytes = 0; Values.Clear(); } }
    private int[]? groupedSlots;
    internal void Group(Resolved[] keys)
    {
        if (referencedSlots.Length == 0) return;
        groupedSlots = Enumerable.Repeat(-1, frame.RowTypes.Length).ToArray();
        foreach (var slot in referencedSlots)
        {
            var key = Array.FindIndex(keys, key => key.Op == "column" && key.Slot == slot);
            if (key < 0) throw new SqlException(SqlStates.GroupingError, "subquery uses an ungrouped outer column");
            groupedSlots[slot] = key;
        }
    }
    internal IReadOnlyList<int> ReferencedSlots => referencedSlots;
    internal SqlType RowType { get; } = !rowValued && source.Columns.Length == 1 ? source.Columns[0].Type : SqlType.Record(source.Columns.Select((column, index) => new SqlField($"f{index + 1}", column.Type)));
    internal SqlValue Value { get; private set; }
    internal List<SqlValue> Values { get; } = [];
    internal OuterFrame[] Dependencies => source.Dependencies ?? [];
    internal bool IsCorrelated => Dependencies.Length != 0;
    internal bool Initialized { get; private set; }
    internal async ValueTask Initialize()
    {
        if (Initialized) return;
        if (source.InitializeConstants is { } constants) await constants().ConfigureAwait(false);
        if (IsCorrelated) return;
        await Read().ConfigureAwait(false); Initialized = true;
    }
    internal async ValueTask Execute(SqlValue[]? row)
    {
        if (groupedSlots is not null)
        {
            var values = frame.RowTypes.Select(SqlValue.Null).ToArray();
            for (var i = 0; i < groupedSlots.Length; i++) if (groupedSlots[i] >= 0) values[i] = row![groupedSlots[i]];
            frame.Assign(values);
        }
        else frame.Assign(row);
        await Read().ConfigureAwait(false);
    }
    private async ValueTask Read()
    {
        meter.Memory.Release(heldBytes); heldBytes = 0; Values.Clear();
        if (source.Initialize is { } initialize) await initialize().ConfigureAwait(false);
        await foreach (var row in source.Rows.ConfigureAwait(false))
        {
            if (meter.Memory.Enabled)
            {
                var bytes = MemorySize.RowBytes(row); if (!source.MemoryOutput) meter.Memory.Reserve(bytes);
                heldBytes = Meter.Add(heldBytes, bytes);
            }
            Values.Add(!rowValued && row.Length == 1 ? row[0] : SqlValue.Composite(new(RowType, row)));
        }
        if (mode == "scalar" && Values.Count > 1) throw new SqlException(SqlStates.CardinalityViolation, "scalar subquery returned more than one row");
        Value = mode == "exists" ? SqlValue.Boolean(Values.Count > 0) : Values.Count == 0 ? SqlValue.Null(RowType) : Values[0];
    }
}

internal static class SubqueryExecution
{
    internal static Expression Bind(Expression expression, OuterFrame frame, Database database, DatabaseSnapshot snapshot, PendingDatabase? pending,
        Catalog catalog, Meter meter, CancellationToken cancellationToken, QueryScope? scope, List<SubqueryCell> cells)
    {
        if (expression.Query is { } body)
        {
            var referenced = frame.StartCapture(); PreparedRelation source;
            try { source = RelationExecution.Query(body, database, snapshot, pending, catalog, meter, cancellationToken, scope: new(scope, frame)); }
            finally { frame.EndCapture(referenced); }
            var rowValued = expression.Text.StartsWith("row:", StringComparison.Ordinal);
            var arity = rowValued ? int.Parse(expression.Text[4..], System.Globalization.CultureInfo.InvariantCulture) : 1;
            if (expression.Op != "exists" && source.Columns.Length != arity) throw Errors.Syntax("subquery column count does not match operand");
            var mode = expression.Op == "exists" ? "exists" : expression.Op == "subquery_set" ? "set" : "scalar";
            var cell = new SubqueryCell(source, mode, frame, referenced.ToArray(), meter, rowValued); cells.Add(cell);
            return expression with { Op = "initplan", Binding = new(cell.IsCorrelated ? "correlated" : "initplan", mode == "exists" ? SqlType.Boolean : cell.RowType, default, [], Subquery: cell,
                Collation: mode != "exists" && !rowValued && source.Columns.Length == 1 ? source.Columns[0].Collation?.Implicit() : null), Query = null };
        }
        if (expression.Args.Length == 2 && expression.Args[0].Op == "row" && expression.Args[1].Query is not null &&
            expression.Op is "=" or "<>" or "!=" or "<" or "<=" or ">" or ">=" or "in_subquery" or "quantified_subquery")
            expression = expression with { Args = [expression.Args[0], expression.Args[1] with { Text = "row:" + expression.Args[0].Args.Length }] };
        return expression with
        {
            Args = expression.Args.Select(arg => Bind(arg, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, cells)).ToArray(),
            Predicate = expression.Predicate is null ? null : Bind(expression.Predicate, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, cells),
            AggregateOrder = expression.AggregateOrder?.Select(key => key with { Expression = Bind(key.Expression, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, cells) }).ToArray(),
            Window = expression.Window is null ? null : expression.Window with
            {
                Partition = expression.Window.Partition.Select(key => Bind(key, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, cells)).ToArray(),
                Order = expression.Window.Order.Select(key => key with { Expression = Bind(key.Expression, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, cells) }).ToArray()
            }

        };
    }
    internal static SelectSyntax Bind(SelectSyntax query, OuterFrame frame, Database database, DatabaseSnapshot snapshot, PendingDatabase? pending,
        Catalog catalog, Meter meter, CancellationToken cancellationToken, QueryScope? scope, List<SubqueryCell> cells)
    {
        Expression BindExpression(Expression expression) => Bind(expression, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, cells);
        return query with
        {
            DistinctOn = query.DistinctOn?.Select(BindExpression).ToArray(),
            Items = query.Items.Select(item => item with { Expression = BindExpression(item.Expression) }).ToArray(),
            Windows = query.Windows?.ToDictionary(pair => pair.Key, pair => pair.Value with
            {
                Partition = pair.Value.Partition.Select(BindExpression).ToArray(),
                Order = pair.Value.Order.Select(key => key with { Expression = BindExpression(key.Expression) }).ToArray()
            }, StringComparer.Ordinal),

            Filter = query.Filter is null ? null : BindExpression(query.Filter),
            Order = query.Order?.Select(order => order with { Expression = BindExpression(order.Expression) }).ToArray(),
            GroupBy = query.GroupBy?.Select(BindExpression).ToArray(), Having = query.Having is null ? null : BindExpression(query.Having)
        };
    }
    internal static async IAsyncEnumerable<SqlValue[]> Initialized(Func<ValueTask> initialize, IAsyncEnumerable<SqlValue[]> source,
        [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        await initialize().ConfigureAwait(false);
        await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false)) yield return row;
    }
    internal static SqlValue In(Resolved expression, Meter meter, SqlValue[]? row)
    {
        meter.Charge(Costs.OperatorEval);
        return Negate(Membership(Evaluate.Expr(expression.Args[0], meter, row: row), expression.Subquery!.Values, meter), expression.Text == "not");
    }
    internal static async ValueTask<SqlValue> InAsync(Resolved expression, Meter meter, SqlValue[]? row, bool correlated = false)
    {
        if (!correlated) meter.Charge(Costs.OperatorEval);
        return Negate(Membership(await Evaluate.ExprAsync(expression.Args[0], meter, row: row).ConfigureAwait(false), expression.Subquery!.Values, meter), expression.Text == "not");
    }
    private static SqlValue Negate(SqlValue value, bool negate) => negate && !value.IsNull ? SqlValue.Boolean(!value.AsBoolean()) : value;
    private static SqlValue Membership(SqlValue left, IEnumerable<SqlValue> values, Meter meter)
    {
        var unknown = false; var matched = false;
        foreach (var value in values)
        {
            meter.Charge(Costs.OperatorEval); meter.Guard();
            var equal = Equal(left, value, meter);
            if (equal.IsNull) unknown = true; else if (equal.AsBoolean()) matched = true;
        }
        return matched ? SqlValue.Boolean(true) : unknown ? SqlValue.Null(SqlType.Boolean) : SqlValue.Boolean(false);
    }
    private static SqlValue Equal(SqlValue left, SqlValue right, Meter meter) => left.IsNull || right.IsNull ? SqlValue.Null(SqlType.Boolean) :
        left.Type.Kind == SqlKind.Composite ? OpenExpression.Compare("=", left, right) : SqlValue.Boolean(Evaluate.Compare(left, right, meter) == 0);
}

// One binder per expression input shape. The statement driver initializes independent
// subqueries once, then evaluates correlated nodes through the async expression driver.
internal sealed class SubqueryBinder(BoundColumn[] columns, Database database, DatabaseSnapshot snapshot,
    PendingDatabase? pending, Catalog catalog, Meter meter, CancellationToken cancellationToken, QueryScope? scope = null)
{
    private readonly OuterFrame frame = new(columns);
    private readonly List<SubqueryCell> cells = [];
    internal Expression Bind(Expression expression)
    {
        var bound = Resolve.Bind(SubqueryExecution.Bind(Resolve.BindTypes(expression, catalog, meter.Temporal), frame, database, snapshot, pending,
            catalog, meter, cancellationToken, scope, cells), columns.Concat(scope?.OuterColumns ?? []).ToArray());
        void ResolvedCells(Resolved node)
        {
            if (node.Subquery is { } cell && !cells.Contains(cell)) cells.Add(cell);
            foreach (var argument in node.Args) ResolvedCells(argument);
        }
        void ExpressionCells(Expression node)
        {
            if (node.Binding is { } binding) ResolvedCells(binding);
            foreach (var argument in node.Args) ExpressionCells(argument);
            if (node.Predicate is { } predicate) ExpressionCells(predicate);
        }
        ExpressionCells(bound);
        return bound;
    }
    internal async ValueTask Initialize()
    {
        foreach (var cell in cells) await cell.Initialize().ConfigureAwait(false);
    }
}
