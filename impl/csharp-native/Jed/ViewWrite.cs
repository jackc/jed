namespace Jed;

internal sealed record ViewWritePlan(CatalogTable Table, CatalogView View, Resolved[] Projection, Resolved? Filter, Resolved[] Checks);

internal static partial class StatementExecution
{
    private static Resolved ReplaceColumns(Resolved expression, Resolved[] columns, int offset = 0) => expression.Op == "column"
        ? ShiftColumns(columns[expression.Slot], offset)
        : expression with { Args = expression.Args.Select(argument => ReplaceColumns(argument, columns, offset)).ToArray() };
    private static Resolved ShiftColumns(Resolved expression, int offset) => expression.Op == "column" ? expression with { Slot = expression.Slot + offset } :
        expression with { Args = expression.Args.Select(argument => ShiftColumns(argument, offset)).ToArray() };
    private static ViewWritePlan PlanViewWrite(Database database, PendingDatabase pending, CatalogView view, string operation, Meter meter, CancellationToken token, int depth = 0)
    {
        if (depth > 128) throw new SqlException(SqlStates.StatementTooComplex, "view write nesting limit");
        var body = ViewQuery(view);
        if (body.Relation is not TableRelation source || body.Distinct || body.SetQuery || body.With is { Length: > 0 } || body.Limit is not null || body.Offset != 0 ||
            body.GroupBy is { Length: > 0 } || body.Having is not null || body.Items.Any(item => AggregateExpression(item.Expression) || WindowResolve.Contains(item.Expression)))
            throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "view is not automatically updatable");
        _ = RelationExecution.Query(body, database, pending.Before, pending, pending.Catalog, meter, token);
        ViewWritePlan? parent = null; CatalogTable table; Resolved[] inputs; BoundColumn[] columns;
        if ((pending.Catalog.Views ?? []).FirstOrDefault(item => item.Name == source.Name) is { } baseView)
        {
            parent = PlanViewWrite(database, pending, baseView, operation, meter, token, depth + 1); table = parent.Table; inputs = parent.Projection;
            columns = baseView.Columns.Select((column, i) => new BoundColumn(source.ColumnAliases is { } aliases && i < aliases.Length ? aliases[i] : column.Name, inputs[i].Type, source.Alias ?? source.Name)).ToArray();
        }
        else
        {
            table = Table(pending.Catalog, source.Name); columns = table.Columns.Select((column, i) => new BoundColumn(source.ColumnAliases is { } aliases && i < aliases.Length ? aliases[i] : column.Name, column.Type.Type, source.Alias ?? source.Name)).ToArray();
            inputs = columns.Select((column, i) => new Resolved("column", column.Type, default, [], Slot: i)).ToArray();
        }
        var syntax = Resolve.BindTypes(body, pending.Catalog, meter.Temporal);
        var selected = Resolve.Select(syntax with { Order = null }, columns);
        var projection = selected.Projections.Select(item => ReplaceColumns(item.Expression, inputs)).ToArray();
        if (operation != "delete" && !projection.Any(item => item.Op == "column")) throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "view has no updatable columns");
        var ownFilter = selected.Filter is null ? null : ReplaceColumns(selected.Filter, inputs);
        var filter = parent?.Filter is null ? ownFilter : ownFilter is null ? parent.Filter : new("and", SqlType.Boolean, default, [parent.Filter, ownFilter]);
        var checks = (parent?.Checks ?? []).ToList();
        if (view.CheckOption != 0)
        {
            var check = view.CheckOption == 2 ? filter : ownFilter;
            if (check is not null) checks.Add(check);
        }
        return new(table, view, projection, filter, checks.ToArray());
    }
    private static async ValueTask<StatementResult> WriteView(Database database, PendingDatabase pending, StatementSyntax statement, CatalogView view, Meter meter, CancellationToken token)
    {
        var kind = statement is InsertSyntax ? "insert" : statement is UpdateSyntax ? "update" : "delete";
        var plan = PlanViewWrite(database, pending, view, kind, meter, token);
        var label = statement is InsertSyntax { Alias: { } alias } ? alias : view.Name;
        var viewColumns = view.Columns.Select((column, i) => new BoundColumn(column.Name, plan.Projection[i].Type, label)).ToArray();
        var pseudo = new CatalogTable(label, viewColumns.Select(column => new CatalogColumn(column.Name, new(column.Type))).ToArray(), []);
        var baseWidth = plan.Table.Columns.Length; var viewWidth = viewColumns.Length;
        int Target(string name, bool conflict = false)
        {
            var slot = Array.FindIndex(view.Columns, column => column.Name == name);
            if (slot < 0) throw new SqlException(SqlStates.UndefinedColumn, "view column does not exist");
            if (plan.Projection[slot].Op != "column") throw new SqlException(conflict ? SqlStates.InvalidColumnReference : SqlStates.FeatureNotSupported, "view column is not updatable");
            return plan.Projection[slot].Slot;
        }
        Expression Rewrite(Expression expression, bool returning = false, bool excluded = false)
        {
            if (expression.Query is not null) throw Errors.Unsupported("correlated subquery in view write expression");
            if (expression.Op == "default") return expression;
            var columns = returning ? ReturningColumns(pseudo) : excluded ? viewColumns.Concat(viewColumns.Select(column => column with { Qualifier = "excluded", Hidden = true })).ToArray() : viewColumns;
            var bound = Resolve.Bind(Resolve.BindTypes(expression, pending.Catalog, meter.Temporal), columns);
            Expression Expand(Expression node)
            {
                if (node.Binding is { Op: "column" } reference)
                {
                    var offset = reference.Slot / viewWidth * baseWidth;
                    return node with { Binding = ShiftColumns(plan.Projection[reference.Slot % viewWidth], offset) };
                }
                return node with { Args = node.Args.Select(Expand).ToArray(), Predicate = node.Predicate is null ? null : Expand(node.Predicate) };
            }
            return Expand(bound);
        }
        SelectItem[] Returning(SelectItem[] items)
        {
            var expanded = new List<SelectItem>();
            foreach (var item in items)
            {
                if (item.Expression.Op != "star") { expanded.Add(item with { Expression = Rewrite(item.Expression, returning: true) }); continue; }
                var qualifier = item.Expression.Text;
                if (qualifier.Length != 0 && qualifier != label && qualifier != "old" && qualifier != "new") throw new SqlException(SqlStates.UndefinedTable, "RETURNING relation does not exist");
                foreach (var column in view.Columns)
                    expanded.Add(new(Rewrite(new("name", (qualifier.Length == 0 ? "" : qualifier + ".") + column.Name, []), returning: true), column.Name));
            }
            return expanded.ToArray();
        }
        Expression Default(string name, bool inserting = false)
        {
            var column = view.Columns.Single(column => column.Name == name);
            return column.DefaultExpression is { } text ? StoredExpression(text) : new(inserting ? "default" : "null", "", []);
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
                var rows = insert.Values.Select(row => row.Select((value, i) => value.Op == "default" ? Default(targets[i], inserting: true) : value).ToArray()).ToArray();
                var conflict = insert.Conflict;
                if (conflict is not null) conflict = conflict with {
                    Elements = conflict.Elements?.Select(element => element with { Column = plan.Table.Columns[Target(element.Column, true)].Name }).ToArray(),
                    Assignments = conflict.Assignments?.Select(assignment => new AssignmentSyntax(plan.Table.Columns[Target(assignment.Column)].Name, assignment.Value.Op == "default" ? Default(assignment.Column) : Rewrite(assignment.Value, excluded: true))).ToArray(),
                    Filter = conflict.Filter is null ? null : Rewrite(conflict.Filter, excluded: true)
                };
                rewritten = insert with { Table = plan.Table.Name, Alias = label, Columns = mapped, Values = rows, Conflict = conflict, Returning = Returning(insert.Returning), ViewChecks = checks };
                break;
            }
            case UpdateSyntax update:
                rewritten = update with { Table = plan.Table.Name, Assignments = update.Assignments.Select(assignment => new AssignmentSyntax(plan.Table.Columns[Target(assignment.Column)].Name, assignment.Value.Op == "default" ? Default(assignment.Column) : Rewrite(assignment.Value))).ToArray(),
                    Filter = Filter(update.Filter), Returning = Returning(update.Returning), ViewChecks = checks }; break;
            case DeleteSyntax delete:
                rewritten = delete with { Table = plan.Table.Name, Filter = Filter(delete.Filter), Returning = Returning(delete.Returning) }; break;
            default: throw new InvalidOperationException();
        }
        return await Execute(database, pending, rewritten, meter, token).ConfigureAwait(false);
    }
    private static void ValidateViewChecks(Expression[]? checks, SqlValue[] values, Meter meter)
    {
        foreach (var check in checks ?? [])
        {
            var result = Evaluate.Expr(Resolve.Expr(check), meter, row: values);
            if (result.IsNull || !result.AsBoolean()) throw new SqlException(SqlStates.WithCheckOptionViolation, "new row violates view check option");
        }
    }
}
