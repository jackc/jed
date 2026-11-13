namespace Jed;

internal sealed record ViewBinding(BoundColumn[] Columns, BoundColumn[] Returning, BoundColumn[] Conflict);

internal sealed record ViewWritePlan(CatalogTable Table, CatalogView View, Resolved[] Projection, Resolved? Filter, Resolved[] Checks, Dictionary<int, Expression> Defaults, Resolved[] Filters);

internal static partial class StatementExecution
{
    private static Resolved ShiftColumns(Resolved expression, int offset) => expression.Op == "column" ? expression with { Slot = expression.Slot + offset } :
        expression with { Args = expression.Args.Select(argument => ShiftColumns(argument, offset)).ToArray() };
    private static ViewWritePlan PlanViewWrite(Database database, PendingDatabase pending, CatalogView view, string operation, Meter meter, CancellationToken token, int depth = 0)
    {
        if (depth > 128) throw new SqlException(SqlStates.StatementTooComplex, "view write nesting limit");
        var body = ViewQuery(view);
        if (body.Relation is not TableRelation source || body.Distinct || body.SetQuery || body.With is { Length: > 0 } || body.Limit is not null || body.Offset != 0 ||
            body.GroupBy is { Length: > 0 } || body.Having is not null || body.Items.Any(item => AggregateExpression(item.Expression) || WindowResolve.Contains(item.Expression)))
            throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "view is not automatically updatable");
        if (CatalogRelations.IsCatalogName(DatabaseDomains.Name(source.Name).Local))
            throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "catalog-backed view is not automatically updatable");
        _ = RelationExecution.Query(body, database, pending.Before, pending, pending.Catalog, meter, token);
        ViewWritePlan? parent = null; CatalogTable table; Resolved[] inputs; BoundColumn[] columns;
        if ((pending.Catalog.Scope?.View(source.Name)?.View ?? (pending.Catalog.Views ?? []).FirstOrDefault(item => item.Name == source.Name)) is { } baseView)
        {
            parent = PlanViewWrite(database, pending, baseView, operation, meter, token, depth + 1); table = parent.Table; inputs = parent.Projection;
            columns = baseView.Columns.Select((column, i) => new BoundColumn(source.ColumnAliases is { } aliases && i < aliases.Length ? aliases[i] : column.Name, inputs[i].Type, source.Alias ?? source.Name, Collation: inputs[i].Collation, WholeRow: false)).ToArray();
        }
        else
        {
            table = ResolveTable(pending.Catalog, source.Name); columns = table.Columns.Select((column, i) => new BoundColumn(source.ColumnAliases is { } aliases && i < aliases.Length ? aliases[i] : column.Name, column.Type.Type, source.Alias ?? source.Name, Collation: CollationRules.Collatable(column.Type.Type) ? new(Name: column.Collation ?? "C", Table: column.CollationData) : null, WholeRow: false)).ToArray();
            inputs = columns.Select((column, i) => new Resolved("column", column.Type, default, [], Slot: i, Collation: column.Collation)).ToArray();
        }
        columns = columns.Select((column, slot) => column with { Projection = inputs[slot] }).ToArray();
        var binder = new SubqueryBinder(columns, database, pending.Before, pending.Fork(), pending.Catalog, meter, token);
        var syntax = Resolve.BindTypes(body, pending.Catalog, meter.Temporal);
        syntax = syntax with { Items = syntax.Items.Select(item => item.Expression.Op == "star" ? item : item with { Expression = binder.Bind(item.Expression) }).ToArray(),
            Filter = syntax.Filter is null ? null : binder.Bind(syntax.Filter), Order = null };
        var selected = Resolve.Select(syntax, columns);
        var projection = selected.Projections.Select(item => item.Expression).ToArray();
        if (operation != "delete" && !projection.Any(item => item.Op == "column")) throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "view has no updatable columns");
        var ownFilter = selected.Filter;
        var filter = parent?.Filter is null ? ownFilter : ownFilter is null ? parent.Filter : new("and", SqlType.Boolean, default, [parent.Filter, ownFilter]);
        var checks = (view.CheckOption == 2 ? parent?.Filters ?? [] : parent?.Checks ?? []).ToList();
        if (view.CheckOption != 0 && ownFilter is not null) checks.Add(ownFilter);
        var filters = (parent?.Filters ?? []).ToList();
        if (ownFilter is not null) filters.Add(ownFilter);
        var defaults = new Dictionary<int, Expression>(parent?.Defaults ?? []);
        for (var i = 0; i < view.Columns.Length; i++)
            if (view.Columns[i].DefaultExpression is { } text)
            {
                if (projection[i].Op != "column")
                {
                    if (operation == "insert") throw Errors.Unsupported("default targets a read-only view column");
                    continue;
                }
                defaults[projection[i].Slot] = StoredExpression(text);
            }
        return new(table, view, projection, filter, checks.ToArray(), defaults, filters.ToArray());
    }
    private static ValueTask<StatementResult> WriteView(Database database, PendingDatabase pending, StatementSyntax statement, CatalogView view, Meter meter, CancellationToken token, StatementContext? context = null)
    {
        var rewritten = RewriteView(database, context?.Read ?? pending, statement, view, meter, token);
        return pending.Catalog.Scope is null ? ExecuteLocal(database, pending, rewritten, meter, token, context) : ExecuteDomain(database, pending, rewritten, meter, token, context);
    }

    internal static StatementSyntax RewriteView(Database database, PendingDatabase pending, StatementSyntax statement, CatalogView view, Meter meter, CancellationToken token)
    {
        var kind = statement is InsertSyntax ? "insert" : statement is UpdateSyntax or MergeSyntax ? "update" : "delete";
        // A rewritten predicate can retain correlated subqueries until rows are
        // checked. Freeze every domain, not just the target's pending tree.
        var frozen = pending.Catalog.Scope is { } domains
            ? DomainExecution.ReadPending(domains.Fork(), domains.Items.First(domain => ReferenceEquals(domain.Database, database)).Name)
            : pending.Fork();
        var plan = PlanViewWrite(database, frozen, view, kind, meter, token);
        var baseName = plan.Table.Domain is { } home ? DatabaseDomains.Qualified(home.Name, plan.Table.Name) : plan.Table.Name;
        var label = statement switch { InsertSyntax { Alias: { } alias } => alias, MergeSyntax { Alias: { } alias } => alias, _ => view.Name };
        var viewColumns = view.Columns.Select((column, i) => new BoundColumn(column.Name, plan.Projection[i].Type, label, Collation: plan.Projection[i].Collation, WholeRow: false)).ToArray();
        var pseudo = new CatalogTable(label, viewColumns.Select(column => new CatalogColumn(column.Name, new(column.Type))).ToArray(), []);
        var baseWidth = plan.Table.Columns.Length; var viewWidth = viewColumns.Length;
        int Target(string name, bool conflict = false)
        {
            var slot = Array.FindIndex(view.Columns, column => column.Name == name);
            if (slot < 0) throw new SqlException(SqlStates.UndefinedColumn, "view column does not exist");
            if (plan.Projection[slot].Op != "column") throw new SqlException(conflict ? SqlStates.InvalidColumnReference : SqlStates.FeatureNotSupported, "view column is not updatable");
            return plan.Projection[slot].Slot;
        }
        BoundColumn[] Scope(BoundColumn[] columns) => columns.Select((column, slot) => column with
        {
            WholeRow = false, Projection = ShiftColumns(plan.Projection[slot % viewWidth], slot / viewWidth * baseWidth)
        }).ToArray();
        var aliases = statement switch { InsertSyntax insert => insert.ReturningAliases, UpdateSyntax update => update.ReturningAliases, DeleteSyntax delete => delete.ReturningAliases, _ => null };
        var binding = new ViewBinding(Scope(viewColumns), Scope(ReturningColumns(pseudo, aliases)),
            Scope(viewColumns.Concat(viewColumns.Select(column => column with { Qualifier = "excluded", Hidden = true })).ToArray()));
        Expression Rewrite(Expression expression, bool returning = false, bool excluded = false) =>
            Resolve.BindTypes(expression, pending.Catalog, meter.Temporal);
        SelectItem[] Returning(SelectItem[] items)
        {
            var expanded = new List<SelectItem>();
            foreach (var item in items)
            {
                if (item.Expression.Op != "star") { expanded.Add(item with { Expression = Rewrite(item.Expression, returning: true) }); continue; }
                var qualifier = item.Expression.Text;
                if (qualifier.Length != 0 && !binding.Returning.Any(column => column.Qualifier == qualifier)) throw new SqlException(SqlStates.UndefinedTable, "RETURNING relation does not exist");
                foreach (var column in view.Columns)
                    expanded.Add(new(Rewrite(new("name", (qualifier.Length == 0 ? "" : qualifier + ".") + column.Name, []), returning: true), column.Name));
            }
            return expanded.ToArray();
        }
        Expression Default(string name, bool inserting = false)
        {
            var column = view.Columns.Single(column => column.Name == name);
            return inserting && plan.Defaults.TryGetValue(Target(name), out var inherited) ? inherited : column.DefaultExpression is { } text ? StoredExpression(text) : new(inserting ? "default" : "null", "", []);
        }
        Expression? Filter(Expression? expression)
        {
            var rewritten = expression is null ? null : Rewrite(expression);
            if (plan.Filter is null) return rewritten;
            var visible = new Expression("bound", "", [], Binding: plan.Filter);
            return rewritten is null ? visible : new("and", "", [visible, rewritten]);
        }
        var checks = plan.Checks.Select(check => new Expression("bound", "", [], Binding: check)).ToArray();
        StatementSyntax rewritten;
        switch (statement)
        {
            case InsertSyntax insert:
            {
                if (insert.Conflict?.Predicate is not null) throw Errors.Unsupported("partial conflict inference through view");
                if (insert.Conflict?.Constraint is not null) throw new SqlException(SqlStates.UndefinedObject, "view has no constraints");
                var defaultValues = insert.Source is null && insert.Values is [{ Length: 0 }];
                var targets = insert.Columns ?? (defaultValues ? [] : view.Columns.Select(column => column.Name).ToArray());
                var mapped = targets.Select(name => plan.Table.Columns[Target(name)].Name).ToArray();
                if (mapped.Distinct(StringComparer.Ordinal).Count() != mapped.Length) throw new SqlException(SqlStates.DuplicateColumn, "view targets assign the same base column");
                foreach (var row in insert.Values) if (!defaultValues && row.Length != targets.Length) throw Errors.Syntax("INSERT value count differs from target");
                var rows = insert.Values.Select(row => row.Select((value, i) => value.Op == "default" ? Default(targets[i], inserting: true) : value).ToArray()).ToArray();
                var conflict = insert.Conflict;
                foreach (var element in conflict?.Elements ?? [])
                {
                    if (element.Ordered) throw new SqlException(SqlStates.InvalidColumnReference, "ordering is not allowed in a conflict target");
                    if (!view.Columns.Any(column => column.Name == element.Column)) throw new SqlException(SqlStates.UndefinedColumn, "view conflict column does not exist");
                    if (element.Collation is { } collation && collation != "C") _ = meter.Unicode?.Resolve(collation) ?? throw new SqlException(SqlStates.UndefinedObject, "collation does not exist");
                    if (element.Opclass is { } opclass && !KnownBtreeClass(opclass)) throw new SqlException(SqlStates.UndefinedObject, "operator class does not exist");
                }
                if (conflict is not null) conflict = conflict with {
                    Elements = conflict.Elements?.Select(element => element with { Column = plan.Table.Columns[Target(element.Column, true)].Name }).ToArray(),
                    Assignments = conflict.Assignments?.Select(assignment => new AssignmentSyntax(plan.Table.Columns[Target(assignment.Column)].Name, assignment.Value.Op == "default" ? Default(assignment.Column) : Rewrite(assignment.Value, excluded: true))).ToArray(),
                    Filter = conflict.Filter is null ? null : Rewrite(conflict.Filter, excluded: true)
                };
                rewritten = insert with { Table = baseName, Alias = label, Columns = mapped, Values = rows, Conflict = conflict, Returning = Returning(insert.Returning), ViewChecks = checks, ViewBinding = binding, ViewDefaults = plan.Defaults };
                break;
            }
            case MergeSyntax merge:
                rewritten = merge with { Table = baseName, Alias = label, View = plan, Clauses = merge.Clauses.Select(clause => clause with
                {
                    Assignments = clause.Assignments?.Select(assignment => new AssignmentSyntax(plan.Table.Columns[Target(assignment.Column)].Name,
                        assignment.Value.Op == "default" ? Default(assignment.Column) : assignment.Value)).ToArray(),
                    Columns = clause.Action == "insert" ? (clause.Columns ?? view.Columns.Select(column => column.Name).ToArray()).Take(clause.Values!.Length).Select(name => plan.Table.Columns[Target(name)].Name).ToArray() : clause.Columns
                }).ToArray() }; break;
            case UpdateSyntax update:
                rewritten = update with { Table = baseName, Assignments = update.Assignments.Select(assignment => new AssignmentSyntax(plan.Table.Columns[Target(assignment.Column)].Name, assignment.Value.Op == "default" ? Default(assignment.Column) : Rewrite(assignment.Value))).ToArray(),
                    Filter = Filter(update.Filter), Returning = Returning(update.Returning), ViewChecks = checks, ViewBinding = binding }; break;
            case DeleteSyntax delete:
                rewritten = delete with { Table = baseName, Filter = Filter(delete.Filter), Returning = Returning(delete.Returning), ViewBinding = binding }; break;
            default: throw new InvalidOperationException();
        }
        return rewritten;
    }
    private static async ValueTask ValidateViewChecks(Expression[]? checks, SqlValue[] values, Meter meter)
    {
        foreach (var check in checks ?? [])
        {
            var result = await Evaluate.ExprAsync(Resolve.Expr(check), meter, filter: true, row: values).ConfigureAwait(false);
            if (result.IsNull || !result.AsBoolean()) throw new SqlException(SqlStates.WithCheckOptionViolation, "new row violates view check option");
        }
    }
}
