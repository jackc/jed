namespace Jed;

[Flags]
public enum TablePrivileges { None = 0, Select = 1, Insert = 2, Update = 4, Delete = 8, All = 15 }

public sealed partial class Session
{
    private TablePrivileges defaultPrivileges = TablePrivileges.All;
    private readonly Dictionary<string, TablePrivileges> tableGrants = new(StringComparer.Ordinal);
    private readonly Dictionary<string, TablePrivileges> tableRevokes = new(StringComparer.Ordinal);
    private readonly HashSet<string> functionRevokes = new(StringComparer.Ordinal);
    public void SetDefaultPrivileges(TablePrivileges privileges)
    {
        if ((privileges & ~TablePrivileges.All) != 0) throw new ArgumentOutOfRangeException(nameof(privileges));
        using var operation = Enter(); defaultPrivileges = privileges;
    }
    public void Grant(string table, TablePrivileges privileges) => ChangePrivileges(table, privileges, tableGrants);
    public void Revoke(string table, TablePrivileges privileges) => ChangePrivileges(table, privileges, tableRevokes);
    private void ChangePrivileges(string table, TablePrivileges privileges, Dictionary<string, TablePrivileges> target)
    {
        ArgumentNullException.ThrowIfNull(table);
        if ((privileges & ~TablePrivileges.All) != 0) throw new ArgumentOutOfRangeException(nameof(privileges));
        using var operation = Enter(); target[table] = target.GetValueOrDefault(table) | privileges;
    }
    public void RevokeExecute(string function) { ArgumentNullException.ThrowIfNull(function); using var operation = Enter(); functionRevokes.Add(function); }
    // EXECUTE defaults on; a grant cannot override a revoke in the same envelope.
    public void GrantExecute(string function) { ArgumentNullException.ThrowIfNull(function); using var operation = Enter(); }
    public void ResetPrivileges()
    {
        using var operation = Enter(); defaultPrivileges = options.DefaultPrivileges;
        tableGrants.Clear(); tableRevokes.Clear(); functionRevokes.Clear();
        allowDdl = options.AllowDdl; allowTempDdl = options.AllowTempDdl ?? options.AllowDdl;
    }
    private void CheckPrivileges(StatementSyntax statement, Catalog catalog)
    {
        if (defaultPrivileges == TablePrivileges.All && tableRevokes.Count == 0 && functionRevokes.Count == 0) return;
        new PrivilegeDiscovery(
        catalog, (name, required) =>
        {
            var allowed = (defaultPrivileges | tableGrants.GetValueOrDefault(name)) & ~tableRevokes.GetValueOrDefault(name);
            if ((allowed & required) != required) throw new SqlException("42501", "permission denied for table " + name);
        }, name => { if (functionRevokes.Contains(name)) throw new SqlException("42501", "permission denied for function " + name); }).Statement(statement, [], true, 0);
    }
}

// Static preflight captures authorization at submission, before any evaluation or
// pending mutation. Cursor drain never consults the session's mutable envelope.
internal sealed class PrivilegeDiscovery(Catalog catalog, Action<string, TablePrivileges> tableCheck, Action<string> functionCheck)
{
    private readonly HashSet<string> activeViews = new(StringComparer.Ordinal);
    private int expandedViews;
    internal void Statement(StatementSyntax statement, HashSet<string> visible, bool tables, int depth)
    {
        if (depth > 256) throw new SqlException(SqlStates.StatementTooComplex, "privilege discovery nesting exceeds 256");
        switch (statement)
        {
            case ExplainSyntax explain: Statement(explain.Inner, visible, tables, depth + 1); break;
            case ReindexSyntax reindex when reindex.Kind is "table" or "index":
                var indexed = Lookup(reindex.Name!);
                var owningTable = indexed.Local.Tables.FirstOrDefault(table => reindex.Kind == "table" ? table.Name == indexed.Name : (table.Indexes ?? []).Any(index => index.Name == indexed.Name));
                if (tables && owningTable is not null) tableCheck(owningTable.Name, TablePrivileges.Select);
                break;
            case AnalyzeSyntax analyze: Relation(new TableRelation(analyze.Table), visible, tables, depth); break;
            case SelectSyntax query: Query(query, visible, tables, depth); break;
            case WithStatementSyntax with:
                var names = Definitions(with.Definitions, with.Recursive, visible, tables, depth);
                Statement(with.Body, names, tables, depth + 1); break;
            case CreateTableAsSyntax create when create.WithData: Query(create.Query, visible, tables, depth + 1); break;
            case CreateViewSyntax create: Query(create.Query, visible, tables, depth + 1); break;
            case AlterViewSyntax alter:
                var altered = Lookup(alter.Name);
                if ((altered.Local.Views ?? []).FirstOrDefault(view => view.Name == altered.Name) is { } previous)
                {
                    var invoker = previous.SecurityInvoker;
                    foreach (var action in alter.Actions ?? [alter]) foreach (var option in action.Options ?? [])
                    {
                        if (option.Name != "security_invoker") continue;
                        var text = TextEncoding.FoldName(option.Value);
                        if (text is not ("true" or "on" or "1" or "yes" or "false" or "off" or "0" or "no" or "reset")) continue;
                        var next = text is "true" or "on" or "1" or "yes";
                        if (invoker && !next) Expand(previous, altered.Local, visible, tables, depth);
                        invoker = next;
                    }
                }
                break;
            case InsertSyntax insert:
                var insertRead = insert.Conflict?.Elements is not null || insert.Conflict?.Constraint is not null || insert.Returning.Any(item => Reads(item.Expression));
                var insertOps = TablePrivileges.Insert | (insertRead ? TablePrivileges.Select : 0) | (insert.Conflict?.Assignments is not null ? TablePrivileges.Update : 0);
                Target(insert.Table, insertOps, statement, visible, tables, depth);
                foreach (var row in insert.Values) foreach (var expression in row) Expr(expression, visible, tables, depth);
                if (insert.Source is { } source) Query(source, visible, tables, depth + 1);
                foreach (var item in insert.Returning) Expr(item.Expression, visible, tables, depth);
                foreach (var assignment in insert.Conflict?.Assignments ?? []) Expr(assignment.Value, visible, tables, depth);
                Expr(insert.Conflict?.Filter, visible, tables, depth); Expr(insert.Conflict?.Predicate, visible, tables, depth); break;
            case UpdateSyntax update:
                var updateRead = Reads(update.Filter) || update.Assignments.Any(item => Reads(item.Value)) || update.Returning.Any(item => Reads(item.Expression));
                Target(update.Table, TablePrivileges.Update | (updateRead ? TablePrivileges.Select : 0), statement, visible, tables, depth);
                foreach (var assignment in update.Assignments) Expr(assignment.Value, visible, tables, depth);
                Expr(update.Filter, visible, tables, depth); foreach (var item in update.Returning) Expr(item.Expression, visible, tables, depth); break;
            case DeleteSyntax delete:
                var deleteRead = Reads(delete.Filter) || delete.Returning.Any(item => Reads(item.Expression));
                Target(delete.Table, TablePrivileges.Delete | (deleteRead ? TablePrivileges.Select : 0), statement, visible, tables, depth);
                Expr(delete.Filter, visible, tables, depth); foreach (var item in delete.Returning) Expr(item.Expression, visible, tables, depth); break;
        }
    }
    private static bool Reads(Expression? expression) => expression is not null &&
        (expression.Op is "name" or "star" || expression.Query is not null || expression.Args.Any(Reads) || Reads(expression.Predicate));
    private (Catalog Local, string Name, string Key) Lookup(string name)
    {
        var local = DatabaseDomains.Name(name).Local;
        if (catalog.Scope is not { } scope) return (catalog, local, local);
        var domain = scope.Resolve(name);
        return (domain.Catalog with { Scope = scope.Stored(domain.Name) }, local, local);
    }
    private void Target(string name, TablePrivileges required, StatementSyntax statement, HashSet<string> visible, bool tables, int depth)
    {
        if (depth > 256) throw new SqlException(SqlStates.StatementTooComplex, "view privilege nesting exceeds 256");
        var found = Lookup(name); var table = found.Local.Tables.FirstOrDefault(item => item.Name == found.Name);
        var view = (found.Local.Views ?? []).FirstOrDefault(item => item.Name == found.Name);
        if (table is null && view is null) throw new SqlException(SqlStates.UndefinedTable, "table does not exist: " + found.Name);
        if (tables) tableCheck(found.Key, required);
        if (view is not null)
        {
            var query = StatementExecution.ViewQuery(view);
            Expand(view, found.Local, visible, false, depth);
            if (tables && view.SecurityInvoker && statement is not InsertSyntax) Expr(query.Filter, visible, true, depth);
            if (query.Relation is TableRelation relation)
            {
                var baseStatement = MapViewWrite(statement, view, query, relation, found.Local, visible, depth);
                var next = new PrivilegeDiscovery(found.Local, tableCheck, functionCheck);
                next.Target(relation.Name, required | (statement is not InsertSyntax && query.Filter is not null ? TablePrivileges.Select : 0), baseStatement,
                    visible, tables && view.SecurityInvoker, depth + 1);
            }
            return;
        }
        if (statement is DeleteSyntax) return;
        foreach (var check in table!.Checks ?? []) Stored(check.Expression, visible, depth);
        var assignedDefaults = statement switch
        {
            UpdateSyntax update => update.Assignments.Where(item => item.Value.Op == "default").Select(item => item.Column).ToHashSet(StringComparer.Ordinal),
            _ => new HashSet<string>(StringComparer.Ordinal)
        };
        if (statement is InsertSyntax insertStatement)
        {
            var supplied = insertStatement.Columns ?? table.Columns.Select(column => column.Name).ToArray();
            foreach (var column in table.Columns)
            {
                var index = Array.IndexOf(supplied, column.Name);
                if (index < 0 || insertStatement.Source is null && (insertStatement.Values.Length == 0 || insertStatement.Values.Any(row => index >= row.Length || row[index].Op == "default"))) assignedDefaults.Add(column.Name);
            }
            foreach (var assignment in insertStatement.Conflict?.Assignments ?? []) if (assignment.Value.Op == "default") assignedDefaults.Add(assignment.Column);
        }
        foreach (var column in table.Columns) if (assignedDefaults.Contains(column.Name) && column.DefaultExpression is { } expression) Stored(expression, visible, depth);
    }
    private StatementSyntax MapViewWrite(StatementSyntax statement, CatalogView view, SelectSyntax query, TableRelation relation, Catalog local, HashSet<string> visible, int depth)
    {
        var sourceCatalog = local.Scope is { } scope ? scope.Resolve(relation.Name).Catalog : local;
        var sourceName = DatabaseDomains.Name(relation.Name).Local;
        var sourceColumns = sourceCatalog.Tables.FirstOrDefault(table => table.Name == sourceName)?.Columns.Select(column => column.Name).ToArray() ??
            (sourceCatalog.Views ?? []).FirstOrDefault(item => item.Name == sourceName)?.Columns.Select(column => column.Name).ToArray() ?? [];
        string BaseName(string text)
        {
            var leaf = sourceColumns.Contains(text, StringComparer.Ordinal) ? text : text[(text.LastIndexOf('.') + 1)..];
            var alias = Array.IndexOf(relation.ColumnAliases ?? [], leaf);
            return alias >= 0 && alias < sourceColumns.Length ? sourceColumns[alias] : leaf;
        }
        var projection = query.Items.SelectMany(item => item.Expression.Op == "star" ? sourceColumns :
            new[] { item.Expression.Op == "name" ? BaseName(item.Expression.Text) : "" }).ToArray();
        string Map(string name)
        {
            var index = Array.FindIndex(view.Columns, column => column.Name == name);
            return index >= 0 && index < projection.Length && projection[index].Length != 0 ? projection[index] : name;
        }
        Expression? OwnDefault(string name)
        {
            var text = view.Columns.FirstOrDefault(column => column.Name == name)?.DefaultExpression;
            if (text is null) return null;
            var expression = new Parser("SELECT " + text).Parse().Items[0].Expression;
            Expr(expression, visible, true, depth + 1); return expression;
        }
        AssignmentSyntax Assign(AssignmentSyntax assignment) => new(Map(assignment.Column), assignment.Value.Op == "default" ?
            OwnDefault(assignment.Column) ?? new("null", "", []) : assignment.Value);
        if (statement is UpdateSyntax update) return update with { Table = relation.Name, Assignments = update.Assignments.Select(Assign).ToArray() };
        if (statement is DeleteSyntax delete) return delete with { Table = relation.Name };
        if (statement is not InsertSyntax insert) return statement;
        var supplied = insert.Columns ?? view.Columns.Select(column => column.Name).ToArray();
        var extra = view.Columns.Where(column => !supplied.Contains(column.Name, StringComparer.Ordinal) && column.DefaultExpression is not null).Select(column => column.Name).ToArray();
        var columns = supplied.Concat(extra).ToArray();
        var defaults = new Dictionary<string, Expression>(StringComparer.Ordinal);
        foreach (var name in extra) defaults[name] = OwnDefault(name)!;
        var values = insert.Values.Select(row => columns.Select((name, index) =>
        {
            var value = index < row.Length ? row[index] : new Expression("default", "", []);
            if (value.Op != "default") return value;
            Expression? replacement = defaults.GetValueOrDefault(name);
            if (replacement is null)
            {
                replacement = OwnDefault(name);
                if (replacement is not null) defaults[name] = replacement;
            }
            return replacement ?? value;
        }).ToArray()).ToArray();
        var conflict = insert.Conflict is { } action ? action with { Assignments = action.Assignments?.Select(Assign).ToArray() } : null;
        return insert with { Table = relation.Name, Columns = columns.Select(Map).ToArray(), Values = values, Conflict = conflict };
    }
    private void Stored(string text, HashSet<string> visible, int depth) => Expr(new Parser("SELECT " + text).Parse().Items[0].Expression, visible, true, depth + 1);
    private HashSet<string> Definitions(CteSyntax[] definitions, bool recursive, HashSet<string> inherited, bool tables, int depth)
    {
        var visible = new HashSet<string>(inherited, StringComparer.Ordinal);
        if (recursive) foreach (var definition in definitions) visible.Add(definition.Name);
        foreach (var definition in definitions)
        {
            if (definition.Mutation is { } mutation) Statement(mutation, visible, tables, depth + 1);
            else Query(definition.Query, visible, tables, depth + 1);
            visible.Add(definition.Name);
        }
        return visible;
    }
    private void Query(SelectSyntax query, HashSet<string> inherited, bool tables, int depth)
    {
        if (depth > 256) throw new SqlException(SqlStates.StatementTooComplex, "privilege discovery nesting exceeds 256");
        var visible = Definitions(query.With ?? [], query.Recursive, inherited, tables, depth);
        Relation(query.Relation, visible, tables, depth);
        foreach (var item in query.Items) Expr(item.Expression, visible, tables, depth);
        Expr(query.Filter, visible, tables, depth); Expr(query.Having, visible, tables, depth);
        foreach (var group in query.GroupBy ?? []) Expr(group, visible, tables, depth);
        foreach (var distinct in query.DistinctOn ?? []) Expr(distinct, visible, tables, depth);
        foreach (var order in query.Order ?? []) Expr(order.Expression, visible, tables, depth);
        if (query.Windows is { } windows) foreach (var window in windows.Values) Window(window, visible, tables, depth);
    }
    private void Expr(Expression? expression, HashSet<string> visible, bool tables, int depth)
    {
        if (expression is null) return;
        if (expression.Op is "call" or "call_distinct" or "ordered_aggregate" &&
            (FunctionCatalog.Facts.Any(fact => fact.Name == expression.Text) || AggregateResolve.IsAggregate(expression with { Window = null }) ||
             expression.Text is "row_number" or "rank" or "dense_rank" or "percent_rank" or "cume_dist" or "ntile" or "lag" or "lead" or "first_value" or "last_value" or "nth_value")) functionCheck(expression.Text);
        foreach (var argument in expression.Args) Expr(argument, visible, tables, depth);
        Expr(expression.Predicate, visible, tables, depth);
        foreach (var order in expression.AggregateOrder ?? []) Expr(order.Expression, visible, tables, depth);
        Window(expression.Window, visible, tables, depth);
        if (expression.Query is { } query) Query(query, visible, tables, depth + 1);
    }
    private void Window(WindowSyntax? window, HashSet<string> visible, bool tables, int depth)
    {
        if (window is null) return;
        foreach (var expression in window.Partition) Expr(expression, visible, tables, depth);
        foreach (var order in window.Order) Expr(order.Expression, visible, tables, depth);
        Expr(window.Frame?.Start.Offset, visible, tables, depth); Expr(window.Frame?.End.Offset, visible, tables, depth);
    }
    private void Expand(CatalogView view, Catalog local, HashSet<string> visible, bool tables, int depth)
    {
        if (!activeViews.Add(view.Name)) throw new SqlException(SqlStates.InvalidObjectDefinition, "recursive view definition");
        if (++expandedViews > 1024) throw new SqlException(SqlStates.StatementTooComplex, "view expansion exceeds 1024 references");
        try
        {
            // Share the expansion budget through this walker; temporarily use the
            // view's own namespace for unqualified body references.
            var before = catalog; catalog = local;
            try { Query(StatementExecution.ViewQuery(view), [], tables, depth + 1); }
            finally { catalog = before; }
        }
        finally { activeViews.Remove(view.Name); }
    }
    private void Relation(RelationSyntax? relation, HashSet<string> visible, bool tables, int depth)
    {
        switch (relation)
        {
            case TableRelation table when !visible.Contains(table.Name):
                var found = Lookup(table.Name);
                var view = (found.Local.Views ?? []).FirstOrDefault(item => item.Name == found.Name);
                if (view is null && !found.Local.Tables.Any(item => item.Name == found.Name) && !CatalogRelations.IsCatalogName(table.Name))
                    throw new SqlException(SqlStates.UndefinedTable, "table does not exist: " + found.Name);
                if (tables) tableCheck(found.Key, TablePrivileges.Select);
                if (view is not null) Expand(view, found.Local, visible, tables && view.SecurityInvoker, depth); break;
            case LateralRelation lateral: Relation(lateral.Source, visible, tables, depth); break;
            case AliasedRelation alias: Relation(alias.Source, visible, tables, depth); break;
            case GroupedRelation group: Relation(group.Source, visible, tables, depth); break;
            case FunctionRelation function: Expr(function.Function, visible, tables, depth); break;
            case ValuesRelation values: foreach (var row in values.Values) foreach (var expression in row) Expr(expression, visible, tables, depth); break;
            case DerivedRelation derived: Query(derived.Query, visible, tables, depth + 1); break;
            case JoinedRelation join: Relation(join.Left, visible, tables, depth); Relation(join.Right, visible, tables, depth); Expr(join.On, visible, tables, depth); break;
            case SetRelation set: Query(set.Left, visible, tables, depth + 1); Query(set.Right, visible, tables, depth + 1); break;
        }
    }
}
