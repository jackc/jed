namespace Jed;

// Slots belong to one execution, never to a session or a cached parsed statement.
internal sealed class ParameterSlot(int index)
{
    internal int Index { get; } = index;
    internal SqlType Type { get; private set; } = SqlType.Unknown;
    internal bool NotNull { get; set; }
    private SqlValue value;
    private bool bound;
    internal Resolved Resolve(SqlType hint)
    {
        if (hint != SqlType.Unknown && hint.Identity != "record" && hint.Element?.Identity != "record")
        {
            if (Type == SqlType.Unknown) Type = hint;
            else if (Type.IsInteger() && hint.IsInteger()) Type = Jed.Resolve.Common(Type, hint);
            else if (Type != hint) throw Errors.Type($"conflicting types for parameter ${Index}");
        }
        var local = hint != SqlType.Unknown ? hint : Type;
        if (!bound) return new("constant", local, SqlValue.Null(local), []);
        // Integer occurrences keep their contextual width while comparisons retain the
        // widest bound integer. Assignment and explicit cast sites enforce their range.
        var result = local.IsInteger() && Type.IsInteger() ? value : Jed.Resolve.Cast(value, local);
        return new("constant", local, result, []);
    }
    internal void Bind(SqlValue supplied, TemporalContext temporal)
    {
        if (Type == SqlType.Unknown) throw new SqlException(SqlStates.IndeterminateDatatype, $"cannot determine type of parameter ${Index}");
        value = Coerce(supplied, Type, temporal);
        if (NotNull && value.IsNull) throw new SqlException(SqlStates.NotNullViolation, $"NULL parameter ${Index}");
        bound = true;
    }
    private static SqlValue Coerce(SqlValue value, SqlType target, TemporalContext temporal)
    {
        if (value.IsNull) return SqlValue.Null(target);
        if (value.Type == SqlType.Text && target.Kind is SqlKind.Text or SqlKind.Bytea or SqlKind.Uuid or SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.Interval or SqlKind.Array or SqlKind.Range or SqlKind.Composite or SqlKind.Json or SqlKind.Jsonb or SqlKind.Jsonpath) return TemporalText.Parse(value.AsText(), target, temporal);
        if (value.Type == target) return value;
        if (value.Type.IsInteger() && (target.IsInteger() || target == SqlType.Decimal) || value.Type == SqlType.F32 && target == SqlType.F64 || target.IsFloat() && (value.Type.IsInteger() || value.Type == SqlType.Decimal))
            return Jed.Resolve.Cast(value, target);
        if (value.Type.Kind is SqlKind.Array or SqlKind.Range or SqlKind.Composite && value.Type.Kind == target.Kind && OpenValue.CanCast(value.Type, target))
            return Jed.Resolve.Cast(value, target);
        throw Errors.Type($"parameter requires {target.Name()}");
    }
}

internal sealed class Parameters
{
    private readonly Dictionary<int, ParameterSlot> slots = [];
    internal IReadOnlyList<ParameterSlot> Slots => slots.OrderBy(pair => pair.Key).Select(pair => pair.Value).ToArray();
    internal bool Any => slots.Count != 0;
    internal void Bind(IReadOnlyList<SqlValue> values, TemporalContext context)
    {
        var count = slots.Count == 0 ? 0 : slots.Keys.Max();
        if (values.Count != count) throw Errors.Syntax("incorrect number of parameter values");
        for (var i = 1; i <= count; i++)
        {
            if (!slots.TryGetValue(i, out var slot) || slot.Type == SqlType.Unknown) throw new SqlException(SqlStates.IndeterminateDatatype, $"cannot determine type of parameter ${i}");
            slot.Bind(values[i - 1], context);
        }
    }
    internal Expression Expression(Expression expression)
    {
        if (expression.Op == "parameter")
        {
            var index = int.Parse(expression.Text, System.Globalization.CultureInfo.InvariantCulture);
            if (!slots.TryGetValue(index, out var slot)) slots.Add(index, slot = new(index));
            return expression with { Parameter = slot };
        }
        return expression with { Args = expression.Args.Select(Expression).ToArray(), Predicate = expression.Predicate is null ? null : Expression(expression.Predicate),
            Query = expression.Query is null ? null : Query(expression.Query), AggregateOrder = expression.AggregateOrder?.Select(order => order with { Expression = Expression(order.Expression) }).ToArray(),
            Window = expression.Window is null ? null : Window(expression.Window) };
    }
    private WindowSyntax Window(WindowSyntax window) => window with { Partition = window.Partition.Select(Expression).ToArray(), Order = window.Order.Select(order => order with { Expression = Expression(order.Expression) }).ToArray(),
        Frame = window.Frame is not { } frame ? null : frame with { Start = frame.Start with { Offset = frame.Start.Offset is null ? null : Expression(frame.Start.Offset) }, End = frame.End with { Offset = frame.End.Offset is null ? null : Expression(frame.End.Offset) } } };
    internal SelectSyntax Query(SelectSyntax query) => query with
    {
        Items = query.Items.Select(item => item with { Expression = Expression(item.Expression) }).ToArray(),
        Filter = query.Filter is null ? null : Expression(query.Filter), Having = query.Having is null ? null : Expression(query.Having),
        GroupBy = query.GroupBy?.Select(Expression).ToArray(), Order = query.Order?.Select(order => order with { Expression = Expression(order.Expression) }).ToArray(),
        Relation = query.Relation is null ? null : Relation(query.Relation), With = query.With?.Select(cte => cte with { Query = Query(cte.Query) }).ToArray(), Windows = query.Windows?.ToDictionary(pair => pair.Key, pair => Window(pair.Value), StringComparer.Ordinal)
    };
    private RelationSyntax Relation(RelationSyntax relation) => relation switch
    {
        DerivedRelation derived => derived with { Query = Query(derived.Query) },
        SetRelation set => set with { Left = Query(set.Left), Right = Query(set.Right) },
        FunctionRelation function => function with { Function = Expression(function.Function) },
        ValuesRelation values => values with { Values = values.Values.Select(row => row.Select(Expression).ToArray()).ToArray() },
        JoinedRelation join => join with { Left = Relation(join.Left), Right = Relation(join.Right), On = join.On is null ? null : Expression(join.On) },
        LateralRelation lateral => lateral with { Source = Relation(lateral.Source) },
        AliasedRelation aliased => aliased with { Source = Relation(aliased.Source) },
        _ => relation
    };
    private SelectItem[] Returning(SelectItem[] items) => items.Select(item => item with { Expression = Expression(item.Expression) }).ToArray();
    internal StatementSyntax Statement(StatementSyntax statement) => statement switch
    {
        SelectSyntax query => Query(query),
        CreateTableAsSyntax create => create with { Query = Query(create.Query) },
        InsertSyntax insert => insert with { Values = insert.Values.Select(row => row.Select(Expression).ToArray()).ToArray(), Source = insert.Source is null ? null : Query(insert.Source), Returning = Returning(insert.Returning),
            Conflict = insert.Conflict is null ? null : insert.Conflict with { Predicate = insert.Conflict.Predicate is null ? null : Expression(insert.Conflict.Predicate), Filter = insert.Conflict.Filter is null ? null : Expression(insert.Conflict.Filter), Assignments = insert.Conflict.Assignments?.Select(assignment => assignment with { Value = Expression(assignment.Value) }).ToArray() } },
        UpdateSyntax update => update with { Assignments = update.Assignments.Select(assignment => assignment with { Value = Expression(assignment.Value) }).ToArray(), Filter = update.Filter is null ? null : Expression(update.Filter), Returning = Returning(update.Returning) },
        DeleteSyntax delete => delete with { Filter = delete.Filter is null ? null : Expression(delete.Filter), Returning = Returning(delete.Returning) },
        _ => statement
    };
    internal void Infer(StatementSyntax statement, Database database, DatabaseSnapshot snapshot, PendingDatabase? pending, Catalog catalog, Meter meter, CancellationToken token)
    {
        void Query(SelectSyntax query) => _ = RelationExecution.Query(query, database, snapshot, pending, catalog, meter, token);
        void Returning(CatalogTable table, SelectItem[] items)
        {
            var binder = new SubqueryBinder(StatementExecution.ReturningColumns(table), database, snapshot, pending, catalog, meter, token);
            _ = Resolve.Select(new(items.Select(item => item.Expression.Op == "star" ? item : item with { Expression = binder.Bind(item.Expression) }).ToArray(), null, null, 0), StatementExecution.ReturningColumns(table));
        }
        void Assign(Expression expression, CatalogColumn column, SubqueryBinder binder)
        {
            if (expression.Op == "default") return;
            if (expression.Parameter is { } slot) slot.NotNull |= column.NotNull;
            _ = Resolve.Expr(binder.Bind(expression), column.Type.Type);
        }
        switch (statement)
        {
            case SelectSyntax query: Query(query); break;
            case CreateTableAsSyntax create: Query(create.Query); break;
            case InsertSyntax insert:
            {
                var table = StatementExecution.Table(catalog, insert.Table); var binder = new SubqueryBinder([], database, snapshot, pending, catalog, meter, token);
                var targets = insert.Columns is null ? table.Columns : insert.Columns.Select(name => table.Columns.FirstOrDefault(column => column.Name == name) ?? throw new SqlException(SqlStates.UndefinedColumn, "column does not exist")).ToArray();
                foreach (var row in insert.Values)
                {
                    if (row.Length != 0 && row.Length != targets.Length) throw Errors.Syntax("INSERT value count differs from target");
                    for (var i = 0; i < row.Length; i++) Assign(row[i], targets[i], binder);
                }
                if (insert.Source is { } source)
                {
                    if (!source.Distinct && !source.SetQuery && !source.ValuesQuery && source.Items.Length == targets.Length)
                        for (var i = 0; i < source.Items.Length; i++) if (source.Items[i].Expression.Parameter is { } slot) { slot.Resolve(targets[i].Type.Type); slot.NotNull |= targets[i].NotNull; }
                    Query(source);
                }
                if (insert.Conflict is { } conflict)
                {
                    var columns = StatementExecution.Columns(table, insert.Alias).Concat(table.Columns.Select(column => new BoundColumn(column.Name, column.Type.Type, "excluded", Hidden: true))).ToArray();
                    var conflictBinder = new SubqueryBinder(columns, database, snapshot, pending, catalog, meter, token);
                    foreach (var assignment in conflict.Assignments ?? []) Assign(assignment.Value, table.Columns.FirstOrDefault(column => column.Name == assignment.Column) ?? throw new SqlException(SqlStates.UndefinedColumn, "column does not exist"), conflictBinder);
                    if (conflict.Filter is { } filter) _ = Resolve.Expr(conflictBinder.Bind(filter), SqlType.Boolean);
                }
                Returning(insert.Alias is { } alias ? table with { Name = alias } : table, insert.Returning); break;
            }
            case UpdateSyntax update:
            {
                var table = StatementExecution.Table(catalog, update.Table); var binder = new SubqueryBinder(StatementExecution.Columns(table), database, snapshot, pending, catalog, meter, token);
                foreach (var assignment in update.Assignments) Assign(assignment.Value, table.Columns.FirstOrDefault(column => column.Name == assignment.Column) ?? throw new SqlException(SqlStates.UndefinedColumn, "column does not exist"), binder);
                if (update.Filter is { } filter) _ = Resolve.Expr(binder.Bind(filter), SqlType.Boolean);
                Returning(table, update.Returning); break;
            }
            case DeleteSyntax delete:
            {
                var table = StatementExecution.Table(catalog, delete.Table); var binder = new SubqueryBinder(StatementExecution.Columns(table), database, snapshot, pending, catalog, meter, token);
                if (delete.Filter is { } filter) _ = Resolve.Expr(binder.Bind(filter), SqlType.Boolean);
                Returning(table, delete.Returning); break;
            }
        }
    }
}
