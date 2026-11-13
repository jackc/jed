namespace Jed;

internal static class SequenceExecution
{
    internal static bool Writes(SelectSyntax query, Catalog catalog) => new WriteDiscovery(catalog).Query(query, [], 0);

    private sealed class WriteDiscovery(Catalog catalog)
    {
        private readonly HashSet<string> activeViews = new(StringComparer.Ordinal);
        private int expandedViews;

        internal bool Query(SelectSyntax query, HashSet<string> inherited, int depth)
        {
            if (depth > 256) throw new SqlException(SqlStates.StatementTooComplex, "query discovery nesting exceeds 256");
            var visible = new HashSet<string>(inherited, StringComparer.Ordinal);
            var definitions = query.With ?? [];
            if (query.Recursive) foreach (var definition in definitions) visible.Add(definition.Name);
            var writes = false;
            foreach (var definition in definitions)
            {
                // Ordinary WITH bodies see earlier siblings; recursive bodies also see
                // their own name. Nested queries inherit precisely this lexical scope.
                writes |= Query(definition.Query, visible, depth + 1);
                visible.Add(definition.Name);
            }
            foreach (var item in query.Items) writes |= Expression(item.Expression, visible, depth);
            writes |= Expression(query.Filter, visible, depth);
            writes |= Expression(query.Having, visible, depth);
            foreach (var order in query.Order ?? []) writes |= Expression(order.Expression, visible, depth);
            foreach (var group in query.GroupBy ?? []) writes |= Expression(group, visible, depth);
            if (query.Windows is { } windows) foreach (var window in windows.Values) writes |= Window(window, visible, depth);
            writes |= Relation(query.Relation, visible, depth);
            return writes;
        }
        private bool Expression(Expression? expression, HashSet<string> visible, int depth)
        {
            if (expression is null) return false;
            var writes = expression.Op == "call" && expression.Text is "nextval" or "setval";
            foreach (var argument in expression.Args) writes |= Expression(argument, visible, depth);
            writes |= Expression(expression.Predicate, visible, depth);
            foreach (var order in expression.AggregateOrder ?? []) writes |= Expression(order.Expression, visible, depth);
            writes |= Window(expression.Window, visible, depth);
            if (expression.Query is { } query) writes |= Query(query, visible, depth + 1);
            return writes;
        }
        private bool Window(WindowSyntax? window, HashSet<string> visible, int depth)
        {
            if (window is null) return false;
            var writes = false;
            foreach (var expression in window.Partition) writes |= Expression(expression, visible, depth);
            foreach (var order in window.Order) writes |= Expression(order.Expression, visible, depth);
            writes |= Expression(window.Frame?.Start.Offset, visible, depth);
            writes |= Expression(window.Frame?.End.Offset, visible, depth);
            return writes;
        }
        private bool Relation(RelationSyntax? source, HashSet<string> visible, int depth)
        {
            switch (source)
            {
                case TableRelation table:
                {
                    if (visible.Contains(table.Name)) return false;
                    var view = (catalog.Views ?? []).FirstOrDefault(view => view.Name == table.Name);
                    if (view is null) return false;
                    if (!activeViews.Add(view.Name)) throw new SqlException(SqlStates.InvalidObjectDefinition, "recursive view definition");
                    if (++expandedViews > 1024) throw new SqlException(SqlStates.StatementTooComplex, "view expansion exceeds 1024 references");
                    try { return Query(StatementExecution.ViewQuery(view), [], depth + 1); }
                    finally { activeViews.Remove(view.Name); }
                }
                case LateralRelation lateral: return Relation(lateral.Source, visible, depth);
                case AliasedRelation aliased: return Relation(aliased.Source, visible, depth);
                case FunctionRelation function: return Expression(function.Function, visible, depth);
                case ValuesRelation values:
                {
                    var writes = false;
                    foreach (var row in values.Values) foreach (var expression in row) writes |= Expression(expression, visible, depth);
                    return writes;
                }
                case DerivedRelation derived: return Query(derived.Query, visible, depth + 1);
                case JoinedRelation join: return Relation(join.Left, visible, depth) | Relation(join.Right, visible, depth) | Expression(join.On, visible, depth);
                case SetRelation set: return Query(set.Left, visible, depth + 1) | Query(set.Right, visible, depth + 1);
                default: return false;
            }
        }
    }
}

internal static partial class StatementExecution
{
    private static CatalogSequence SequenceDefinition(string name, SequenceOptions options, CatalogSequence? before = null, SqlType? forcedType = null)
    {
        var type = forcedType ?? (options.Type is null ? SqlType.I64 : SqlTypes.ParseSpec(options.Type).Type);
        if (!type.IsInteger()) throw new SqlException(SqlStates.InvalidParameterValue, "sequence type must be an integer");
        var lower = type.Kind switch { SqlKind.I16 => short.MinValue, SqlKind.I32 => int.MinValue, _ => long.MinValue };
        var upper = type.Kind switch { SqlKind.I16 => short.MaxValue, SqlKind.I32 => int.MaxValue, _ => long.MaxValue };
        var increment = options.Increment ?? before?.Increment ?? 1;
        var minimum = options.Minimum ?? (options.NoMinimum || before is null ? increment > 0 ? 1 : lower : before.Minimum);
        var maximum = options.Maximum ?? (options.NoMaximum || before is null ? increment > 0 ? upper : -1 : before.Maximum);
        var start = options.Start ?? before?.Start ?? (increment > 0 ? minimum : maximum);
        var cache = options.Cache ?? before?.Cache ?? 1;
        var last = options.Restart ? options.RestartValue ?? start : before?.LastValue ?? start;
        if (increment == 0 || cache < 1 || minimum < lower || maximum > upper || minimum >= maximum || start < minimum || start > maximum || last < minimum || last > maximum)
            throw new SqlException(SqlStates.InvalidParameterValue, "invalid sequence bounds or options");
        return new(name, increment, minimum, maximum, start, cache, last, options.Cycle ?? before?.Cycle ?? false,
            !options.Restart && (before?.IsCalled ?? false), before?.OwnerTable, before?.OwnerColumn ?? 0);
    }
    private static string SequenceDefault(string name)
    {
        var bare = name.Length != 0 && (name[0] is >= 'a' and <= 'z' or '_') && name.All(character => character is >= 'a' and <= 'z' or >= '0' and <= '9' or '_');
        var identifier = bare ? name : "\"" + name.Replace("\"", "\"\"", StringComparison.Ordinal) + "\"";
        return "nextval ( '" + identifier.Replace("'", "''", StringComparison.Ordinal) + "' )";
    }
    private static CatalogSequence IdentitySequence(Catalog catalog, CatalogColumn column)
    {
        var expression = StoredExpression(column.DefaultExpression ?? throw StorageFormat.Corrupt("identity has no default"));
        var call = ExpressionNodes(expression).FirstOrDefault(node => node.Op == "call" && node.Text == "nextval" && node.Args.Length == 1 && node.Args[0].Op == "text")
            ?? throw StorageFormat.Corrupt("identity has no sequence reference");
        var name = SequenceContext.Name(call.Args[0].Text);
        return (catalog.Sequences ?? []).FirstOrDefault(sequence => sequence.Name == name) ?? throw StorageFormat.Corrupt("identity sequence is absent");
    }
    private static CatalogColumn CreateColumn(PendingDatabase pending, string table, int ordinal, ColumnSyntax definition, Meter meter, IEnumerable<string>? reserved = null)
    {
        var serialType = definition.Type switch { "serial" or "serial4" => "i32", "bigserial" or "serial8" => "i64", "smallserial" or "serial2" => "i16", _ => null };
        if (serialType is not null && (definition.Default is not null || definition.Identity)) throw Errors.Syntax("multiple defaults or identities");
        var type = SqlTypes.ParseSpec(serialType ?? definition.Type, pending.Catalog.NamedType);
        if (serialType is null && !definition.Identity) return WithDefault(new(definition.Name, type, definition.NotNull), definition.Default, definition.DefaultText, meter, pending.Catalog);
        if (!type.Type.IsInteger()) throw new SqlException(SqlStates.InvalidParameterValue, "identity column type must be integer");
        var options = definition.Sequence ?? new SequenceOptions();
        var names = (reserved ?? []).ToHashSet(StringComparer.Ordinal);
        var name = options.SequenceName ?? GeneratedName(table, definition.Name, "seq", candidate => candidate == table || names.Contains(candidate) || RelationExists(pending.Catalog, candidate));
        if (name == table || names.Contains(name) || RelationExists(pending.Catalog, name)) throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
        if (name.StartsWith("jed_", StringComparison.Ordinal)) throw new SqlException(SqlStates.ReservedName, "reserved sequence name");
        PutSequence(pending, SequenceDefinition(name, options, forcedType: type.Type) with { OwnerTable = table, OwnerColumn = ordinal });
        return new(definition.Name, type, true, DefaultExpression: SequenceDefault(name), Identity: definition.Identity, IdentityAlways: definition.IdentityAlways);
    }
    private static void PutSequence(PendingDatabase pending, CatalogSequence sequence)
    {
        pending.Catalog = pending.Catalog with { Sequences = (pending.Catalog.Sequences ?? []).Where(item => item.Name != sequence.Name).Append(sequence).ToArray() };
        pending.Changed = true;
    }
    private static StatementResult CreateSequence(PendingDatabase pending, CreateSequenceSyntax create)
    {
        if (RelationExists(pending.Catalog, create.Name))
        {
            if (create.IfNotExists) return new([], []);
            throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
        }
        if (create.Name.StartsWith("jed_", StringComparison.Ordinal)) throw new SqlException(SqlStates.ReservedName, "reserved sequence name");
        PutSequence(pending, SequenceDefinition(create.Name, create.Options)); return new([], []);
    }
    private static bool SequenceNameEquals(string text, string name)
    {
        try { return SequenceContext.Name(text) == name; }
        catch (SqlException error) when (error.SqlState == SqlStates.InvalidName) { return false; }
    }
    private static bool SequenceDependency(string text, string name) => ExpressionNodes(StoredExpression(text)).Any(node =>
        node.Op == "call" && node.Text is "nextval" or "currval" or "setval" && node.Args.Length > 0 && node.Args[0].Op == "text" && SequenceNameEquals(node.Args[0].Text, name));
    private static string RenameSequenceCalls(string text, string before, string after)
    {
        var lexer = new Lexer(text); var tokens = new List<Token>();
        for (var next = lexer.Next(); next.Kind != TokenKind.End; next = lexer.Next()) tokens.Add(next);
        for (var i = 0; i + 2 < tokens.Count; i++)
            if (tokens[i].Kind == TokenKind.Word && tokens[i].Text is "nextval" or "currval" or "setval" && tokens[i + 1].Text == "(" &&
                tokens[i + 2].Kind == TokenKind.String && SequenceNameEquals(tokens[i + 2].Text, before))
            {
                var quoted = "\"" + after.Replace("\"", "\"\"", StringComparison.Ordinal) + "\"";
                tokens[i + 2] = new(TokenKind.String, quoted);
            }
        return string.Join(' ', tokens.Select(Parser.RenderToken));
    }
    private static StatementResult AlterSequence(PendingDatabase pending, AlterSequenceSyntax alter, Meter meter)
    {
        var sequence = (pending.Catalog.Sequences ?? []).FirstOrDefault(sequence => sequence.Name == alter.Name);
        if (sequence is null)
        {
            if (alter.IfExists) return new([], []);
            throw new SqlException(SqlStates.UndefinedTable, "sequence does not exist");
        }
        if (alter.Rename is { } name)
        {
            if (RelationExists(pending.Catalog, name)) throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
            if (name.StartsWith("jed_", StringComparison.Ordinal)) throw new SqlException(SqlStates.ReservedName, "reserved sequence name");
            pending.Catalog = pending.Catalog with
            {
                Sequences = (pending.Catalog.Sequences ?? []).Where(item => item.Name != sequence.Name).ToArray(),
                Tables = pending.Catalog.Tables.Select(table => table with { Columns = table.Columns.Select(column => column.DefaultExpression is { } text && SequenceDependency(text, sequence.Name) ?
                    column with { DefaultExpression = RenameSequenceCalls(text, sequence.Name, name) } : column).ToArray() }).ToArray(),
                Views = (pending.Catalog.Views ?? []).Select(view => view with { Definition = RenameSequenceCalls(view.Definition, sequence.Name, name), Dependencies = view.Dependencies.Select(dependency => dependency.Kind == 2 && dependency.Name == sequence.Name ? dependency with { Name = name } : dependency).OrderBy(dependency => dependency.Name, Utf8NameComparer.Instance).ThenBy(dependency => dependency.Kind).ToArray(), Columns = view.Columns.Select(column => column.DefaultExpression is { } text && SequenceDependency(text, sequence.Name) ?
                    column with { DefaultExpression = RenameSequenceCalls(text, sequence.Name, name) } : column).ToArray() }).ToArray()
            };
            if (meter.Sequences.State.Current.Remove(sequence.Name, out var value)) meter.Sequences.State.Current[name] = value;
            if (meter.Sequences.State.Last == sequence.Name) meter.Sequences.State.Last = name;
            PutSequence(pending, sequence with { Name = name });
        }
        else PutSequence(pending, SequenceDefinition(sequence.Name, alter.Options, sequence));
        return new([], []);
    }
    private static StatementResult DropSequence(PendingDatabase pending, DropSequenceSyntax drop, Meter meter)
    {
        foreach (var name in drop.Names)
        {
            var sequence = (pending.Catalog.Sequences ?? []).FirstOrDefault(sequence => sequence.Name == name);
            if (sequence is null)
            {
                if (drop.IfExists) continue;
                throw new SqlException(SqlStates.UndefinedTable, "sequence does not exist");
            }
            DropDependentViews(pending, name, 2, drop.Cascade);
            var dependent = sequence.OwnerTable is not null || pending.Catalog.Tables.Any(table => table.Columns.Any(column => column.DefaultExpression is { } text && SequenceDependency(text, name))) ||
                (pending.Catalog.Views ?? []).Any(view => view.Columns.Any(column => column.DefaultExpression is { } text && SequenceDependency(text, name)));
            if (dependent)
            {
                if (drop.Cascade) throw Errors.Unsupported("DROP SEQUENCE CASCADE with column dependents");
                throw new SqlException(SqlStates.DependentObjectsStillExist, "column depends on sequence");
            }
            pending.Catalog = pending.Catalog with { Sequences = (pending.Catalog.Sequences ?? []).Where(sequence => sequence.Name != name).ToArray() };
            meter.Sequences.State.Current.Remove(name); pending.Changed = true;
        }
        return new([], []);
    }
}
