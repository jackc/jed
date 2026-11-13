namespace Jed;

internal static partial class StatementExecution
{
    private static bool RelationExists(Catalog catalog, string name) => catalog.Tables.Any(t => t.Name == name || (t.Indexes ?? []).Any(i => i.Name == name)) ||
        (catalog.Views ?? []).Any(v => v.Name == name) || (catalog.Sequences ?? []).Any(s => s.Name == name);
    private static IEnumerable<string> ConstraintNames(CatalogTable table) => (table.Checks ?? []).Select(c => c.Name)
        .Concat((table.ForeignKeys ?? []).Select(c => c.Name)).Concat((table.Exclusions ?? []).Select(c => c.Name))
        .Concat((table.Indexes ?? []).Where(i => i.IsConstraint).Select(i => i.Name));
    private static string GeneratedName(string table, string part, string label, Func<string, bool> occupied)
    {
        for (var suffix = 0; ; suffix++)
        {
            var tail = label + (suffix == 0 ? "" : suffix.ToString(System.Globalization.CultureInfo.InvariantCulture));
            var first = table; var second = part;
            while (TextEncoding.Utf8.GetByteCount(first) + TextEncoding.Utf8.GetByteCount(second) + tail.Length + (second.Length == 0 ? 1 : 2) > 63)
            {
                if (TextEncoding.Utf8.GetByteCount(first) >= TextEncoding.Utf8.GetByteCount(second)) first = TrimRune(first);
                else second = TrimRune(second);
            }
            var name = first + "_" + (second.Length == 0 ? "" : second + "_") + tail;
            if (!occupied(name)) return name;
        }
    }
    private static string TrimRune(string text) => text.Length > 1 && char.IsLowSurrogate(text[^1]) ? text[..^2] : text[..^1];
    private static IEnumerable<Expression> ExpressionNodes(Expression expression)
    {
        yield return expression;
        foreach (var child in expression.Args) foreach (var node in ExpressionNodes(child)) yield return node;
        if (expression.Predicate is { } predicate) foreach (var node in ExpressionNodes(predicate)) yield return node;
    }
    private static void ValidateStoredExpression(Expression expression, bool isDefault = false, bool immutable = false)
    {
        foreach (var node in ExpressionNodes(expression))
        {
            if (node.Window is not null) throw new SqlException(SqlStates.WindowingError, "window in stored expression");
            if (node.Query is not null) throw Errors.Unsupported("subquery in stored expression");
            if (node.Op == "parameter") throw new SqlException(SqlStates.UndefinedParameter, "parameter in stored expression");
            if (AggregateResolve.IsAggregate(node)) throw new SqlException(SqlStates.GroupingError, "aggregate in stored expression");
            if (node.Op == "name" && isDefault) throw Errors.Unsupported("column reference in DEFAULT");
            if (immutable && node.Op == "stable_date") throw new SqlException(SqlStates.InvalidObjectDefinition, "index expression depends on statement clock");
            if (immutable && node.Op == "call" && node.Text is "now" or "clock_timestamp" or "current_timestamp" or "current_date" or "uuidv4" or "uuidv7" or "nextval" or "currval" or "setval")
                throw new SqlException(SqlStates.InvalidObjectDefinition, "index expression must be immutable");
        }
    }
    private static CatalogColumn WithDefault(CatalogColumn column, Expression? expression, string? text, Meter meter, Catalog catalog)
    {
        column = column with { IntervalFields = column.Type.IntervalFields };
        if (expression is null) return column with { Default = null, DefaultExpression = null };
        expression = Resolve.BindTypes(expression, catalog, meter.Temporal);
        ValidateStoredExpression(expression, isDefault: true);
        if (expression.Op == "text" && (column.Type.Type.IsNumeric() || column.Type.Type == SqlType.Boolean)) throw Errors.Type("cross-family literal DEFAULT");
        if (expression.Op is "null" or "integer" or "decimal" or "text" or "boolean")
            return column with { Default = Assign(expression, column.Type, [], [], meter, catalog), DefaultExpression = null };
        var resolved = Resolve.Expr(expression, column.Type.Type); Assignable(resolved.Type, column.Type.Type);
        return column with { Default = null, DefaultExpression = text ?? throw new InvalidOperationException("missing persisted default text") };
    }
    private static void Assignable(SqlType source, SqlType target)
    {
        if (source != target && source != SqlType.Unknown && !(source == SqlType.F32 && target == SqlType.F64) && !(source.IsInteger() && (target.IsInteger() || target == SqlType.Decimal)))
            throw Errors.Type($"cannot assign {source.Name()} to {target.Name()}");
    }
    private static Expression StoredExpression(string text)
    {
        try { return Parser.ParseExpression(text); }
        catch (SqlException) { throw new SqlException(SqlStates.UnparsableStoredDefinition, "stored expression cannot be parsed"); }
    }
    private static SqlValue DefaultValue(CatalogColumn column, Meter meter, Catalog catalog) => column.DefaultExpression is { } text ?
        Assign(StoredExpression(text), column.Type, [], [], meter, catalog) : column.Default ?? SqlValue.Null(column.Type.Type);

    private static StatementResult CreateType(PendingDatabase pending, CreateTypeSyntax create)
    {
        if ((pending.Catalog.Types ?? []).Any(type => type.Name == create.Name)) throw new SqlException(SqlStates.DuplicateObject, "type already exists");
        var names = new HashSet<string>(StringComparer.Ordinal); var fields = new List<CatalogField>();
        foreach (var field in create.Fields)
        {
            if (!names.Add(field.Name)) throw new SqlException(SqlStates.DuplicateColumn, "duplicate composite field");
            var spec = SqlTypes.ParseSpec(field.Type, pending.Catalog.NamedType);
            fields.Add(new(field.Name, spec, field.NotNull, spec.IntervalFields));
        }
        pending.Catalog = (pending.Catalog with { Types = (pending.Catalog.Types ?? []).Append(new(create.Name, fields.ToArray())).ToArray() }).BindTypes();
        pending.Changed = true; return new([], []);
    }
    private static bool ReferencesType(SqlType type, string name) => type.Kind == SqlKind.Composite && type.Identity == name || type.Element is { } element && ReferencesType(element, name);
    private static StatementResult DropType(PendingDatabase pending, DropTypeSyntax drop)
    {
        if (!(pending.Catalog.Types ?? []).Any(type => type.Name == drop.Name))
        {
            if (drop.IfExists) return new([], []);
            throw new SqlException(SqlStates.UndefinedObject, "type does not exist");
        }
        if ((pending.Catalog.Types ?? []).Any(type => type.Fields.Any(field => ReferencesType(field.Type.Type, drop.Name))) ||
            pending.Catalog.Tables.Any(table => table.Columns.Any(column => ReferencesType(column.Type.Type, drop.Name))))
        {
            if (drop.Cascade) throw Errors.Unsupported("DROP TYPE CASCADE with stored dependents");
            throw new SqlException(SqlStates.DependentObjectsStillExist, "type has dependents");
        }
        DropDependentViews(pending, drop.Name, 3, drop.Cascade);
        pending.Catalog = pending.Catalog with { Types = (pending.Catalog.Types ?? []).Where(type => type.Name != drop.Name).ToArray() };
        pending.Changed = true; return new([], []);
    }

    private static StatementResult CreateTable(PendingDatabase pending, CreateTableSyntax create, Meter meter)
    {
        if (RelationExists(pending.Catalog, create.Name))
        {
            if (create.IfNotExists) return new([], []);
            throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
        }
        var names = new HashSet<string>(StringComparer.Ordinal); var columns = new List<CatalogColumn>();
        foreach (var column in create.Columns)
        {
            if (!names.Add(column.Name)) throw new SqlException(SqlStates.DuplicateColumn, "duplicate column");
            columns.Add(CreateColumn(pending, create.Name, columns.Count, column, meter, (create.Constraints ?? []).Where(constraint => constraint.Kind == "unique" && constraint.Name is not null).Select(constraint => constraint.Name!)));
        }
        var definitions = create.Constraints ?? [];
        if (create.PrimaryKey.Length != 0) definitions = [new("primary", null, create.PrimaryKey), .. definitions];
        var table = new CatalogTable(create.Name, columns.ToArray(), []);
        table = AddDefinitions(pending.Catalog, table, definitions, meter);
        Put(pending, table); return new([], []);
    }

    private static CatalogTable AddDefinitions(Catalog catalog, CatalogTable table, ConstraintSyntax[] definitions, Meter meter)
    {
        var primaries = definitions.Where(c => c.Kind == "primary").ToArray();
        if (primaries.Length > 1 || primaries.Length != 0 && table.PrimaryKey.Length != 0)
            throw new SqlException(SqlStates.InvalidTableDefinition, "multiple primary keys");
        if (primaries.Length != 0)
        {
            var primary = primaries[0].Columns.Select(c => ColumnIndex(table, c)).ToArray();
            if (primary.Distinct().Count() != primary.Length) throw new SqlException(SqlStates.DuplicateColumn, "duplicate primary-key column");
            foreach (var ordinal in primary) Keyable(table.Columns[ordinal].Type.Type);
            var columns = table.Columns.ToArray(); foreach (var ordinal in primary) columns[ordinal] = columns[ordinal] with { NotNull = true };
            table = table with { PrimaryKey = primary, Columns = columns };
        }
        var folded = new List<ConstraintSyntax>();
        foreach (var definition in definitions)
        {
            if (definition.Kind != "unique") { folded.Add(definition); continue; }
            var members = definition.Columns.Select(c => ColumnIndex(table, c)).ToArray();
            if (members.Distinct().Count() != members.Length) throw new SqlException(SqlStates.DuplicateColumn, "duplicate unique column");
            foreach (var member in members) Keyable(table.Columns[member].Type.Type);
            if (members.SequenceEqual(table.PrimaryKey)) continue;
            var prior = folded.FindIndex(item => item.Kind == "unique" && item.Columns.SequenceEqual(definition.Columns));
            if (prior < 0) folded.Add(definition);
            else if (folded[prior].Name is null && definition.Name is not null) folded[prior] = folded[prior] with { Name = definition.Name };
        }
        definitions = folded.ToArray();
        foreach (var definition in definitions.Where(d => d.Kind == "check"))
        {
            ValidateStoredExpression(definition.Expression!);
            var resolved = Resolve.Expr(Resolve.Bind(Resolve.BindTypes(definition.Expression!, catalog, meter.Temporal), Columns(table)));
            if (resolved.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("CHECK must be boolean");
        }
        var checks = (table.Checks ?? []).ToList(); var indexes = (table.Indexes ?? []).ToList(); var foreignKeys = (table.ForeignKeys ?? []).ToList();
        var names = ConstraintNames(table).ToHashSet(StringComparer.Ordinal);
        var globalNames = catalog.Tables.SelectMany(ConstraintNames).ToHashSet(StringComparer.Ordinal); globalNames.UnionWith(names);
        foreach (var definition in definitions.Where(d => d.Kind != "primary"))
        {
            var ordinals = definition.Kind == "check" ? [] : definition.Columns.Select(c => ColumnIndex(table, c)).ToArray();
            string label; string part;
            if (definition.Kind == "check")
            {
                var used = ExpressionNodes(definition.Expression!).Where(e => e.Op == "name").Select(e => e.Text.Split('.')[^1]).Distinct(StringComparer.Ordinal).ToArray();
                part = used.Length == 1 ? used[0] : ""; label = "check";
            }
            else { part = string.Join('_', definition.Columns); label = definition.Kind == "unique" ? "key" : "fkey"; }
            var name = definition.Name ?? GeneratedName(table.Name, part, label, candidate => globalNames.Contains(candidate) || definition.Kind == "unique" && (RelationExists(catalog, candidate) || indexes.Any(i => i.Name == candidate)));
            if (definition.Kind == "unique" && (name == table.Name || RelationExists(catalog, name) || indexes.Any(index => index.Name == name)))
                throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
            if (!names.Add(name)) throw new SqlException(SqlStates.DuplicateObject, "duplicate constraint name");
            globalNames.Add(name);
            switch (definition.Kind)
            {
                case "check": checks.Add(new(name, definition.Text!)); break;
                case "unique":
                    if (ordinals.Distinct().Count() != ordinals.Length) throw new SqlException(SqlStates.DuplicateColumn, "duplicate unique column");
                    foreach (var ordinal in ordinals) Keyable(table.Columns[ordinal].Type.Type);
                    if (RelationExists(catalog, name) || indexes.Any(index => index.Name == name)) throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
                    indexes.Add(new(name, ordinals.Select(ordinal => new CatalogIndexKey(ordinal)).ToArray(), Unique: true, IsConstraint: true)); break;
                case "foreign":
                    var parent = definition.Parent == table.Name ? table : Table(catalog, definition.Parent!);
                    var referenced = definition.ParentColumns is null ? parent.PrimaryKey : definition.ParentColumns.Select(c => ColumnIndex(parent, c)).ToArray();
                    if (definition.ParentColumns is null && referenced.Length == 0) throw new SqlException(SqlStates.UndefinedObject, "referenced table has no primary key");
                    if (ordinals.Distinct().Count() != ordinals.Length || referenced.Distinct().Count() != referenced.Length) throw new SqlException(SqlStates.DuplicateColumn, "duplicate foreign key column");
                    if (referenced.Length == 0 || referenced.Length != ordinals.Length) throw new SqlException(SqlStates.InvalidForeignKey, "foreign key column count mismatch");
                    for (var i = 0; i < ordinals.Length; i++)
                        if (!ForeignKeyTypes(table.Columns[ordinals[i]].Type.Type, parent.Columns[referenced[i]].Type.Type)) throw Errors.Type("foreign key types must agree");
                    if (!SameColumns(referenced, parent.PrimaryKey) && !(parent.Indexes ?? []).Concat(parent.Name == table.Name ? indexes : []).Any(index => index.Unique && index.Predicate is null && index.Keys.All(k => k.Expression is null) && SameColumns(index.Keys.Select(k => k.Column), referenced)))
                        throw new SqlException(SqlStates.InvalidForeignKey, "foreign key target is not unique");
                    foreignKeys.Add(new(name, ordinals, parent.Name, referenced, definition.OnDelete, definition.OnUpdate, definition.MatchFull)); break;
                default: throw Errors.Unsupported("constraint kind");
            }
        }
        return table with { Checks = checks.OrderBy(c => c.Name, Utf8NameComparer.Instance).ToArray(),
            Indexes = indexes.OrderBy(c => c.Name, Utf8NameComparer.Instance).ToArray(), ForeignKeys = foreignKeys.OrderBy(c => c.Name, Utf8NameComparer.Instance).ToArray() };
    }

    private static ValueTask<StatementResult> AlterTable(Database database, PendingDatabase pending, AlterTableSyntax alter, Meter meter, CancellationToken token) =>
        AlterFrame(database, pending, [alter], meter, token);
}
