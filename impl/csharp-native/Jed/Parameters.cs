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
        if (!bound) return new("constant", local, SqlValue.Null(local), [], ParameterIndex: Index, ParameterUnbound: true);
        // Integer occurrences keep their contextual width while comparisons retain the
        // widest bound integer. Assignment and explicit cast sites enforce their range.
        var result = local.IsInteger() && Type.IsInteger() ? value : Jed.Resolve.Cast(value, local);
        return new("constant", local, result, [], ParameterIndex: Index);
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
        if (value.NativeTemporal is { } native)
            return target.Kind switch
            {
                SqlKind.Date => SqlValue.Date(native.Days),
                SqlKind.Timestamp => SqlValue.Timestamp(native.CalendarMicros),
                SqlKind.Timestamptz when native.InstantMicros is { } instant => SqlValue.Timestamp(instant, withZone: true),
                _ => throw Errors.Type("native time requires a compatible temporal parameter; use DateTimeOffset for an instant")
            };
        if (value.Type.Kind == SqlKind.Array && target.Kind == SqlKind.Array && value.AsArray().Count == 0)
            return SqlValue.Array(new(target.Element!, []));
        if (value.Type == SqlType.Text && target.Kind is SqlKind.Text or SqlKind.Bytea or SqlKind.Uuid or SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.Interval or SqlKind.Array or SqlKind.Range or SqlKind.Composite or SqlKind.Json or SqlKind.Jsonb or SqlKind.Jsonpath) return TemporalText.Parse(value.AsText(), target, temporal);
        if (value.Type == target) return value;
        if (value.Type.IsInteger() && (target.IsInteger() || target == SqlType.Decimal) || value.Type == SqlType.F32 && target == SqlType.F64 || target.IsFloat() && (value.Type.IsInteger() || value.Type == SqlType.Decimal))
            return Jed.Resolve.Cast(value, target);
        if (value.Type.Kind == SqlKind.Array && target.Kind == SqlKind.Array)
        {
            var array = value.AsArray();
            return SqlValue.Array(new(target.Element!, array.Values.Select(element => Coerce(element, target.Element!, temporal)), array.Dimensions, array.LowerBounds, true));
        }
        if (value.Type.Kind == SqlKind.Composite && target.Kind == SqlKind.Composite)
        {
            var fields = value.AsComposite().Fields;
            if (fields.Count != target.Fields.Count) throw Errors.Type("parameter record field count mismatch");
            return SqlValue.Composite(new(target, fields.Select((field, index) => Coerce(field, target.Fields[index].Type, temporal))));
        }
        if (value.Type.Kind == SqlKind.Range && target.Kind == SqlKind.Range)
        {
            var range = value.AsRange();
            return SqlValue.Range(range.IsEmpty ? SqlRange.Empty(target) : new(target,
                range.Lower is { } lower ? Coerce(lower, target.Element!, temporal) : (SqlValue?)null,
                range.Upper is { } upper ? Coerce(upper, target.Element!, temporal) : (SqlValue?)null,
                range.LowerInclusive, range.UpperInclusive));
        }
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
        for (var i = 1; i <= count; i++)
            if (!slots.TryGetValue(i, out var slot) || slot.Type == SqlType.Unknown) throw new SqlException(SqlStates.IndeterminateDatatype, $"cannot determine type of parameter ${i}");
        if (values.Count != count) throw Errors.Syntax("incorrect number of parameter values");
        for (var i = 1; i <= count; i++) slots[i].Bind(values[i - 1], context);
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
        LimitExpression = query.LimitExpression is null ? null : Expression(query.LimitExpression),
        OffsetExpression = query.OffsetExpression is null ? null : Expression(query.OffsetExpression),
        DistinctOn = query.DistinctOn?.Select(Expression).ToArray(),
        Items = query.Items.Select(item => item with { Expression = Expression(item.Expression) }).ToArray(),
        Filter = query.Filter is null ? null : Expression(query.Filter), Having = query.Having is null ? null : Expression(query.Having),
        GroupBy = query.GroupBy?.Select(Expression).ToArray(), Order = query.Order?.Select(order => order with { Expression = Expression(order.Expression) }).ToArray(),
        Relation = query.Relation is null ? null : Relation(query.Relation), With = query.With?.Select(cte => cte with { Query = Query(cte.Query), Mutation = cte.Mutation is null ? null : Statement(cte.Mutation) }).ToArray(), Windows = query.Windows?.ToDictionary(pair => pair.Key, pair => Window(pair.Value), StringComparer.Ordinal)
    };
    private RelationSyntax Relation(RelationSyntax relation) => relation switch
    {
        DerivedRelation derived => derived with { Query = Query(derived.Query) },
        SetRelation set => set with { Left = Query(set.Left), Right = Query(set.Right) },
        FunctionRelation function => function with { Function = Expression(function.Function) },
        ValuesRelation values => values with { Values = values.Values.Select(row => row.Select(Expression).ToArray()).ToArray() },
        GroupedRelation group => group with { Source = Relation(group.Source) },
        JoinedRelation join => join with { Left = Relation(join.Left), Right = Relation(join.Right), On = join.On is null ? null : Expression(join.On) },
        LateralRelation lateral => lateral with { Source = Relation(lateral.Source) },
        AliasedRelation aliased => aliased with { Source = Relation(aliased.Source) },
        _ => relation
    };
    private SelectItem[] Returning(SelectItem[] items) => items.Select(item => item with { Expression = Expression(item.Expression) }).ToArray();
    internal StatementSyntax Statement(StatementSyntax statement) => statement switch
    {
        ExplainSyntax explain => explain with { Inner = Statement(explain.Inner) },
        SelectSyntax query => Query(query),
        MergeSyntax merge => merge with { Source = Relation(merge.Source), On = Expression(merge.On), Returning = Returning(merge.Returning), Clauses = merge.Clauses.Select(clause => clause with { Condition = clause.Condition is null ? null : Expression(clause.Condition), Assignments = clause.Assignments?.Select(item => item with { Value = Expression(item.Value) }).ToArray(), Values = clause.Values?.Select(Expression).ToArray() }).ToArray() },
        WithStatementSyntax wrapped => wrapped with { Definitions = wrapped.Definitions.Select(cte => cte with { Query = Query(cte.Query), Mutation = cte.Mutation is null ? null : Statement(cte.Mutation) }).ToArray(), Body = Statement(wrapped.Body) },
        CreateTableAsSyntax create => create with { Query = Query(create.Query) },
        InsertSyntax insert => insert with { Values = insert.Values.Select(row => row.Select(Expression).ToArray()).ToArray(), Source = insert.Source is null ? null : Query(insert.Source), Returning = Returning(insert.Returning),
            Conflict = insert.Conflict is null ? null : insert.Conflict with { Predicate = insert.Conflict.Predicate is null ? null : Expression(insert.Conflict.Predicate), Filter = insert.Conflict.Filter is null ? null : Expression(insert.Conflict.Filter), Assignments = insert.Conflict.Assignments?.Select(assignment => assignment with { Value = Expression(assignment.Value) }).ToArray() } },
        UpdateSyntax update => update with { Assignments = update.Assignments.Select(assignment => assignment with { Value = Expression(assignment.Value) }).ToArray(), Filter = update.Filter is null ? null : Expression(update.Filter), Returning = Returning(update.Returning) },
        DeleteSyntax delete => delete with { Filter = delete.Filter is null ? null : Expression(delete.Filter), Returning = Returning(delete.Returning) },
        _ => statement
    };
    internal BoundColumn[] Infer(StatementSyntax statement, Database database, DatabaseSnapshot snapshot, PendingDatabase? pending, Catalog catalog, Meter meter, CancellationToken token, QueryScope? scope = null, bool localDomain = false)
    {
        if (!localDomain && catalog.Scope is not null && DomainExecution.Target(statement) is not null)
            return DomainExecution.Infer(this, statement, catalog, meter, token, scope);
        if (pending is null && (statement is WithStatementSyntax || statement is SelectSyntax writingQuery && WritableCteExecution.HasWrites(writingQuery) ||
            statement is CreateTableAsSyntax writingCreate && WritableCteExecution.HasWrites(writingCreate.Query)))
            pending = new PendingDatabase(snapshot) { Catalog = catalog };
        if (pending is not null && statement is SelectSyntax withQuery && WritableCteExecution.HasWrites(withQuery))
            return WritableCteExecution.Prepare(database, pending, withQuery.With!, withQuery.Recursive, withQuery with { With = null }, meter, token).Columns;
        if (pending is not null && statement is WithStatementSyntax wrapped)
            return WritableCteExecution.Prepare(database, pending, wrapped.Definitions, wrapped.Recursive, wrapped.Body, meter, token).Columns;
        var target = statement switch { InsertSyntax insert => insert.Table, UpdateSyntax update => update.Table, DeleteSyntax delete => delete.Table, _ => null };
        if ((catalog.Views ?? []).FirstOrDefault(view => view.Name == target) is { } view)
            statement = StatementExecution.RewriteView(database, pending ?? new PendingDatabase(snapshot) { Catalog = catalog }, statement, view, meter, token);
        CatalogTable TargetTable(string name) => localDomain && DatabaseDomains.Name(name).Domain is null ? StatementExecution.Table(catalog, name) : StatementExecution.ResolveTable(catalog, name);
        BoundColumn[] Query(SelectSyntax query) => RelationExecution.Query(query, database, snapshot, pending, catalog, meter, token, scope: scope).Columns;
        BoundColumn[] Returning(CatalogTable table, SelectItem[] items, BoundColumn[]? viewColumns = null, ReturningAliases? aliases = null)
        {
            StatementExecution.ValidateReturning(items);
            var binder = new SubqueryBinder(viewColumns ?? StatementExecution.ReturningColumns(table, aliases), database, snapshot, pending, catalog, meter, token, scope);
            return Resolve.Select(new(items.Select(item => item.Expression.Op == "star" ? item : item with { Expression = binder.Bind(item.Expression) }).ToArray(), null, null, 0), StatementExecution.ReturningColumns(table, aliases)).Projections.Select(projection => new BoundColumn(projection.Name, projection.Expression.Type, Collation: projection.Expression.Collation?.Implicit())).ToArray();
        }
        void Assign(Expression expression, CatalogColumn column, SubqueryBinder binder)
        {
            if (expression.Op == "default") return;
            if (expression.Parameter is { } slot) slot.NotNull |= column.NotNull;
            StatementExecution.Assignable(Resolve.Expr(binder.Bind(expression), column.Type.Type).Type, column.Type.Type);
        }
        switch (statement)
        {
            case SelectSyntax query: return Query(query);
            case MergeSyntax merge: return StatementExecution.PrepareMerge(database, pending ?? new PendingDatabase(snapshot) { Catalog = catalog }, merge, meter, token, scope is null ? null : new StatementContext(pending ?? new PendingDatabase(snapshot) { Catalog = catalog }, scope)).Columns;
            case CreateTableAsSyntax create: return pending is not null && WritableCteExecution.HasWrites(create.Query)
                ? WritableCteExecution.Prepare(database, pending, create.Query.With!, create.Query.Recursive, create.Query with { With = null }, meter, token).Columns : Query(create.Query);
            case InsertSyntax insert:
            {
                var table = TargetTable(insert.Table); var binder = new SubqueryBinder([], database, snapshot, pending, catalog, meter, token, scope);
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
                    var sourceColumns = Query(source);
                    if (sourceColumns.Length != targets.Length) throw Errors.Syntax("INSERT source width differs from target");
                    for (var i = 0; i < targets.Length; i++) StatementExecution.Assignable(sourceColumns[i].Type, targets[i].Type.Type);
                }
                if (insert.Conflict is { } conflict)
                {
                    var columns = insert.ViewBinding?.Conflict ?? StatementExecution.Columns(table, insert.Alias).Concat(table.Columns.Select(column => new BoundColumn(column.Name, column.Type.Type, "excluded", Hidden: true, Collation: CollationRules.Collatable(column.Type.Type) ? new(Name: column.Collation ?? "C", Table: column.CollationData) : null, WholeRow: false))).ToArray();
                    var conflictBinder = new SubqueryBinder(columns, database, snapshot, pending, catalog, meter, token, scope);
                    foreach (var assignment in conflict.Assignments ?? []) Assign(assignment.Value, table.Columns.FirstOrDefault(column => column.Name == assignment.Column) ?? throw new SqlException(SqlStates.UndefinedColumn, "column does not exist"), conflictBinder);
                    if (conflict.Filter is { } filter) _ = Resolve.Expr(conflictBinder.Bind(filter), SqlType.Boolean);
                }
                return Returning(insert.Alias is { } alias ? table with { Name = alias } : table, insert.Returning, insert.ViewBinding?.Returning, insert.ReturningAliases);
            }
            case UpdateSyntax update:
            {
                var table = TargetTable(update.Table); var binder = new SubqueryBinder(update.ViewBinding?.Columns ?? StatementExecution.Columns(table), database, snapshot, pending, catalog, meter, token, scope);
                foreach (var assignment in update.Assignments) Assign(assignment.Value, table.Columns.FirstOrDefault(column => column.Name == assignment.Column) ?? throw new SqlException(SqlStates.UndefinedColumn, "column does not exist"), binder);
                if (update.Filter is { } filter) _ = Resolve.Expr(binder.Bind(filter), SqlType.Boolean);
                return Returning(table, update.Returning, update.ViewBinding?.Returning, update.ReturningAliases);
            }
            case DeleteSyntax delete:
            {
                var table = TargetTable(delete.Table); var binder = new SubqueryBinder(delete.ViewBinding?.Columns ?? StatementExecution.Columns(table), database, snapshot, pending, catalog, meter, token, scope);
                if (delete.Filter is { } filter) _ = Resolve.Expr(binder.Bind(filter), SqlType.Boolean);
                return Returning(table, delete.Returning, delete.ViewBinding?.Returning, delete.ReturningAliases);
            }
        }
        return [];
    }
}
