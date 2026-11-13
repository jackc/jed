namespace Jed;

internal static partial class StatementExecution
{
    private static CatalogViewDependency[] ViewDependencies(SelectSyntax query, Catalog catalog)
    {
        var dependencies = new Dictionary<(byte Kind, string Name), HashSet<string>>();
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
        void Expr(Expression expression, List<(string Label, string Table)> scope, HashSet<string> ctes)
        {
            if (expression.Op == "parameter") throw new SqlException(SqlStates.UndefinedParameter, "view cannot contain parameters");
            if (expression.Op == "name")
            {
                var dot = expression.Text.LastIndexOf('.'); var name = expression.Text[(dot + 1)..]; var label = dot < 0 ? null : expression.Text[..dot];
                foreach (var source in scope.Where(source => label is null || source.Label == label))
                    if (catalog.Tables.FirstOrDefault(table => table.Name == source.Table) is { } table && table.Columns.Any(column => column.Name == name)) Add(0, table.Name, name);
            }
            if (expression.Op == "star")
                foreach (var source in scope.Where(source => expression.Text.Length == 0 || source.Label == expression.Text))
                    if (catalog.Tables.FirstOrDefault(table => table.Name == source.Table) is { } table)
                        foreach (var column in table.Columns) Add(0, table.Name, column.Name);
            if (expression.Op == "cast") Type(SqlTypes.ParseSpec(expression.Text, catalog.NamedType).Type);
            if (expression.Op == "call" && expression.Text is "nextval" or "currval" or "setval" && expression.Args.Length > 0 && expression.Args[0].Op == "text")
            {
                var name = SequenceContext.Name(expression.Args[0].Text);
                if (!(catalog.Sequences ?? []).Any(sequence => sequence.Name == name)) throw new SqlException(SqlStates.UndefinedTable, "view sequence does not exist");
                Add(2, name);
            }
            if (expression.Query is { } body) Query(body, scope, ctes);
            foreach (var argument in expression.Args) Expr(argument, scope, ctes);
            if (expression.Predicate is { } predicate) Expr(predicate, scope, ctes);
            foreach (var order in expression.AggregateOrder ?? []) Expr(order.Expression, scope, ctes);
            if (expression.Window is { } window) Window(window, scope, ctes);
        }
        void Window(WindowSyntax window, List<(string Label, string Table)> scope, HashSet<string> ctes)
        {
            foreach (var expression in window.Partition) Expr(expression, scope, ctes);
            foreach (var order in window.Order) Expr(order.Expression, scope, ctes);
            if (window.Frame?.Start.Offset is { } start) Expr(start, scope, ctes);
            if (window.Frame?.End.Offset is { } end) Expr(end, scope, ctes);
        }
        void Relation(RelationSyntax? relation, List<(string Label, string Table)> scope, HashSet<string> ctes)
        {
            switch (relation)
            {
                case LateralRelation lateral: Relation(lateral.Source, scope, ctes); break;
                case AliasedRelation aliased: Relation(aliased.Source, scope, ctes); break;
                case TableRelation table when !ctes.Contains(table.Name):
                    scope.Add((table.Alias ?? table.Name, table.Name));
                    if (catalog.Tables.Any(item => item.Name == table.Name)) Add(0, table.Name);
                    else if ((catalog.Views ?? []).Any(item => item.Name == table.Name)) Add(1, table.Name);
                    break;
                case DerivedRelation derived: Query(derived.Query, [], ctes); break;
                case SetRelation set: Query(set.Left, scope, ctes); Query(set.Right, scope, ctes); break;
                case FunctionRelation function: Expr(function.Function, scope, ctes); break;
                case ValuesRelation values: foreach (var row in values.Values) foreach (var value in row) Expr(value, scope, ctes); break;
                case JoinedRelation join:
                    Relation(join.Left, scope, ctes); Relation(join.Right, scope, ctes);
                    if (join.On is { } predicate) Expr(predicate, scope, ctes);
                    foreach (var name in join.Using ?? []) Expr(new("name", name, []), scope, ctes);
                    if (join.Natural)
                    {
                        var columns = scope.SelectMany(source => catalog.Tables.FirstOrDefault(table => table.Name == source.Table)?.Columns.Select(column => column.Name) ?? []);
                        foreach (var name in columns.GroupBy(name => name).Where(group => group.Count() > 1).Select(group => group.Key)) Expr(new("name", name, []), scope, ctes);
                    }
                    break;
            }
        }
        void Query(SelectSyntax body, List<(string Label, string Table)> outer, HashSet<string> inherited)
        {
            var ctes = new HashSet<string>(inherited, StringComparer.Ordinal);
            foreach (var cte in body.With ?? []) { ctes.Add(cte.Name); Query(cte.Query, [], ctes); }
            var scope = new List<(string Label, string Table)>(); Relation(body.Relation, scope, ctes);
            if (body.Relation is null && body.Table is { } name) Relation(new TableRelation(name, body.TableAlias), scope, ctes);
            scope.AddRange(outer.Where(source => !scope.Any(inner => inner.Label == source.Label)));
            foreach (var item in body.Items) Expr(item.Expression, scope, ctes);
            if (body.Filter is { } filter) Expr(filter, scope, ctes);
            if (body.Having is { } having) Expr(having, scope, ctes);
            foreach (var group in body.GroupBy ?? []) Expr(group, scope, ctes);
            foreach (var order in body.Order ?? []) Expr(order.Expression, scope, ctes);
            foreach (var window in body.Windows?.Values.AsEnumerable() ?? []) Window(window, scope, ctes);
        }
        Query(query, [], new(StringComparer.Ordinal));
        return dependencies.OrderBy(pair => pair.Key.Name, Utf8NameComparer.Instance).ThenBy(pair => pair.Key.Kind)
            .Select(pair => new CatalogViewDependency(pair.Key.Kind, pair.Key.Name, pair.Value.Order(Utf8NameComparer.Instance).ToArray())).ToArray();
    }
    private static StatementResult AlterView(Database database, PendingDatabase pending, AlterViewSyntax alter, Meter meter, CancellationToken token)
    {
        var view = (pending.Catalog.Views ?? []).FirstOrDefault(view => view.Name == alter.Name);
        if (view is null) throw new SqlException(RelationExists(pending.Catalog, alter.Name) ? SqlStates.WrongObjectType : SqlStates.UndefinedTable, "view does not exist");
        if (alter.Operation == "rename")
        {
            if (RelationExists(pending.Catalog, alter.Value!)) throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
            if (alter.Value!.StartsWith("jed_", StringComparison.Ordinal)) throw new SqlException(SqlStates.ReservedName, "reserved view name");
            // Stored definitions bind by name. Renames must rewrite dependent relation tokens too.
            var views = (pending.Catalog.Views ?? []).Select(item => item.Name == view.Name ? item with { Name = alter.Value! } : item.Dependencies.Any(dependency => dependency.Kind == 1 && dependency.Name == view.Name)
                ? item with { Definition = RewriteViewRelation(item.Definition, view.Name, alter.Value!), Dependencies = item.Dependencies.Select(dependency => dependency.Kind == 1 && dependency.Name == view.Name ? dependency with { Name = alter.Value! } : dependency).ToArray() } : item).ToArray();
            pending.Catalog = pending.Catalog with { Views = views }; pending.Changed = true; return new([], []);
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
                if (alter.Default is { } value)
                {
                    ValidateStoredExpression(value, isDefault: true);
                    var resolved = Resolve.Expr(Resolve.BindTypes(value, pending.Catalog, meter.Temporal), query.Columns[index].Type); Assignable(resolved.Type, query.Columns[index].Type);
                }
                columns[index] = columns[index] with { DefaultExpression = alter.DefaultText };
            }
            view = view with { Columns = columns };
        }
        else
        {
            var options = ViewOptions(alter.Options ?? [], view.CheckOption, view.SecurityBarrier, view.SecurityInvoker);
            if (options.Check != 0 && !UpdatableView(ViewQuery(view))) throw Errors.Unsupported("CHECK OPTION requires an updatable view");
            view = view with { CheckOption = options.Check, SecurityBarrier = options.Barrier, SecurityInvoker = options.Invoker };
        }
        pending.Catalog = pending.Catalog with { Views = (pending.Catalog.Views ?? []).Select(item => item.Name == view.Name ? view : item).ToArray() }; pending.Changed = true; return new([], []);
    }
    private static string RewriteViewRelation(string text, string before, string after, string[]? exposedColumns = null, int renamedColumn = -1)
    {
        var lexer = new Lexer(text); var tokens = new List<Token>(); Token token;
        while ((token = lexer.Next()).Kind != TokenKind.End) tokens.Add(token);
        var clauses = new HashSet<string>(["where", "group", "having", "order", "limit", "offset", "union", "intersect", "except", "join", "inner", "left", "right", "full", "cross", "natural", "on", "using", "window"], StringComparer.Ordinal);
        var from = false; var expect = false; var states = new Stack<(bool From, bool Expect)>();
        for (var i = 0; i < tokens.Count; i++)
        {
            var current = tokens[i];
            if (current.Text == "(") { states.Push((from, expect)); from = false; expect = false; continue; }
            if (current.Text == ")") { if (states.TryPop(out var state)) { from = state.From; expect = false; } continue; }
            if (current.Kind == TokenKind.Word && current.Text is "from" or "join") { from = true; expect = true; continue; }
            if (current.Kind == TokenKind.Word && clauses.Contains(current.Text)) { if (current.Text is "where" or "group" or "having" or "order" or "limit" or "offset" or "union" or "intersect" or "except" or "window") from = false; expect = false; continue; }
            if (current.Text == "," && from) { expect = true; continue; }
            if (!expect) continue;
            expect = false;
            if (current.Text == "main" && i + 2 < tokens.Count && tokens[i + 1].Text == ".") { i += 2; current = tokens[i]; }
            if (current.Text != before || current.Kind is not (TokenKind.Word or TokenKind.QuotedName)) continue;
            var end = i + 1; var hasAlias = false;
            if (end < tokens.Count && tokens[end].Text == "as") { hasAlias = true; end += 2; }
            else if (end < tokens.Count && (tokens[end].Kind == TokenKind.QuotedName || tokens[end].Kind == TokenKind.Word && !clauses.Contains(tokens[end].Text))) { hasAlias = true; end++; }
            if (after != before) tokens[i] = new(TokenKind.QuotedName, after);
            if (!hasAlias)
            {
                tokens.InsertRange(end, [new(TokenKind.Word, "as"), new(TokenKind.QuotedName, before)]); end += 2;
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
        return string.Join(' ', tokens.Select(Parser.RenderToken));
    }
    private static void RenameViews(PendingDatabase pending, string before, string after, byte kind, string[]? columns = null, int renamedColumn = -1, string? renamedTo = null)
    {
        pending.Catalog = pending.Catalog with { Views = (pending.Catalog.Views ?? []).Select(view => view.Dependencies.Any(dependency => dependency.Kind == kind && dependency.Name == before) ? view with {
            Definition = RewriteViewRelation(view.Definition, before, after, columns, renamedColumn),
            Dependencies = view.Dependencies.Select(dependency => dependency.Kind == kind && dependency.Name == before ? dependency with { Name = after,
                Columns = dependency.Columns.Select(column => columns is not null && renamedColumn >= 0 && column == columns[renamedColumn] ? renamedTo! : column).Order(Utf8NameComparer.Instance).ToArray() } : dependency).OrderBy(dependency => dependency.Name, Utf8NameComparer.Instance).ThenBy(dependency => dependency.Kind).ToArray()
        } : view).ToArray() };
    }
}
