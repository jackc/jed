namespace Jed;

internal static partial class StatementExecution
{
    private sealed record AlterValue(int Original, CatalogColumn? Added = null, TypeSpec? Cast = null);
    private sealed record AlterRowStep(string Operation, int Ordinal, CatalogColumn? Added = null, TypeSpec? Type = null, Resolved? Expression = null);
    private static bool ExpressionUses(string text, string column) => ExpressionNodes(StoredExpression(text)).Any(node => node.Op == "name" && node.Text.Split('.')[^1] == column);
    private static string RenameExpression(string text, string before, string after)
    {
        _ = StoredExpression(text);
        var lexer = new Lexer(text); var tokens = new List<Token>();
        for (var token = lexer.Next(); token.Kind != TokenKind.End; token = lexer.Next()) tokens.Add(token);
        for (var i = 0; i < tokens.Count; i++)
            if (tokens[i].Kind is TokenKind.Word or TokenKind.QuotedName && tokens[i].Text == before && (i + 1 == tokens.Count || tokens[i + 1].Text != "("))
                tokens[i] = new(TokenKind.QuotedName, after);
        return string.Join(' ', tokens.Select(Parser.RenderToken));
    }
    private static async ValueTask<StatementResult> AlterFrame(Database database, PendingDatabase pending, AlterTableSyntax[] actions, Meter meter, CancellationToken token)
    {
        if (actions[0].TableIfExists && !RelationExists(pending.Catalog, actions[0].Table)) return new([], []);
        if ((pending.Catalog.Views ?? []).Any(view => view.Name == actions[0].Table))
        {
            foreach (var action in actions)
            {
                var viewAction = action.Operation switch
                {
                    "default" => new AlterViewSyntax(action.Table, "default", action.Column, Default: action.Expression, DefaultText: action.Text),
                    "drop_default" => new AlterViewSyntax(action.Table, "default", action.Column),
                    "rename" => new AlterViewSyntax(action.Table, "rename", Value: action.Name),
                    "rename_column" => new AlterViewSyntax(action.Table, "rename_column", action.Column, action.Name),
                    _ => throw new SqlException(SqlStates.WrongObjectType, "ALTER TABLE action requires a table")
                };
                _ = AlterView(database, pending, viewAction, meter, token);
            }
            return new([], []);
        }
        if (!pending.Catalog.Tables.Any(table => table.Name == actions[0].Table) && RelationExists(pending.Catalog, actions[0].Table))
            throw new SqlException(SqlStates.WrongObjectType, "ALTER TABLE target is not a table");
        var original = Table(pending.Catalog, actions[0].Table);
        if (actions.Any(action => action.Operation is "type" or "drop_column" or "rename" or "rename_column"))
            foreach (var index in original.Indexes ?? []) ParseIndexDefinitions(index); var table = original;
        var removedIndexNames = new HashSet<string>(StringComparer.Ordinal);
        var retypedColumns = new HashSet<string>(StringComparer.Ordinal);
        var values = Enumerable.Range(0, original.Columns.Length).Select(i => new AlterValue(i)).ToList();
        var rewrite = false; var validate = false; var changed = false;
        var rowSteps = new List<AlterRowStep>();
        foreach (var action in actions)
        {
            var columns = table.Columns.ToArray();
            var ordinal = action.Column is null || action.Operation is "drop_constraint" or "rename_constraint" ? -1 : Array.FindIndex(columns, column => column.Name == action.Column);
            if (action.Column is not null && action.Operation is not ("drop_constraint" or "rename_constraint") && ordinal < 0)
            {
                if (action.Operation == "drop_column" && action.IfExists) continue;
                throw new SqlException(SqlStates.UndefinedColumn, "column does not exist");
            }
            switch (action.Operation)
            {
                case "default": if (columns[ordinal].Identity) throw Errors.Syntax("identity column default is owned"); columns[ordinal] = WithDefault(columns[ordinal], action.Expression, action.Text, meter, pending.Catalog); table = table with { Columns = columns }; break;
                case "drop_default": if (columns[ordinal].Identity) throw Errors.Syntax("identity column default is owned"); columns[ordinal] = columns[ordinal] with { Default = null, DefaultExpression = null }; table = table with { Columns = columns }; break;
                case "nullable":
                    if (columns[ordinal].Identity) throw Errors.Syntax("identity column nullability is owned");
                    if (table.PrimaryKey.Contains(ordinal)) throw new SqlException(SqlStates.InvalidTableDefinition, "primary key column cannot be nullable");
                    columns[ordinal] = columns[ordinal] with { NotNull = false }; table = table with { Columns = columns }; break;
                case "not_null": columns[ordinal] = columns[ordinal] with { NotNull = true }; table = table with { Columns = columns }; validate = true; break;
                case "add_identity":
                {
                    var column = columns[ordinal];
                    if (!column.Type.Type.IsInteger()) throw new SqlException(SqlStates.InvalidParameterValue, "identity column type must be integer");
                    _ = SequenceDefinition("", action.Sequence ?? new(), forcedType: column.Type.Type);
                    if (!column.NotNull || column.Identity || column.Default is not null || column.DefaultExpression is not null)
                        throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "column is not eligible for identity");
                    var syntax = new ColumnSyntax(column.Name, column.Type.Type.Name(), true, false, null, Identity: true, IdentityAlways: action.Name == "always", Sequence: action.Sequence);
                    columns[ordinal] = CreateColumn(pending, table.Name, ordinal, syntax, meter); table = table with { Columns = columns }; break;
                }
                case "drop_identity":
                {
                    if (!columns[ordinal].Identity)
                    {
                        if (action.IfExists) continue;
                        throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "column is not an identity");
                    }
                    var sequence = IdentitySequence(pending.Catalog, columns[ordinal]);
                    DropDependentViews(pending, sequence.Name, 2, cascade: false);
                    pending.Catalog = pending.Catalog with { Sequences = (pending.Catalog.Sequences ?? []).Where(item => item.Name != sequence.Name).ToArray() };
                    meter.Sequences.State.Current.Remove(sequence.Name);
                    columns[ordinal] = columns[ordinal] with { Identity = false, IdentityAlways = false, Default = null, DefaultExpression = null };
                    table = table with { Columns = columns }; break;
                }
                case "identity_options":
                {
                    if (!columns[ordinal].Identity) throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "column is not an identity");
                    var sequence = IdentitySequence(pending.Catalog, columns[ordinal]);
                    PutSequence(pending, SequenceDefinition(sequence.Name, action.Sequence ?? new(), sequence, columns[ordinal].Type.Type));
                    if (action.Name is { } generation) columns[ordinal] = columns[ordinal] with { IdentityAlways = generation == "always" };
                    table = table with { Columns = columns }; break;
                }
                case "add_column":
                {
                    var definition = action.Definition!;
                    if (columns.Any(column => column.Name == definition.Name))
                    {
                        if (action.IfExists) continue;
                        throw new SqlException(SqlStates.DuplicateColumn, "column already exists");
                    }
                    var column = CreateColumn(pending, table.Name, columns.Length, definition, meter, actions.SelectMany(action => action.AddedConstraints ?? []).Where(constraint => constraint.Kind == "unique" && constraint.Name is not null).Select(constraint => constraint.Name!));
                    rowSteps.Add(new("add", columns.Length, Added: column));
                    values.Add(new(-1, column)); table = table with { Columns = [.. columns, column] };
                    table = AddDefinitions(pending.Catalog, table, action.AddedConstraints ?? [], meter); rewrite = true; break;
                }
                case "drop_column":
                {
                    if (table.PrimaryKey.Contains(ordinal)) throw Errors.Unsupported("dropping primary key member");
                    var name = columns[ordinal].Name;
                    DropDependentViews(pending, table.Name, 0, action.Cascade, name);
                    foreach (var owned in (pending.Catalog.Sequences ?? []).Where(sequence => sequence.OwnerTable == table.Name && sequence.OwnerColumn == ordinal).ToArray())
                        DropDependentViews(pending, owned.Name, 2, action.Cascade);
                    DropViewColumnAliases(pending, table.Name, ordinal);
                    static int Shift(int value, int removed) => value > removed ? value - 1 : value;
                    if (!action.Cascade && (table.Exclusions ?? []).Any(exclusion => !exclusion.Columns.Contains(ordinal) &&
                        table.Indexes!.Any(index => index.Name == exclusion.Index && index.Predicate is { } predicate && ExpressionUses(predicate, name))))
                        throw new SqlException(SqlStates.DependentObjectsStillExist, "exclusion predicate depends on column");
                    var retainedIndexes = (table.Indexes ?? []).Where(index => !index.Keys.Any(key => key.Column == ordinal || key.Expression is { } expr && ExpressionUses(expr, name)) &&
                        (index.Predicate is null || !ExpressionUses(index.Predicate, name))).ToArray();
                    removedIndexNames.UnionWith((table.Indexes ?? []).Except(retainedIndexes).Select(index => index.Name));
                    foreach (var child in pending.Catalog.Tables.Select(item => item.Name == table.Name ? table : item).ToArray())
                    {
                        var foreignKeys = new List<CatalogForeignKey>();
                        foreach (var foreign in child.ForeignKeys ?? [])
                        {
                            if (foreign.Parent != table.Name) { foreignKeys.Add(foreign); continue; }
                            if (foreign.ParentColumns.Contains(ordinal))
                            {
                                if (!action.Cascade) throw new SqlException(SqlStates.DependentObjectsStillExist, "foreign key references column");
                                continue;
                            }
                            foreignKeys.Add(foreign with { ParentColumns = foreign.ParentColumns.Select(value => Shift(value, ordinal)).ToArray() });
                        }
                        if (child.Name == table.Name) table = table with { ForeignKeys = foreignKeys.ToArray() };
                        else Put(pending, child with { ForeignKeys = foreignKeys.ToArray() });
                    }
                    table = table with
                    {
                        Columns = columns.Where((_, i) => i != ordinal).ToArray(),
                        PrimaryKey = table.PrimaryKey.Select(value => Shift(value, ordinal)).ToArray(),
                        Checks = (table.Checks ?? []).Where(check => !ExpressionUses(check.Expression, name)).ToArray(),
                        Indexes = retainedIndexes.Select(index => index with { Keys = index.Keys.Select(key => key.Column < 0 ? key : key with { Column = Shift(key.Column, ordinal) }).ToArray() }).ToArray(),
                        Exclusions = (table.Exclusions ?? []).Where(exclusion => retainedIndexes.Any(index => index.Name == exclusion.Index)).Select(exclusion => exclusion with { Columns = exclusion.Columns.Select(value => Shift(value, ordinal)).ToArray() }).ToArray(),
                        ForeignKeys = (table.ForeignKeys ?? []).Where(foreign => !foreign.Columns.Contains(ordinal)).Select(foreign => foreign with { Columns = foreign.Columns.Select(value => Shift(value, ordinal)).ToArray() }).ToArray()
                    };
                    if (table.Columns.Length == 0) throw Errors.Unsupported("zero-column tables");
                    foreach (var sequence in (pending.Catalog.Sequences ?? []).Where(sequence => sequence.OwnerTable == table.Name && sequence.OwnerColumn == ordinal)) meter.Sequences.State.Current.Remove(sequence.Name);
                    pending.Catalog = pending.Catalog with { Sequences = (pending.Catalog.Sequences ?? []).Where(sequence => sequence.OwnerTable != table.Name || sequence.OwnerColumn != ordinal)
                        .Select(sequence => sequence.OwnerTable == table.Name ? sequence with { OwnerColumn = Shift(sequence.OwnerColumn, ordinal) } : sequence).ToArray() };
                    rowSteps.Add(new("drop", ordinal)); values.RemoveAt(ordinal); rewrite = true; break;
                }
                case "type":
                    WritableCheckDependencies(table, pending.Catalog, meter);
                {
                    retypedColumns.Add(columns[ordinal].Name);
                    var type = SqlTypes.ParseSpec(action.Name!, pending.Catalog.NamedType);
                    if (columns[ordinal].Identity)
                    {
                        if (!type.Type.IsInteger()) throw new SqlException(SqlStates.InvalidParameterValue, "identity column type must be integer");
                        var sequence = IdentitySequence(pending.Catalog, columns[ordinal]);
                        var oldDefault = SequenceDefinition("", new(Increment: sequence.Increment), forcedType: columns[ordinal].Type.Type);
                        var bounds = new SequenceOptions(NoMinimum: sequence.Minimum == oldDefault.Minimum, NoMaximum: sequence.Maximum == oldDefault.Maximum);
                        PutSequence(pending, SequenceDefinition(sequence.Name, bounds, sequence, type.Type));
                    }
                    var expression = action.Expression is null ? null : Resolve.Expr(Resolve.Bind(Resolve.BindTypes(action.Expression, pending.Catalog, meter.Temporal), Columns(table)));
                    expression ??= new("column", columns[ordinal].Type.Type, default, [], Slot: ordinal);
                    rowSteps.Add(new("type", ordinal, Type: type, Expression: new Resolved("cast", type.Type, default, [expression], Target: type)));
                    var changedColumn = columns[ordinal] with { Type = type, IntervalFields = type.IntervalFields };
                    if (!CollationRules.Collatable(type.Type)) changedColumn = changedColumn with { Collation = null, CollationData = null, CollationSkew = false };
                    else if (!CollationRules.Collatable(columns[ordinal].Type.Type)) changedColumn = ColumnCollation(pending, changedColumn, null, meter);
                    if (changedColumn.Default is { } constant) changedColumn = changedColumn with { Default = Evaluate.Expr(new("cast", type.Type, default, [new Resolved("constant", constant.Type, constant, [])], Target: type), meter) };
                    if (changedColumn.DefaultExpression is { } defaultText)
                    {
                        _ = StoredExpression(defaultText);
                        var castText = "CAST ( " + defaultText + " AS " + action.Name + " )";
                        changedColumn = WithDefault(changedColumn, Parser.ParseExpression(castText), castText, meter, pending.Catalog);
                    }
                    columns[ordinal] = changedColumn; table = table with { Columns = columns };
                    values[ordinal] = values[ordinal] with { Cast = type }; rewrite = true; break;
                }
                case "add_constraint":
                    table = AddDefinitions(pending.Catalog, table, [action.Constraint!], meter); validate = true;
                    rewrite |= action.Constraint!.Kind == "primary"; break;
                case "drop_primary":
                {
                    if (table.PrimaryKey.Length == 0) throw new SqlException(SqlStates.UndefinedObject, "table has no primary key");
                    foreach (var child in pending.Catalog.Tables.Select(item => item.Name == table.Name ? table : item).ToArray())
                    {
                        var dependent = (child.ForeignKeys ?? []).Where(foreign => foreign.Parent == table.Name && SameColumns(foreign.ParentColumns, table.PrimaryKey)).ToArray();
                        if (dependent.Length == 0) continue;
                        if (!action.Cascade) throw new SqlException(SqlStates.DependentObjectsStillExist, "foreign key references primary key");
                        var updated = child with { ForeignKeys = (child.ForeignKeys ?? []).Except(dependent).ToArray() };
                        if (child.Name == table.Name) table = updated; else Put(pending, updated);
                    }
                    table = table with { PrimaryKey = [] }; rewrite = true; break;
                }
                case "drop_constraint":
                {
                    var name = action.Column!;
                    if (!ConstraintNames(table).Contains(name, StringComparer.Ordinal))
                    {
                        if (action.IfExists) continue;
                        throw new SqlException(SqlStates.UndefinedObject, "constraint does not exist");
                    }
                    var excludedIndex = (table.Exclusions ?? []).FirstOrDefault(exclusion => exclusion.Name == name)?.Index;
                    var remaining = (table.Indexes ?? []).Where(index => (!index.IsConstraint || index.Name != name) && index.Name != excludedIndex).ToArray();
                    removedIndexNames.UnionWith((table.Indexes ?? []).Except(remaining).Select(index => index.Name));
                    foreach (var child in pending.Catalog.Tables.Select(item => item.Name == table.Name ? table : item).ToArray())
                    {
                        var lost = (child.ForeignKeys ?? []).Where(foreign => foreign.Parent == table.Name && !SameColumns(foreign.ParentColumns, table.PrimaryKey) &&
                            !remaining.Any(index => index.Unique && index.Predicate is null && index.Keys.All(key => key.Expression is null) && SameColumns(foreign.ParentColumns, index.Keys.Select(key => key.Column)))).ToArray();
                        if (lost.Length == 0) continue;
                        if (!action.Cascade) throw new SqlException(SqlStates.DependentObjectsStillExist, "foreign key requires unique constraint");
                        var updated = child with { ForeignKeys = (child.ForeignKeys ?? []).Except(lost).ToArray() };
                        if (child.Name == table.Name) table = updated; else Put(pending, updated);
                    }
                    table = table with { Checks = (table.Checks ?? []).Where(check => check.Name != name).ToArray(),
                        ForeignKeys = (table.ForeignKeys ?? []).Where(foreign => foreign.Name != name).ToArray(),
                        Exclusions = (table.Exclusions ?? []).Where(exclusion => exclusion.Name != name).ToArray(),
                        Indexes = remaining }; break;
                }
                case "rename_constraint":
                {
                    var name = action.Column!; var renamed = action.Name!;
                    if (!ConstraintNames(table).Contains(name, StringComparer.Ordinal)) throw new SqlException(SqlStates.UndefinedObject, "constraint does not exist");
                    if (ConstraintNames(table).Contains(renamed, StringComparer.Ordinal)) throw new SqlException(SqlStates.DuplicateObject, "constraint already exists");
                    var index = (table.Indexes ?? []).FirstOrDefault(index => index.Name == name && (index.IsConstraint || (table.Exclusions ?? []).Any(exclusion => exclusion.Index == name)));
                    if (index is not null && renamed.StartsWith("jed_", StringComparison.Ordinal)) throw new SqlException(SqlStates.ReservedName, "reserved backing index name");
                    if (index is not null && RelationExists(pending.Catalog, renamed)) throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
                    table = table with { Checks = (table.Checks ?? []).Select(check => check.Name == name ? check with { Name = renamed } : check).ToArray(),
                        ForeignKeys = (table.ForeignKeys ?? []).Select(foreign => foreign.Name == name ? foreign with { Name = renamed } : foreign).ToArray(),
                        Indexes = (table.Indexes ?? []).Select(item => item.Name == name ? item with { Name = renamed } : item).ToArray(),
                        Exclusions = (table.Exclusions ?? []).Select(exclusion => exclusion.Name == name ? exclusion with { Name = renamed, Index = renamed } : exclusion).ToArray() };
                    if (index is not null)
                    {
                        pending.IndexTrees[renamed] = database.IndexTree(index, pending, pending.Before);
                        pending.IndexTrees.Remove(name);
                    }
                    break;
                }
                case "rename_column":
                {
                    if (columns.Any(column => column.Name == action.Name)) throw new SqlException(SqlStates.DuplicateColumn, "column already exists");
                    RenameViews(pending, table.Name, table.Name, 0, columns.Select(column => column.Name).ToArray(), ordinal, action.Name!);
                    var old = columns[ordinal].Name; columns[ordinal] = columns[ordinal] with { Name = action.Name! };
                    table = table with { Columns = columns,
                        Checks = (table.Checks ?? []).Select(check => check with { Expression = RenameExpression(check.Expression, old, action.Name!) }).ToArray(),
                        Indexes = (table.Indexes ?? []).Select(index => index with { Predicate = index.Predicate is null ? null : RenameExpression(index.Predicate, old, action.Name!),
                            Keys = index.Keys.Select(key => key.Expression is null ? key : key with { Expression = RenameExpression(key.Expression, old, action.Name!) }).ToArray() }).ToArray() }; break;
                }
                case "rename":
                {
                    foreach (var column in table.Columns) if (column.DefaultExpression is { } definition) _ = StoredExpression(definition);
                    if (RelationExistsAnywhere(pending.Catalog, action.Name!)) throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
                    RenameViews(pending, table.Name, action.Name!, 0);
                    var old = table.Name;
                    pending.Catalog = pending.Catalog with { Statistics = pending.Catalog.Statistics?.Select(stat => stat.Table == old ? stat with { Table = action.Name! } : stat).ToArray() };
                    table = table with { Name = action.Name!,
                        Checks = (table.Checks ?? []).Select(check => check with { Expression = RenameExpression(check.Expression, old, action.Name!) }).ToArray() };
                    pending.Catalog = pending.Catalog with { Tables = pending.Catalog.Tables.Where(item => item.Name != old).Select(item => item with {
                        ForeignKeys = (item.ForeignKeys ?? []).Select(foreign => foreign.Parent == old ? foreign with { Parent = action.Name! } : foreign).ToArray() }).ToArray(),
                        Sequences = (pending.Catalog.Sequences ?? []).Select(sequence => sequence.OwnerTable == old ? sequence with { OwnerTable = action.Name! } : sequence).ToArray() };
                    table = table with { ForeignKeys = (table.ForeignKeys ?? []).Select(foreign => foreign.Parent == old ? foreign with { Parent = action.Name! } : foreign).ToArray() };
                    if (pending.Trees.Remove(old, out var tree)) pending.Trees[action.Name!] = tree;
                    break;
                }
                default: throw Errors.Unsupported($"ALTER TABLE {action.Operation}");
            }
            changed = true;
        }
        if (!changed) return new([], []);
        foreach (var name in retypedColumns)
            if (table.Columns.Any(column => column.Name == name) && DependentViews(pending.Catalog, [table.Name], 0, name).Count != 0)
                throw Errors.Unsupported("view depends on altered column type");
        foreach (var ordinal in table.PrimaryKey) Keyable(table.Columns[ordinal].Type.Type);
        foreach (var index in table.Indexes ?? [])
            foreach (var key in index.Keys.Where(key => key.Expression is null))
                if (index.Kind == 1) GinIndex.ValidateType(table.Columns[key.Column].Type.Type); else if (index.Kind == 2) GistBounds.ValidateType(table.Columns[key.Column].Type.Type); else Keyable(table.Columns[key.Column].Type.Type);
        var finalTables = pending.Catalog.Tables.Where(item => item.Name != original.Name && item.Name != table.Name).Append(table).ToArray();
        foreach (var child in finalTables)
            foreach (var foreign in child.ForeignKeys ?? [])
            {
                var parent = finalTables.First(item => item.Name == foreign.Parent);
                for (var i = 0; i < foreign.Columns.Length; i++)
                    if (!ForeignKeyTypes(child.Columns[foreign.Columns[i]].Type.Type, parent.Columns[foreign.ParentColumns[i]].Type.Type)) throw Errors.Type("foreign key types must remain comparable");
            }
        if (rowSteps.Any(step => step.Operation == "type"))
        {
            table = table with { Checks = (table.Checks ?? []).Select(check => check with { HostDependencies = HostDependencies.Collect(
                [Resolve.Expr(Resolve.Bind(Resolve.BindTypes(StoredExpression(check.Expression), pending.Catalog, meter.Temporal), Columns(table)))], immutable: false) }).ToArray() };
            finalTables = finalTables.Select(candidate => candidate.Name == table.Name ? table : candidate).ToArray();
        }
        pending.Catalog = pending.Catalog with { Tables = finalTables };
        RevalidateViews(database, pending, table.Name, 0, meter, token);
        var source = database.Tree(original, pending, pending.Before); var result = source;
        var candidateRows = new List<SqlValue[]>();
        if (rewrite || validate)
        {
            if (rewrite)
            {
                long scanUnits = source.NodeCount;
                await foreach (var record in source.ScanAsync(token).ConfigureAwait(false)) scanUnits = Meter.Add(scanUnits, source.ValueReadUnits(record));
                meter.Charge(scanUnits);
            }
            else meter.Charge(await source.CountPagesAsync(token).ConfigureAwait(false));
            meter.Checkpoint();
            if (rewrite) result = database.Tree(table with { Root = 0 }, null, pending.Before);
            var checks = (table.Checks ?? []).Where(check => rowSteps.Any(step => step.Operation == "type") || !(original.Checks ?? []).Any(prior => prior.Name == check.Name && prior.Expression == check.Expression)).ToArray();
            long rewrittenRowid = -1;
            await foreach (var record in source.ScanAsync(token).ConfigureAwait(false))
            {
                meter.Charge(Costs.StorageRowRead);
                meter.Checkpoint();
                var row = original.DecodeRow(await source.MaterializeRecordAsync(record, token).ConfigureAwait(false));
                foreach (var step in rowSteps)
                {
                    if (step.Operation == "add") row = [.. row, DefaultValue(step.Added!, meter, pending.Catalog)];
                    else if (step.Operation == "drop") row = row.Where((_, ordinal) => ordinal != step.Ordinal).ToArray();
                    else
                    {
                        var value = step.Expression is { } expression ? Evaluate.Expr(expression, meter, row: row) : row[step.Ordinal];
                        row[step.Ordinal] = Resolve.ApplyModifier(Resolve.Cast(value, step.Type!.Type), step.Type);
                    }
                }
                Validate(table with { Checks = checks }, row, meter, pending.Catalog); candidateRows.Add(row);
                if (rewrite)
                {
                    var encoded = result.PrepareRecord(table.EncodeRow(row, original.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(record.Key) : checked(++rewrittenRowid)), meter);
                    if (await result.FindAsync(encoded.Key, token).ConfigureAwait(false) is not null) throw new SqlException(SqlStates.UniqueViolation, "rewritten primary key duplicates");
                    result = await result.UpsertAsync(encoded, token).ConfigureAwait(false);
                }
            }
            if (rewrite)
            {
                var cleared = await source.ClearAsync(token).ConfigureAwait(false); pending.RetiredPages.UnionWith(cleared.RetiredPages);
                pending.Trees.Remove(original.Name);
            }
        }
        if (rewrite && table.Columns.Select((column, i) => (Column: column, Source: values[i].Original)).Any(item => item.Source >= 0 && !original.Columns[item.Source].NotNull && item.Column.NotNull))
        {
            meter.Charge(await source.CountPagesAsync(token).ConfigureAwait(false));
            await foreach (var record in source.ScanAsync(token).ConfigureAwait(false)) { meter.Charge(Costs.StorageRowRead); meter.Checkpoint(); }
        }
        var rekeyed = table.PrimaryKey.Length != original.PrimaryKey.Length || table.PrimaryKey.Select((column, i) =>
            i >= original.PrimaryKey.Length || values[column].Original != original.PrimaryKey[i] || values[column].Cast is not null).Any(changed => changed);
        var rebuildExistingIndexes = rekeyed || values.Any(value => value.Cast is not null);
        var renamedIndexes = actions.Where(action => action.Operation == "rename_constraint").ToDictionary(action => action.Column!, action => action.Name!, StringComparer.Ordinal);
        Put(pending, table, rewrite ? result : null);
        if (rowSteps.Any(step => step.Operation is "type" or "drop")) ClearStatistics(pending, table.Name);
        foreach (var index in original.Indexes ?? [])
            if (rebuildExistingIndexes || removedIndexNames.Contains(index.Name) || !renamedIndexes.ContainsKey(index.Name) && !(table.Indexes ?? []).Any(current => current.Name == index.Name))
            {
                var cleared = await database.IndexTree(index, pending, pending.Before).ClearAsync(token).ConfigureAwait(false);
                pending.RetiredPages.UnionWith(cleared.RetiredPages); if (index.Gist is { } gist) pending.RetiredPages.UnionWith(gist.Pages); pending.IndexTrees.Remove(index.Name);
            }
        foreach (var index in table.Indexes ?? [])
            if (rebuildExistingIndexes || removedIndexNames.Contains(index.Name) || !renamedIndexes.ContainsValue(index.Name) && !(original.Indexes ?? []).Any(prior => prior.Name == index.Name))
                await BuildIndex(database, pending, table, index, meter, token, chargeScan: false).ConfigureAwait(false);
        if (candidateRows.Count != 0)
        {
            ValidateAddedExclusions(table, original, candidateRows.ToArray(), meter, pending.Catalog);
            foreach (var foreign in table.ForeignKeys ?? [])
            {
                if (foreign.Parent != table.Name || (original.ForeignKeys ?? []).Any(prior => prior.Name == foreign.Name)) continue;
                foreach (var row in candidateRows)
                {
                    if (foreign.Columns.Any(column => row[column].IsNull)) continue;
                    foreach (var parentRow in candidateRows)
                    {
                        meter.Charge(Costs.ConstraintCheck); meter.Checkpoint();
                        if (SameForeignKey(row, foreign.Columns, parentRow, foreign.ParentColumns)) break;
                    }
                }
            }
            await ValidateForeignKeys(database, pending, table, candidateRows, token).ConfigureAwait(false);
        }
        return new([], []);
    }
}
