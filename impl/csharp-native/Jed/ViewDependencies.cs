namespace Jed;

internal static partial class StatementExecution
{
    private static void RevalidateViews(Database database, PendingDatabase pending, string name, byte kind, Meter meter, CancellationToken token)
    {
        var dependents = (pending.Catalog.Views ?? []).Where(view => view.Dependencies.Any(dependency => dependency.Kind == kind && dependency.Name == name)).OrderBy(view => view.Name, Utf8NameComparer.Instance).ToArray();
        var definitions = dependents.Select(ViewQuery).ToArray(); // Unreadable stored definitions take precedence.
        for (var at = 0; at < dependents.Length; at++)
        {
            var view = dependents[at]; var body = definitions[at];
            try
            {
                _ = RelationExecution.Query(body, database, pending.Before, pending, pending.Catalog, meter, token);
                var actual = ViewDependencies(body, pending.Catalog);
                if (actual.Length != view.Dependencies.Length || actual.Where((dependency, index) =>
                    dependency.Kind != view.Dependencies[index].Kind || dependency.Name != view.Dependencies[index].Name ||
                    !dependency.Columns.SequenceEqual(view.Dependencies[index].Columns)).Any())
                    throw Errors.Unsupported("alteration changes a dependent view binding");
            }
            catch (SqlException error) when (error.SqlState != SqlStates.UnparsableStoredDefinition)
            { throw Errors.Unsupported("alteration invalidates a dependent view"); }
        }
    }
    internal static CatalogViewDependency[] ViewDependencies(SelectSyntax query, Catalog catalog, Action<string, string>? viewColumn = null)
    {
        var dependencies = new Dictionary<(byte Kind, string Name), HashSet<string>>();
        Catalog Home(string name) => catalog.Scope?.Resolve(name).Catalog ?? catalog;
        CatalogTable? SourceTable(string name) => Home(name).Tables.FirstOrDefault(table => table.Name == DatabaseDomains.Name(name).Local);
        void Add(byte kind, string name, string? column = null)
        {
            if (!dependencies.TryGetValue((kind, name), out var columns)) dependencies.Add((kind, name), columns = new(StringComparer.Ordinal));
            if (column is not null) columns.Add(column);
        }
        void Type(SqlType type)
        {
            if (type.Kind == SqlKind.Composite && type.Identity != "record") Add(3, type.Identity!);
            if (type.Element is { } element) Type(element);
        }
        void Expr(Expression expression, List<(string Label, string Table, string[]? Aliases)> scope, HashSet<string> ctes)
        {
            if (expression.Op == "parameter") throw new SqlException(SqlStates.UndefinedParameter, "view cannot contain parameters");
            if (expression.Op == "name")
            {
                var dot = expression.Text.LastIndexOf('.'); var name = expression.Text[(dot + 1)..]; var label = dot < 0 ? null : expression.Text[..dot];
                foreach (var source in scope.Where(source => label is null || source.Label == label))
                {
                    if (SourceTable(source.Table) is { } table)
                    {
                        for (var ordinal = 0; ordinal < table.Columns.Length; ordinal++)
                            if ((source.Aliases is { } aliases && ordinal < aliases.Length ? aliases[ordinal] : table.Columns[ordinal].Name) == name)
                                Add(0, table.Name, table.Columns[ordinal].Name);
                    }
                    else if ((Home(source.Table).Views ?? []).FirstOrDefault(view => view.Name == DatabaseDomains.Name(source.Table).Local) is { } view)
                        for (var ordinal = 0; ordinal < view.Columns.Length; ordinal++)
                            if ((source.Aliases is { } aliases && ordinal < aliases.Length ? aliases[ordinal] : view.Columns[ordinal].Name) == name)
                                viewColumn?.Invoke(view.Name, view.Columns[ordinal].Name);
                }
            }
            if (expression.Op == "star")
                foreach (var source in scope.Where(source => expression.Text.Length == 0 || source.Label == expression.Text))
                    if (SourceTable(source.Table) is { } table)
                        foreach (var column in table.Columns) Add(0, table.Name, column.Name);
            if (expression.Op == "cast") Type(SqlTypes.ParseSpec(expression.Text, catalog.NamedType).Type);
            if (expression.Op == "call" && expression.Text is "nextval" or "currval" or "setval" && expression.Args.Length > 0 && expression.Args[0].Op == "text")
            {
                var name = SequenceContext.Name(expression.Args[0].Text);
                if (!(Home(name).Sequences ?? []).Any(sequence => sequence.Name == DatabaseDomains.Name(name).Local)) throw new SqlException(SqlStates.UndefinedTable, "view sequence does not exist");
                Add(2, DatabaseDomains.Name(name).Local);
            }
            if (expression.Query is { } body) Query(body, scope, ctes);
            foreach (var argument in expression.Args)
                if (!(expression.Op == "call" && expression.Text == "count" && argument.Op == "star")) Expr(argument, scope, ctes);
            if (expression.Predicate is { } predicate) Expr(predicate, scope, ctes);
            foreach (var order in expression.AggregateOrder ?? []) Expr(order.Expression, scope, ctes);
            if (expression.Window is { } window) Window(window, scope, ctes);
        }
        void Window(WindowSyntax window, List<(string Label, string Table, string[]? Aliases)> scope, HashSet<string> ctes)
        {
            foreach (var expression in window.Partition) Expr(expression, scope, ctes);
            foreach (var order in window.Order) Expr(order.Expression, scope, ctes);
            if (window.Frame?.Start.Offset is { } start) Expr(start, scope, ctes);
            if (window.Frame?.End.Offset is { } end) Expr(end, scope, ctes);
        }
        void Relation(RelationSyntax? relation, List<(string Label, string Table, string[]? Aliases)> scope, HashSet<string> ctes)
        {
            switch (relation)
            {
                case LateralRelation lateral: Relation(lateral.Source, scope, ctes); break;
                case AliasedRelation aliased: Relation(aliased.Source, scope, ctes); break;
                case TableRelation table when !ctes.Contains(table.Name):
                {
                    var local = DatabaseDomains.Name(table.Name).Local;
                    scope.Add((table.Alias ?? local, table.Name, table.ColumnAliases));
                    var home = Home(table.Name);
                    if (catalog.Scope?.Resolve(table.Name).Name is { } domain && domain is not ("main" or "temp")) throw Errors.Unsupported("view body references an attached database");
                    if (home.Tables.Any(item => item.Name == local)) Add(0, local);
                    else if ((home.Views ?? []).Any(item => item.Name == local)) Add(1, local);
                    break;
                }
                case DerivedRelation derived: Query(derived.Query, [], ctes); break;
                case SetRelation set: Query(set.Left, scope, ctes); Query(set.Right, scope, ctes); break;
                case FunctionRelation function: Expr(function.Function, scope, ctes); break;
                case ValuesRelation values: foreach (var row in values.Values) foreach (var value in row) Expr(value, scope, ctes); break;
                case GroupedRelation group: Relation(group.Source, scope, ctes); break;
                case JoinedRelation join:
                    Relation(join.Left, scope, ctes); Relation(join.Right, scope, ctes);
                    if (join.On is { } predicate) Expr(predicate, scope, ctes);
                    foreach (var name in join.Using ?? []) Expr(new("name", name, []), scope, ctes);
                    if (join.Natural)
                    {
                        var columns = scope.SelectMany(source => SourceTable(source.Table)?.Columns.Select(column => column.Name) ?? []);
                        foreach (var name in columns.GroupBy(name => name).Where(group => group.Count() > 1).Select(group => group.Key)) Expr(new("name", name, []), scope, ctes);
                    }
                    break;
            }
        }
        void Query(SelectSyntax body, List<(string Label, string Table, string[]? Aliases)> outer, HashSet<string> inherited)
        {
            var ctes = new HashSet<string>(inherited, StringComparer.Ordinal);
            foreach (var cte in body.With ?? [])
            {
                if (body.Recursive) ctes.Add(cte.Name);
                Query(cte.Query, [], ctes); ctes.Add(cte.Name);
            }
            var scope = new List<(string Label, string Table, string[]? Aliases)>(); Relation(body.Relation, scope, ctes);
            if (body.Relation is null && body.Table is { } name) Relation(new TableRelation(name, body.TableAlias), scope, ctes);
            scope.AddRange(outer.Where(source => !scope.Any(inner => inner.Label == source.Label)));
            foreach (var item in body.Items) Expr(item.Expression, scope, ctes);
            if (body.Filter is { } filter) Expr(filter, scope, ctes);
            if (body.Having is { } having) Expr(having, scope, ctes);
            if (body.LimitExpression is { } limit) Expr(limit, scope, ctes);
            if (body.OffsetExpression is { } offset) Expr(offset, scope, ctes);
            foreach (var group in body.GroupBy ?? []) Expr(group, scope, ctes);
            foreach (var key in body.DistinctOn ?? []) Expr(key, scope, ctes);
            foreach (var order in body.Order ?? []) Expr(order.Expression, scope, ctes);
            foreach (var window in body.Windows?.Values.AsEnumerable() ?? []) Window(window, scope, ctes);
        }
        Query(query, [], new(StringComparer.Ordinal));
        return dependencies.OrderBy(pair => pair.Key.Name, Utf8NameComparer.Instance).ThenBy(pair => pair.Key.Kind)
            .Select(pair => new CatalogViewDependency(pair.Key.Kind, pair.Key.Name, pair.Value.Order(Utf8NameComparer.Instance).ToArray())).ToArray();
    }
    private static StatementResult AlterView(Database database, PendingDatabase pending, AlterViewSyntax alter, Meter meter, CancellationToken token)
    {
        if (alter.Actions is { } actions)
        {
            foreach (var action in actions) AlterView(database, pending, action with { Name = alter.Name }, meter, token);
            return new([], []);
        }
        var view = (pending.Catalog.Views ?? []).FirstOrDefault(view => view.Name == alter.Name);
        if (view is null && alter.IfExists && !RelationExists(pending.Catalog, alter.Name)) return new([], []);
        if (view is null) throw new SqlException(RelationExists(pending.Catalog, alter.Name) ? SqlStates.WrongObjectType : SqlStates.UndefinedTable, "view does not exist");
        if (alter.Operation == "rename")
        {
            if (RelationExistsAnywhere(pending.Catalog, alter.Value!)) throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
            if (alter.Value!.StartsWith("jed_", StringComparison.Ordinal)) throw new SqlException(SqlStates.ReservedName, "reserved view name");
            RenameViews(pending, view.Name, alter.Value!, 1);
            pending.Catalog = pending.Catalog with { Views = (pending.Catalog.Views ?? []).Select(item => item.Name == view.Name ? item with { Name = alter.Value! } : item).ToArray() };
            pending.Changed = true; RevalidateViews(database, pending, alter.Value!, 1, meter, token); return new([], []);
        }
        if (alter.Operation is "rename_column" or "default")
        {
            var index = Array.FindIndex(view.Columns, column => column.Name == alter.Column);
            if (index < 0) throw new SqlException(SqlStates.UndefinedColumn, "view column does not exist");
            var columns = view.Columns.ToArray();
            if (alter.Operation == "rename_column")
            {
                if (columns.Any(column => column.Name == alter.Value)) throw new SqlException(SqlStates.DuplicateColumn, "view column already exists");
                RenameViews(pending, view.Name, view.Name, 1, columns.Select(column => column.Name).ToArray(), index, alter.Value!);
                columns[index] = columns[index] with { Name = alter.Value! };
            }
            else
            {
                var query = RelationExecution.Query(ViewQuery(view), database, pending.Before, pending, pending.Catalog, meter, token);
                var type = QueryColumnType(ViewQuery(view), query.Columns[index], index, pending.Catalog);
                var validated = WithDefault(new CatalogColumn(columns[index].Name, type), alter.Default, alter.DefaultText, meter, pending.Catalog);
                var text = validated.DefaultExpression;
                if (validated.Default is { IsNull: false } constant)
                {
                    if (constant.Type == SqlType.Decimal && alter.Default is { } literal && literal.Op is "integer" or "decimal")
                        constant = Assign(literal, type with { Precision = null, Scale = null }, [], [], meter, pending.Catalog);
                    var rendered = constant.ToString();
                    text = constant.Type.IsInteger() || constant.Type == SqlType.Decimal || constant.Type == SqlType.Boolean ? rendered : "'" + rendered.Replace("'", "''", StringComparison.Ordinal) + "'";
                }
                columns[index] = columns[index] with { DefaultExpression = text };
            }
            view = view with { Columns = columns };
        }
        else
        {
            var options = ViewOptions(alter.Options ?? [], view.CheckOption, view.SecurityBarrier, view.SecurityInvoker);
            if (options.Check != 0 && !UpdatableView(ViewQuery(view))) throw Errors.Unsupported("CHECK OPTION requires an updatable view");
            view = view with { CheckOption = options.Check, SecurityBarrier = options.Barrier, SecurityInvoker = options.Invoker };
        }
        pending.Catalog = pending.Catalog with { Views = (pending.Catalog.Views ?? []).Select(item => item.Name == view.Name ? view : item).ToArray() }; pending.Changed = true; RevalidateViews(database, pending, view.Name, 1, meter, token); return new([], []);
    }
    private static string RewriteViewRelation(string text, string before, string after, string[]? exposedColumns = null, int renamedColumn = -1, int droppedColumn = -1)
    {
        List<Token> Rewrite(List<Token> tokens, HashSet<string> inherited)
        {
            var ctes = new HashSet<string>(inherited, StringComparer.Ordinal); var prefixEnd = 0;
            if (tokens.Count > 0 && tokens[0].Kind == TokenKind.Word && tokens[0].Text == "with")
            {
                var cursor = 1; var recursive = tokens[cursor].Text == "recursive"; if (recursive) cursor++;
                while (true)
                {
                    var name = tokens[cursor++].Text;
                    if (tokens[cursor].Text == "(") cursor = ViewTokens.Close(tokens, cursor) + 1;
                    while (tokens[cursor].Text != "(") cursor++;
                    var close = ViewTokens.Close(tokens, cursor);
                    if (recursive) ctes.Add(name);
                    var inner = Rewrite(tokens.GetRange(cursor + 1, close - cursor - 1), ctes);
                    tokens.RemoveRange(cursor + 1, close - cursor - 1); tokens.InsertRange(cursor + 1, inner);
                    ctes.Add(name); cursor += inner.Count + 2;
                    if (cursor >= tokens.Count || tokens[cursor].Text != ",") break;
                    cursor++;
                }
                prefixEnd = cursor;
            }
            ViewTokens.Nested(tokens, prefixEnd, nested => Rewrite(nested, ctes));
        var clauses = new HashSet<string>(["where", "group", "having", "order", "limit", "offset", "union", "intersect", "except", "join", "inner", "left", "right", "full", "cross", "natural", "on", "using", "window"], StringComparer.Ordinal);
        var from = false; var expect = false;
        for (var i = prefixEnd; i < tokens.Count; i++)
        {
            var current = tokens[i];
            if (current.Kind == TokenKind.Symbol && current.Text == "(") { i = ViewTokens.Close(tokens, i); expect = false; continue; }
            if (current.Kind == TokenKind.Word && current.Text is "from" or "join") { from = true; expect = true; continue; }
            if (current.Kind == TokenKind.Word && clauses.Contains(current.Text)) { if (current.Text is "where" or "group" or "having" or "order" or "limit" or "offset" or "union" or "intersect" or "except" or "window") from = false; expect = false; continue; }
            if (current.Text == "," && from) { expect = true; continue; }
            if (!expect) continue;
            expect = false;
            var qualified = current.Text == "main" && i + 2 < tokens.Count && tokens[i + 1].Text == ".";
            if (qualified) { i += 2; current = tokens[i]; }
            if (!qualified && ctes.Contains(current.Text)) continue;
            if (current.Text != before || current.Kind is not (TokenKind.Word or TokenKind.QuotedName)) continue;
            var end = i + 1; var hasAlias = false;
            if (end < tokens.Count && tokens[end].Text == "as") { hasAlias = true; end += 2; }
            else if (end < tokens.Count && (tokens[end].Kind == TokenKind.QuotedName || tokens[end].Kind == TokenKind.Word && !clauses.Contains(tokens[end].Text))) { hasAlias = true; end++; }
            if (after != before) tokens[i] = new(TokenKind.QuotedName, after);
            if (droppedColumn >= 0 && !hasAlias) continue;
            if (!hasAlias)
            {
                tokens.InsertRange(end, [new(TokenKind.Word, "as"), new(TokenKind.QuotedName, before)]); end += 2;
            }
            if (droppedColumn >= 0 && end < tokens.Count && tokens[end].Text == "(")
            {
                var listEnd = ViewTokens.Close(tokens, end);
                var names = tokens.GetRange(end + 1, listEnd - end - 1).Where(item => item.Kind is TokenKind.Word or TokenKind.QuotedName).ToList();
                if (droppedColumn < names.Count)
                {
                    names.RemoveAt(droppedColumn); var replacement = new List<Token>();
                    if (names.Count != 0)
                    {
                        replacement.Add(new(TokenKind.Symbol, "("));
                        foreach (var name in names) { if (replacement.Count > 1) replacement.Add(new(TokenKind.Symbol, ",")); replacement.Add(name); }
                        replacement.Add(new(TokenKind.Symbol, ")"));
                    }
                    tokens.RemoveRange(end, listEnd - end + 1); tokens.InsertRange(end, replacement); end += replacement.Count;
                }
            }
            if (renamedColumn >= 0 && exposedColumns is not null)
            {
                var names = new List<Token>(); var hasList = end < tokens.Count && tokens[end].Text == "("; var listEnd = end;
                if (hasList)
                {
                    listEnd++;
                    while (listEnd < tokens.Count && tokens[listEnd].Text != ")") { if (tokens[listEnd].Kind is TokenKind.Word or TokenKind.QuotedName) names.Add(tokens[listEnd]); listEnd++; }
                }
                if (names.Count <= renamedColumn)
                {
                    while (names.Count <= renamedColumn) names.Add(new(TokenKind.QuotedName, exposedColumns[names.Count]));
                    var list = new List<Token> { new(TokenKind.Symbol, "(") };
                    foreach (var name in names) { if (list.Count > 1) list.Add(new(TokenKind.Symbol, ",")); list.Add(name); }
                    list.Add(new(TokenKind.Symbol, ")"));
                    if (hasList) tokens.RemoveRange(end, listEnd - end + 1);
                    tokens.InsertRange(end, list); end += list.Count;
                }
            }
            i = end - 1;
        }
        return tokens;
        }
        return ViewTokens.Render(Rewrite(ViewTokens.Read(text), new(StringComparer.Ordinal)));
    }
    private static void DropViewColumnAliases(PendingDatabase pending, string table, int ordinal)
    {
        EditScopedViews(pending, views => views.Select(view =>
            view.Dependencies.Any(dependency => dependency.Kind == 0 && dependency.Name == table)
                ? view with { Definition = RewriteViewRelation(view.Definition, table, table, droppedColumn: ordinal) } : view).ToArray());
    }
    private static void RenameViews(PendingDatabase pending, string before, string after, byte kind, string[]? columns = null, int renamedColumn = -1, string? renamedTo = null)
    {
        foreach (var view in pending.Catalog.Views ?? [])
            if (view.Dependencies.Any(dependency => dependency.Kind == kind && dependency.Name == before)) _ = ViewQuery(view);
        EditScopedViews(pending, views => views.Select(view => view.Dependencies.Any(dependency => dependency.Kind == kind && dependency.Name == before) ? view with {
            Definition = RewriteViewRelation(view.Definition, before, after, columns, renamedColumn),
            Dependencies = view.Dependencies.Select(dependency => dependency.Kind == kind && dependency.Name == before ? dependency with { Name = after,
                Columns = dependency.Columns.Select(column => columns is not null && renamedColumn >= 0 && column == columns[renamedColumn] ? renamedTo! : column).Order(Utf8NameComparer.Instance).ToArray() } : dependency).OrderBy(dependency => dependency.Name, Utf8NameComparer.Instance).ThenBy(dependency => dependency.Kind).ToArray()
        } : view).ToArray());
    }
}
