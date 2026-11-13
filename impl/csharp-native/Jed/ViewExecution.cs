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
            if (!seen.Add(name)) throw new SqlException(SqlStates.InvalidParameterValue, "view option specified more than once");
            if (value == "reset")
            {
                if (name == "check_option") check = 0; else if (name == "security_barrier") barrier = false; else if (name == "security_invoker") invoker = false;
                continue;
            }
            if (name == "check_option") { check = TextEncoding.FoldName(value) switch { "local" => 1, "cascaded" => 2, _ => throw new SqlException(SqlStates.InvalidParameterValue, "invalid check option") }; continue; }
            var enabled = TextEncoding.FoldName(value) switch { "true" or "on" or "1" or "yes" => true, "false" or "off" or "0" or "no" => false, _ => throw new SqlException(SqlStates.InvalidParameterValue, "invalid boolean view option") };
            if (name == "security_barrier") barrier = enabled;
            else if (name == "security_invoker") invoker = enabled;
            else throw new SqlException(SqlStates.InvalidParameterValue, "unknown view option");
        }
        return (check, barrier, invoker);
    }
    private static bool UpdatableView(SelectSyntax query) => query.Relation is TableRelation && !query.Distinct && !query.SetQuery &&
        query.Limit is null && query.Offset == 0 && (query.With?.Length ?? 0) == 0 && (query.GroupBy?.Length ?? 0) == 0 && query.Having is null &&
        query.Items.Any(item => item.Expression.Op is "name" or "star") && !query.Items.Any(item => AggregateExpression(item.Expression) || WindowResolve.Contains(item.Expression));
    private static bool AggregateExpression(Expression expression) => AggregateResolve.IsAggregate(expression) || expression.Args.Any(AggregateExpression);
    private static StatementResult CreateView(Database database, PendingDatabase pending, CreateViewSyntax create, Meter meter, CancellationToken token)
    {
        if (create.Name.StartsWith("jed_", StringComparison.Ordinal)) throw new SqlException(SqlStates.ReservedName, "reserved view name");
        if (create.IfNotExists && RelationExists(pending.Catalog, create.Name)) return new([], []);
        _ = ViewDependencies(create.Query, pending.Catalog);
        var query = RelationExecution.Query(create.Query, database, pending.Before, pending, pending.Catalog, meter, token);
        if (create.CheckOption != 0 && create.Options.Any(option => option.Name == "check_option")) throw new SqlException(SqlStates.InvalidParameterValue, "view check option specified more than once");
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
                if (names[i] != old.Columns[i].Name || previous is not null && (previous.Columns[i].Collation?.Name ?? "C") != (query.Columns[i].Collation?.Name ?? "C") || previous is not null && QueryColumnType(create.Query, query.Columns[i], i, pending.Catalog) != QueryColumnType(ViewQuery(old), previous.Columns[i], i, pending.Catalog))
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
        var tokens = ViewTokens.Read(definition);
        List<Token> Freeze(List<Token> inputTokens, QueryScope? inherited)
        {
            var body = new Parser(ViewTokens.Render(inputTokens)).Parse();
            var scope = new QueryScope(inherited);
            var prefixEnd = 0;
            if (body.With is { Length: > 0 } definitions)
            {
                var cursor = 1; if (inputTokens[cursor].Text == "recursive") cursor++;
                foreach (var cte in definitions)
                {
                    cursor++; // CTE name
                    if (inputTokens[cursor].Text == "(") cursor = ViewTokens.Close(inputTokens, cursor) + 1;
                    while (inputTokens[cursor].Text != "(") cursor++;
                    var close = ViewTokens.Close(inputTokens, cursor);
                    var cteBody = inputTokens.GetRange(cursor + 1, close - cursor - 1);
                    // Recursive shapes are already validated by the enclosing query planner.
                    // The seed supplies the recursive reference's column shape while freezing.
                    if (body.Recursive && cte.Query.Relation is SetRelation recursive)
                    {
                        var seed = RelationExecution.Query(recursive.Left, database, pending.Before, pending, pending.Catalog, meter, token, scope: scope);
                        scope.Add(cte.Name, seed with { Columns = RelationExecution.Alias(seed.Columns, cte.Name, cte.Columns) });
                    }
                    var frozen = Freeze(cteBody, scope);
                    inputTokens.RemoveRange(cursor + 1, close - cursor - 1); inputTokens.InsertRange(cursor + 1, frozen);
                    close = cursor + frozen.Count + 1;
                    if (!body.Recursive || cte.Query.Relation is not SetRelation)
                    {
                        var prepared = RelationExecution.Query(cte.Query, database, pending.Before, pending, pending.Catalog, meter, token, scope: scope);
                        scope.Add(cte.Name, prepared with { Columns = RelationExecution.Alias(prepared.Columns, cte.Name, cte.Columns) });
                    }
                    cursor = close + 1; if (cursor < inputTokens.Count && inputTokens[cursor].Text == ",") cursor++;
                }
                prefixEnd = cursor;
            }
            if (body.SetQuery)
            {
                for (var i = prefixEnd; i < inputTokens.Count; i++)
                {
                    if (inputTokens[i].Text == "(") { i = ViewTokens.Close(inputTokens, i); continue; }
                    if (inputTokens[i].Kind != TokenKind.Word || inputTokens[i].Text is not ("union" or "intersect" or "except")) continue;
                    var rightStart = i + 1;
                    if (rightStart < inputTokens.Count && inputTokens[rightStart].Text is "all" or "distinct") rightStart++;
                    return [.. inputTokens.Take(prefixEnd), .. Freeze(inputTokens.GetRange(prefixEnd, i - prefixEnd), scope),
                        .. inputTokens.GetRange(i, rightStart - i), .. Freeze(inputTokens.GetRange(rightStart, inputTokens.Count - rightStart), scope)];
                }
            }
            // Each nested SELECT has its own projection, including EXISTS and derived tables.
            ViewTokens.Nested(inputTokens, prefixEnd, nested => Freeze(nested, scope));
            if (body.ValuesQuery || body.SetQuery || !body.Items.Any(item => item.Expression.Op == "star")) return inputTokens;
            var input = body.Relation is null ? [] : RelationExecution.Prepare(body.Relation, database, pending.Before, pending, pending.Catalog, meter, token, scope).Columns;
            var start = prefixEnd;
            while (start < inputTokens.Count && inputTokens[start].Text != "select") start++;
            var itemStart = start + 1;
            if (itemStart < inputTokens.Count && inputTokens[itemStart].Text is "distinct" or "all") itemStart++;
            if (itemStart < inputTokens.Count && inputTokens[itemStart].Text == "on") itemStart = ViewTokens.Close(inputTokens, itemStart + 1) + 1;
            var spans = new List<(int Start, int End)>(); var end = itemStart;
            for (; end < inputTokens.Count; end++)
            {
                var text = inputTokens[end].Text;
                if (inputTokens[end].Kind == TokenKind.Word && text is "from" or "where" or "order" or "group" or "limit" or "offset" or "window" or "union" or "intersect" or "except") break;
                if (text == "(") { end = ViewTokens.Close(inputTokens, end); continue; }
                if (text == ",") { spans.Add((itemStart, end)); itemStart = end + 1; }
            }
            spans.Add((itemStart, end));
            if (spans.Count != body.Items.Length) throw Errors.Unsupported("view star expansion shape");
            for (var i = spans.Count - 1; i >= 0; i--)
            {
                var expression = body.Items[i].Expression; if (expression.Op != "star") continue;
                var columns = input.Where(column => !column.Hidden && (expression.Text.Length == 0 || column.Qualifier == expression.Text));
                var replacements = new List<Token>();
                foreach (var column in columns)
                {
                    if (replacements.Count != 0) replacements.Add(new(TokenKind.Symbol, ","));
                    if (column.Qualifier is { } qualifier) { replacements.Add(new(TokenKind.QuotedName, qualifier)); replacements.Add(new(TokenKind.Symbol, ".")); }
                    replacements.Add(new(TokenKind.QuotedName, column.Name));
                }
                inputTokens.RemoveRange(spans[i].Start, spans[i].End - spans[i].Start); inputTokens.InsertRange(spans[i].Start, replacements);
            }
            return inputTokens;
        }
        return ViewTokens.Render(Freeze(tokens, null));
    }
    private static IEnumerable<CatalogView> ScopedViews(Catalog catalog) => catalog.Scope is { } domains
        ? domains.Items.Where(domain => domain.Name is "main" or "temp").SelectMany(domain => domain.Catalog.Views ?? []) : catalog.Views ?? [];
    private static void EditScopedViews(PendingDatabase pending, Func<CatalogView[], CatalogView[]> edit)
    {
        var owners = new HashSet<PendingDatabase>(ReferenceEqualityComparer.Instance) { pending };
        if (pending.Catalog.Scope is { } domains)
            foreach (var domain in domains.Items.Where(domain => domain.Name is "main" or "temp")) if (domain.Pending is { } owner) owners.Add(owner);
        foreach (var owner in owners)
        {
            var before = owner.Catalog.Views ?? []; var after = edit(before);
            if (!before.SequenceEqual(after)) { owner.Catalog = owner.Catalog with { Views = after }; owner.Changed = true; }
        }
    }
    private static bool RelationExistsAnywhere(Catalog catalog, string name) => RelationExists(catalog, name) ||
        catalog.Scope is { } scope && scope.Items.Where(domain => domain.Name is "main" or "temp").Any(domain => RelationExists(domain.Catalog, name));
    private static HashSet<string> DependentViews(Catalog catalog, IEnumerable<string> names, byte kind, string? column = null)
    {
        var roots = names.ToHashSet(StringComparer.Ordinal); var dropped = new HashSet<string>(StringComparer.Ordinal); bool changed;
        do
        {
            changed = false;
            foreach (var view in ScopedViews(catalog))
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
        EditScopedViews(pending, views => views.Where(view => !dependents.Contains(view.Name)).ToArray());
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
        if (names.Count != 0) EditScopedViews(pending, views => views.Where(view => !names.Contains(view.Name)).ToArray());
        return new([], []);
    }
}
