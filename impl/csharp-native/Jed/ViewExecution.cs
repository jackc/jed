namespace Jed;

internal static partial class StatementExecution
{
    internal static SelectSyntax ViewQuery(CatalogView view)
    {
        try { return new Parser(view.Definition).Parse(); }
        catch (SqlException error) when (error.SqlState is SqlStates.SyntaxError or SqlStates.FeatureNotSupported)
        { throw new SqlException("XX003", $"cannot parse stored view: {view.Name}"); }
    }
    private static (byte Check, bool Barrier, bool Invoker) ViewOptions((string Name, string Value)[] options, byte check = 0, bool barrier = false, bool invoker = false)
    {
        var seen = new HashSet<string>(StringComparer.Ordinal);
        foreach (var (name, value) in options)
        {
            if (!seen.Add(name) || name == "check_option" && check != 0) throw new SqlException(SqlStates.InvalidParameterValue, "view option specified more than once");
            if (name == "check_option") { check = value switch { "local" => 1, "cascaded" => 2, _ => throw new SqlException(SqlStates.InvalidParameterValue, "invalid check option") }; continue; }
            var enabled = value switch { "true" or "on" or "1" or "yes" => true, "false" or "off" or "0" or "no" => false, _ => throw new SqlException(SqlStates.InvalidParameterValue, "invalid boolean view option") };
            if (name == "security_barrier") barrier = enabled;
            else if (name == "security_invoker") invoker = enabled;
            else throw new SqlException(SqlStates.InvalidParameterValue, "unknown view option");
        }
        return (check, barrier, invoker);
    }
    private static bool UpdatableView(SelectSyntax query) => query.Relation is TableRelation && !query.Distinct && !query.SetQuery &&
        query.Limit is null && query.Offset == 0 && (query.GroupBy?.Length ?? 0) == 0 && query.Having is null &&
        query.Items.Any(item => item.Expression.Op is "name" or "star") && !query.Items.Any(item => AggregateExpression(item.Expression));
    private static bool AggregateExpression(Expression expression) => expression.Op == "call" && expression.Text is "count" or "sum" or "avg" or "min" or "max" or "array_agg" or "string_agg" || expression.Args.Any(AggregateExpression);
    private static StatementResult CreateView(Database database, PendingDatabase pending, CreateViewSyntax create, Meter meter, CancellationToken token)
    {
        if (create.Name.StartsWith("jed_", StringComparison.Ordinal)) throw new SqlException(SqlStates.ReservedName, "reserved view name");
        if (create.IfNotExists && RelationExists(pending.Catalog, create.Name)) return new([], []);
        _ = ViewDependencies(create.Query, pending.Catalog);
        var query = RelationExecution.Query(create.Query, database, pending.Before, pending, pending.Catalog, meter, token);
        var options = ViewOptions(create.Options, create.CheckOption);
        if (options.Check != 0 && !UpdatableView(create.Query)) throw Errors.Unsupported("CHECK OPTION requires an updatable view");
        if ((create.Columns?.Length ?? 0) > query.Columns.Length) throw Errors.Syntax("too many view column names");
        var names = query.Columns.Select((column, i) => create.Columns is { } aliases && i < aliases.Length ? aliases[i] : column.Name).ToArray();
        if (names.Distinct(StringComparer.Ordinal).Count() != names.Length) throw new SqlException(SqlStates.DuplicateColumn, "duplicate view column");
        if (query.Columns.Any(column => column.Type.Identity == "record" || column.Type.Element?.Identity == "record")) throw new SqlException(SqlStates.InvalidTableDefinition, "view column has anonymous record type");
        var definition = FreezeViewStars(create.Query, create.Definition, database, pending, meter, token);
        var parsed = new Parser(definition).Parse();
        var dependencies = ViewDependencies(parsed, pending.Catalog);
        var old = (pending.Catalog.Views ?? []).FirstOrDefault(view => view.Name == create.Name);
        if (RelationExists(pending.Catalog, create.Name))
        {
            if (!create.Replace) throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
            if (old is null) throw new SqlException(SqlStates.WrongObjectType, "relation is not a view");
        }
        if (old is not null)
        {
            if (names.Length < old.Columns.Length) throw new SqlException(SqlStates.InvalidTableDefinition, "cannot drop view columns");
            PreparedRelation? previous = null;
            try { previous = RelationExecution.Query(ViewQuery(old), database, pending.Before, pending, pending.Catalog, meter, token); }
            catch (SqlException) { }
            for (var i = 0; i < old.Columns.Length; i++)
                if (names[i] != old.Columns[i].Name || previous is not null && QueryColumnType(create.Query, query.Columns[i], i, pending.Catalog) != QueryColumnType(ViewQuery(old), previous.Columns[i], i, pending.Catalog))
                    throw new SqlException(SqlStates.InvalidTableDefinition, "cannot change view column name or type");
        }
        bool Reaches(string name, HashSet<string> visited)
        {
            if (name == create.Name) return true;
            return visited.Add(name) && (pending.Catalog.Views ?? []).FirstOrDefault(view => view.Name == name) is { } view && view.Dependencies.Where(dependency => dependency.Kind == 1).Any(dependency => Reaches(dependency.Name, visited));
        }
        if (dependencies.Where(dependency => dependency.Kind == 1).Any(dependency => Reaches(dependency.Name, []))) throw new SqlException(SqlStates.InvalidObjectDefinition, "recursive view definition");
        var view = new CatalogView(create.Name, names.Select((name, i) => new CatalogViewColumn(name, old is not null && i < old.Columns.Length ? old.Columns[i].DefaultExpression : null)).ToArray(), definition, dependencies, options.Check, options.Barrier, options.Invoker);
        var candidate = pending.Catalog with { Views = (pending.Catalog.Views ?? []).Where(item => item.Name != view.Name).Append(view).ToArray() };
        foreach (var dependent in (pending.Catalog.Views ?? []).Where(item => item.Name != view.Name && item.Dependencies.Any(dependency => dependency.Kind == 1 && dependency.Name == view.Name)))
            try { _ = RelationExecution.Query(ViewQuery(dependent), database, pending.Before, pending, candidate, meter, token); }
            catch (SqlException) { throw Errors.Unsupported("replacement invalidates a dependent view"); }
        pending.Catalog = candidate; pending.Changed = true; return new([], []);
    }
    private static string FreezeViewStars(SelectSyntax query, string definition, Database database, PendingDatabase pending, Meter meter, CancellationToken token)
    {
        if (!query.Items.Any(item => item.Expression.Op == "star") || query.ValuesQuery || query.SetQuery) return definition;
        var input = query.Relation is null ? [] : RelationExecution.Prepare(query.Relation, database, pending.Before, pending, pending.Catalog, meter, token).Columns;
        var lexer = new Lexer(definition); var tokens = new List<Token>(); Token current;
        while ((current = lexer.Next()).Kind != TokenKind.End) tokens.Add(current);
        var start = tokens.FindIndex(item => item.Text == "select"); if (start < 0) return definition;
        var depth = 0; var itemStart = start + 1; if (itemStart < tokens.Count && tokens[itemStart].Text is "distinct" or "all") itemStart++;
        var spans = new List<(int Start, int End)>(); var end = itemStart;
        for (; end < tokens.Count; end++)
        {
            var text = tokens[end].Text;
            if (depth == 0 && text is "from" or "where" or "order" or "group" or "limit" or "offset") break;
            if (text == "(") depth++; else if (text == ")") depth--;
            if (depth == 0 && text == ",") { spans.Add((itemStart, end)); itemStart = end + 1; }
        }
        spans.Add((itemStart, end));
        if (spans.Count != query.Items.Length) throw Errors.Unsupported("view star expansion shape");
        for (var i = spans.Count - 1; i >= 0; i--)
        {
            var expression = query.Items[i].Expression; if (expression.Op != "star") continue;
            var columns = input.Where(column => !column.Hidden && (expression.Text.Length == 0 || column.Qualifier == expression.Text)).ToArray();
            var replacements = new List<Token>();
            foreach (var column in columns)
            {
                if (replacements.Count != 0) replacements.Add(new(TokenKind.Symbol, ","));
                if (column.Qualifier is { } qualifier) { replacements.Add(new(TokenKind.QuotedName, qualifier)); replacements.Add(new(TokenKind.Symbol, ".")); }
                replacements.Add(new(TokenKind.QuotedName, column.Name));
            }
            tokens.RemoveRange(spans[i].Start, spans[i].End - spans[i].Start); tokens.InsertRange(spans[i].Start, replacements);
        }
        return string.Join(' ', tokens.Select(Parser.RenderToken));
    }
    private static HashSet<string> DependentViews(Catalog catalog, IEnumerable<string> names, byte kind, string? column = null)
    {
        var roots = names.ToHashSet(StringComparer.Ordinal); var dropped = new HashSet<string>(StringComparer.Ordinal); bool changed;
        do
        {
            changed = false;
            foreach (var view in catalog.Views ?? [])
                if (!dropped.Contains(view.Name) && view.Dependencies.Any(dependency => dependency.Kind == kind && roots.Contains(dependency.Name) && (column is null || dependency.Columns.Contains(column)) || dependency.Kind == 1 && dropped.Contains(dependency.Name)))
                    changed |= dropped.Add(view.Name);
        } while (changed);
        return dropped;
    }
    private static void DropDependentViews(PendingDatabase pending, string name, byte kind, bool cascade, string? column = null)
    {
        var dependents = DependentViews(pending.Catalog, [name], kind, column);
        if (dependents.Count == 0) return;
        if (!cascade) throw new SqlException(SqlStates.DependentObjectsStillExist, "dependent view exists");
        pending.Catalog = pending.Catalog with { Views = (pending.Catalog.Views ?? []).Where(view => !dependents.Contains(view.Name)).ToArray() }; pending.Changed = true;
    }
    private static StatementResult DropView(PendingDatabase pending, DropViewSyntax drop)
    {
        var names = new HashSet<string>(StringComparer.Ordinal);
        foreach (var name in drop.Names)
        {
            if ((pending.Catalog.Views ?? []).Any(view => view.Name == name)) names.Add(name);
            else if (RelationExists(pending.Catalog, name)) throw new SqlException(SqlStates.WrongObjectType, "relation is not a view");
            else if (!drop.IfExists) throw new SqlException(SqlStates.UndefinedTable, "view does not exist");
        }
        var dependents = DependentViews(pending.Catalog, names, 1); dependents.ExceptWith(names);
        if (dependents.Count != 0 && !drop.Cascade) throw new SqlException(SqlStates.DependentObjectsStillExist, "dependent view exists");
        names.UnionWith(dependents);
        if (names.Count != 0) { pending.Catalog = pending.Catalog with { Views = (pending.Catalog.Views ?? []).Where(view => !names.Contains(view.Name)).ToArray() }; pending.Changed = true; }
        return new([], []);
    }
}
