namespace Jed;

internal sealed record StatementResult(Column[] Columns, Row[] Rows, long Affected = 0);

internal static partial class StatementExecution
{
    internal static CatalogTable Table(Catalog catalog, string name) => catalog.Tables.FirstOrDefault(t => t.Name == name)
        ?? throw new SqlException(SqlStates.UndefinedTable, $"table does not exist: {name}");
    internal static BoundColumn[] Columns(CatalogTable table, string? alias = null) =>
        table.Columns.Select(c => new BoundColumn(c.Name, c.Type.Type, alias ?? table.Name)).ToArray();
    private static int ColumnIndex(CatalogTable table, string name)
    {
        var index = Array.FindIndex(table.Columns, c => c.Name == name);
        if (index < 0) throw new SqlException(SqlStates.UndefinedColumn, $"column does not exist: {name}");
        return index;
    }
    private static void Put(PendingDatabase pending, CatalogTable table, StorageTree? tree = null)
    {
        pending.Catalog = pending.Catalog with { Tables = pending.Catalog.Tables.Where(t => t.Name != table.Name).Append(table).ToArray() };
        if (tree is not null) pending.Trees[table.Name] = tree;
        pending.Changed = true;
    }
    internal static async ValueTask<StatementResult> Execute(Database database, PendingDatabase pending, StatementSyntax statement,
        Meter meter, CancellationToken cancellationToken)
    {
        var target = statement switch { InsertSyntax insert => insert.Table, UpdateSyntax update => update.Table, DeleteSyntax delete => delete.Table, _ => null };
        if (target is not null && (pending.Catalog.Views ?? []).FirstOrDefault(view => view.Name == target) is { } view)
            return await WriteView(database, pending, statement, view, meter, cancellationToken).ConfigureAwait(false);
        switch (statement)
        {
            case CreateViewSyntax create: return CreateView(database, pending, create, meter, cancellationToken);
            case DropViewSyntax drop: return DropView(pending, drop);
            case AlterViewSyntax alter: return AlterView(database, pending, alter, meter, cancellationToken);
            case CreateSequenceSyntax create: return CreateSequence(pending, create);
            case AlterSequenceSyntax alter: return AlterSequence(pending, alter, meter);
            case DropSequenceSyntax drop: return DropSequence(pending, drop, meter);
            case CreateTypeSyntax create: return CreateType(pending, create);
            case DropTypeSyntax drop: return DropType(pending, drop);
            case CreateTableAsSyntax create: return await CreateTableAs(database, pending, create, meter, cancellationToken).ConfigureAwait(false);
            case CreateTableSyntax create: return CreateTable(pending, create, meter);
            case CreateIndexSyntax index: return await CreateIndex(database, pending, index, meter, cancellationToken).ConfigureAwait(false);
            case DropIndexSyntax index: return await DropIndex(database, pending, index, cancellationToken).ConfigureAwait(false);
            case AlterBatchSyntax batch: return await AlterFrame(database, pending, batch.Actions, meter, cancellationToken).ConfigureAwait(false);
            case AlterTableSyntax alter: return await AlterTable(database, pending, alter, meter, cancellationToken).ConfigureAwait(false);
            case DropTableSyntax drop:
            {
                var table = pending.Catalog.Tables.FirstOrDefault(t => t.Name == drop.Name);
                if (table is null) { if (RelationExists(pending.Catalog, drop.Name)) throw new SqlException(SqlStates.WrongObjectType, "relation is not a table"); if (!drop.IfExists) throw new SqlException(SqlStates.UndefinedTable, "table does not exist"); return new([], []); }
                DropDependentViews(pending, table.Name, 0, drop.Cascade);
                foreach (var child in pending.Catalog.Tables.Where(child => child.Name != table.Name).ToArray())
                {
                    if (!(child.ForeignKeys ?? []).Any(foreign => foreign.Parent == table.Name)) continue;
                    if (!drop.Cascade) throw new SqlException(SqlStates.DependentObjectsStillExist, "foreign key depends on table");
                    Put(pending, child with { ForeignKeys = child.ForeignKeys!.Where(foreign => foreign.Parent != table.Name).ToArray() });
                }
                foreach (var index in table.Indexes ?? [])
                {
                    var oldIndex = await database.IndexTree(index, pending, pending.Before).ClearAsync(cancellationToken).ConfigureAwait(false);
                    pending.RetiredPages.UnionWith(oldIndex.RetiredPages); pending.IndexTrees.Remove(index.Name);
                }
                var tree = database.Tree(table, pending, pending.Before);
                var cleared = await tree.ClearAsync(cancellationToken).ConfigureAwait(false);
                pending.RetiredPages.UnionWith(cleared.RetiredPages);
                pending.Trees.Remove(table.Name);
                foreach (var sequence in (pending.Catalog.Sequences ?? []).Where(sequence => sequence.OwnerTable == table.Name)) meter.Sequences.State.Current.Remove(sequence.Name);
                pending.Catalog = pending.Catalog with { Tables = pending.Catalog.Tables.Where(t => t.Name != table.Name).ToArray(),
                    Sequences = (pending.Catalog.Sequences ?? []).Where(sequence => sequence.OwnerTable != table.Name).ToArray() };
                pending.Changed = true; return new([], []);
            }
            case InsertSyntax insert:
            {
                var table = Table(pending.Catalog, insert.Table); var tree = database.Tree(table, pending, pending.Before);
                var slots = insert.Columns is null ? Enumerable.Range(0, table.Columns.Length).ToArray() : insert.Columns.Select(n => ColumnIndex(table, n)).ToArray();
                if (slots.Distinct().Count() != slots.Length) throw new SqlException(SqlStates.DuplicateColumn, "column specified more than once");
                var readState = pending.Fork();
                var inputBinder = new SubqueryBinder([], database, pending.Before, readState, pending.Catalog, meter, cancellationToken);
                var input = insert.Values.Select(row => row.Select(value => value.Op == "default" ? value : inputBinder.Bind(value)).ToArray()).ToArray();
                if (insert.Source is { } unboundSource)
                {
                    if (insert.Overriding is null && slots.Any(slot => table.Columns[slot].IdentityAlways)) throw new SqlException(SqlStates.GeneratedAlways, "explicit SELECT source for GENERATED ALWAYS identity");
                    var sourceSyntax = Resolve.BindTypes(unboundSource, pending.Catalog, meter.Temporal);
                    sourceSyntax = AdaptInsertSource(sourceSyntax, slots.Select(slot => table.Columns[slot].Type).ToArray(), pending.Catalog, meter);
                    var source = RelationExecution.Query(sourceSyntax, database, pending.Before, pending, pending.Catalog, meter, cancellationToken);
                    if (source.Columns.Length != slots.Length) throw Errors.Syntax("INSERT source width differs from target");
                    for (var i = 0; i < slots.Length; i++) Assignable(source.Columns[i].Type, table.Columns[slots[i]].Type.Type);
                    var rows = new List<Expression[]>();
                    if (source.Initialize is not null) await source.Initialize().ConfigureAwait(false);
                    await foreach (var row in source.Rows.WithCancellation(cancellationToken).ConfigureAwait(false))
                        rows.Add(row.Select(value => new Expression("bound", "", [], Binding: new Resolved("constant", value.Type, value, []))).ToArray());
                    input = rows.ToArray();
                }
                if (insert.Conflict is not null) return await Upsert(database, pending, table, tree, insert, slots, input, meter, cancellationToken, inputBinder).ConfigureAwait(false);
                var returnTable = insert.Alias is { } alias ? table with { Name = alias } : table;
                var returnBinder = new SubqueryBinder(ReturningColumns(returnTable), database, pending.Before, readState, pending.Catalog, meter, cancellationToken);
                var returning = Returning(returnTable, insert.Returning, pending.Catalog, meter, returnBinder);
                foreach (var expressions in input)
                {
                    if (expressions.Length != 0 && expressions.Length != slots.Length) throw Errors.Syntax("INSERT value count differs from column count");
                    for (var i = 0; i < expressions.Length; i++) if (expressions[i].Op != "default") Assignable(Resolve.Expr(expressions[i], table.Columns[slots[i]].Type.Type).Type, table.Columns[slots[i]].Type.Type);
                }
                await inputBinder.Initialize().ConfigureAwait(false); await returnBinder.Initialize().ConfigureAwait(false);
                var results = new List<Row>(); var inserted = new List<SqlValue[]>();
                long nextRow = -1;
                if (table.PrimaryKey.Length == 0)
                    await foreach (var record in tree.ScanAsync(cancellationToken).ConfigureAwait(false)) nextRow = Math.Max(nextRow, KeyEncoding.ReadInteger(record.Key));
                foreach (var expressions in input)
                {
                    if (expressions.Length != 0 && expressions.Length != slots.Length) throw Errors.Syntax("INSERT value count differs from column count");
                    var values = new SqlValue[table.Columns.Length];
                    for (var column = 0; column < values.Length; column++)
                    {
                        var supplied = Array.IndexOf(slots, column);
                        var declaration = table.Columns[column];
                        var useDefault = supplied < 0 || supplied >= expressions.Length || expressions[supplied].Op == "default" || declaration.Identity && insert.Overriding == "user";
                        if (!useDefault && declaration.IdentityAlways && insert.Overriding != "system") throw new SqlException(SqlStates.GeneratedAlways, "explicit value for GENERATED ALWAYS identity");
                        values[column] = useDefault ? DefaultValue(declaration, meter, pending.Catalog) : await AssignAsync(expressions[supplied], declaration.Type, [], [], meter, pending.Catalog).ConfigureAwait(false);
                    }
                    Validate(table, values, meter, pending.Catalog); ValidateViewChecks(insert.ViewChecks, values, meter); inserted.Add(values);
                    var encoded = tree.PrepareRecord(table.EncodeRow(values, checked(++nextRow)), meter);
                    if (table.PrimaryKey.Length != 0 && await tree.FindAsync(encoded.Key, cancellationToken).ConfigureAwait(false) is not null)
                        throw new SqlException(SqlStates.UniqueViolation, "duplicate primary key");
                    tree = await tree.UpsertAsync(encoded, cancellationToken).ConfigureAwait(false);
                    await AddIndexes(database, pending, table, encoded, values, meter, cancellationToken).ConfigureAwait(false);
                }
                meter.Checkpoint(); if (input.Length != 0) Put(pending, table with { RowCount = checked(table.RowCount + input.LongLength) }, tree);
                await ValidateForeignKeys(database, pending, table, inserted, cancellationToken).ConfigureAwait(false);
                if (returning.Length != 0) foreach (var values in inserted) results.Add(await QueryExecution.ProjectAsync(returning, ReturningRow(table, null, values), meter).ConfigureAwait(false));
                return new(returning.Select(p => new Column(p.Name, p.Expression.Type)).ToArray(), results.ToArray(), input.LongLength);
            }
            case UpdateSyntax update: return await Mutate(database, pending, update.Table, update.Filter, update.Assignments, update.Returning, meter, cancellationToken, update.ViewChecks).ConfigureAwait(false);
            case DeleteSyntax delete: return await Mutate(database, pending, delete.Table, delete.Filter, null, delete.Returning, meter, cancellationToken).ConfigureAwait(false);
            default: throw Errors.Unsupported("statement execution");
        }
    }
    internal static BoundColumn[] ReturningColumns(CatalogTable table) => Columns(table)
        .Concat(table.Columns.Select(column => new BoundColumn(column.Name, column.Type.Type, table.Name == "old" ? null : "old", Hidden: true)))
        .Concat(table.Columns.Select(column => new BoundColumn(column.Name, column.Type.Type, table.Name == "new" ? null : "new", Hidden: true))).ToArray();
    private static SqlValue[] ReturningRow(CatalogTable table, SqlValue[]? old, SqlValue[]? current) =>
        (current ?? old!).Concat(old ?? table.Columns.Select(column => SqlValue.Null(column.Type.Type))).Concat(current ?? table.Columns.Select(column => SqlValue.Null(column.Type.Type))).ToArray();
    private static Projection[] Returning(CatalogTable table, SelectItem[] items, Catalog catalog, Meter meter, SubqueryBinder? binder = null) => items.Length == 0 ? [] :
        Resolve.Select(Resolve.BindTypes(new SelectSyntax(binder is null ? items : items.Select(item => item.Expression.Op == "star" ? item : item with { Expression = binder.Bind(item.Expression) }).ToArray(), null, null, 0), catalog, meter.Temporal), ReturningColumns(table)).Projections;

    private static async ValueTask<StatementResult> Mutate(Database database, PendingDatabase pending, string name, Expression? where,
        AssignmentSyntax[]? assignments, SelectItem[] items, Meter meter, CancellationToken cancellationToken, Expression[]? viewChecks = null)
    {
        var table = Table(pending.Catalog, name); var tree = database.Tree(table, pending, pending.Before);
        var columns = Columns(table);
        var binder = new SubqueryBinder(columns, database, pending.Before, pending.Fork(), pending.Catalog, meter, cancellationToken);
        var filter = where is null ? null : Resolve.Expr(binder.Bind(where));
        if (filter is not null && filter.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("WHERE requires boolean");
        var targets = assignments?.Select(a => (Slot: ColumnIndex(table, a.Column), Value: a.Value.Op == "default" ? a.Value : binder.Bind(a.Value))).ToArray();
        if (targets is not null && targets.Select(t => t.Slot).Distinct().Count() != targets.Length) throw new SqlException(SqlStates.DuplicateColumn, "column assigned twice");
        if (targets is not null)
            foreach (var target in targets)
                if (target.Value.Op != "default" && table.Columns[target.Slot].IdentityAlways) throw new SqlException(SqlStates.GeneratedAlways, "explicit update of GENERATED ALWAYS identity");
                else if (target.Value.Op != "default") Assignable(Resolve.Expr(Resolve.Bind(target.Value, columns), table.Columns[target.Slot].Type.Type).Type, table.Columns[target.Slot].Type.Type);
        var returnBinder = new SubqueryBinder(ReturningColumns(table), database, pending.Before, pending.Fork(), pending.Catalog, meter, cancellationToken);
        var returning = Returning(table, items, pending.Catalog, meter, returnBinder);
        await returnBinder.Initialize().ConfigureAwait(false);
        await binder.Initialize().ConfigureAwait(false); var results = new List<Row>();
        var changes = new List<(StorageRecord Old, SqlValue[] Values)>();
        var storedRows = new Dictionary<string, StorageRecord>(StringComparer.Ordinal);
        var unchanged = Enumerable.Range(0, table.Columns.Length).Where(column => !table.PrimaryKey.Contains(column) && !(targets ?? []).Any(target => target.Slot == column)).ToHashSet();
        var scan = TableAccess.Bounds(filter, table);
        if (!scan.Empty) meter.Charge(await tree.CountRangePagesAsync(scan.Lower, scan.LowerInclusive, scan.Upper, scan.UpperInclusive, cancellationToken).ConfigureAwait(false)); meter.Checkpoint();
        await foreach (var record in tree.ScanRangeAsync(scan.Empty ? [1] : scan.Lower, scan.LowerInclusive, scan.Empty ? [0] : scan.Upper, scan.UpperInclusive, cancellationToken: cancellationToken).ConfigureAwait(false))
        {
            meter.Charge(Costs.StorageRowRead); meter.Checkpoint();
            var materialized = await tree.MaterializeRecordAsync(record, cancellationToken).ConfigureAwait(false);
            var original = table.DecodeRow(materialized);
            if (!await QueryExecution.KeepsAsync(filter, original, meter).ConfigureAwait(false)) continue;
            var values = original.ToArray();
            if (targets is not null)
                foreach (var target in targets.OrderBy(target => target.Slot))
                    values[target.Slot] = target.Value.Op == "default" ? DefaultValue(table.Columns[target.Slot], meter, pending.Catalog) :
                        await AssignAsync(target.Value, table.Columns[target.Slot].Type, columns, original, meter, pending.Catalog).ConfigureAwait(false);
            if (targets is not null) { Validate(table, values, meter, pending.Catalog); ValidateViewChecks(viewChecks, values, meter); }
            storedRows.Add(Convert.ToHexString(record.Key), record);
            changes.Add((materialized, values));
        }
        foreach (var change in changes)
        {
            tree = await tree.DeleteAsync(change.Old.Key, cancellationToken).ConfigureAwait(false);
            await RemoveIndexes(database, pending, table, change.Old, meter, cancellationToken).ConfigureAwait(false);
        }
        foreach (var change in changes)
        {
            if (targets is not null)
            {
                var encoded = tree.KeepUnchangedChains(tree.PrepareRecord(table.EncodeRow(change.Values, table.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(change.Old.Key) : 0), meter), storedRows[Convert.ToHexString(change.Old.Key)], unchanged);
                if (await tree.FindAsync(encoded.Key, cancellationToken).ConfigureAwait(false) is not null) throw new SqlException(SqlStates.UniqueViolation, "duplicate primary key");
                tree = await tree.UpsertAsync(encoded, cancellationToken).ConfigureAwait(false);
                await AddIndexes(database, pending, table, encoded, change.Values, meter, cancellationToken).ConfigureAwait(false);
            }
        }
        meter.Checkpoint();
        if (changes.Count != 0) Put(pending, table with { RowCount = assignments is null ? table.RowCount - changes.Count : table.RowCount }, tree);
        if (changes.Count != 0)
        {
            await ApplyReferentialActions(database, pending, table, changes, assignments is null, meter, cancellationToken).ConfigureAwait(false);
            if (assignments is not null) await ValidateForeignKeys(database, pending, table, changes.Select(change => change.Values).ToArray(), cancellationToken).ConfigureAwait(false);
        }
        if (returning.Length != 0) foreach (var change in changes) results.Add(await QueryExecution.ProjectAsync(returning, ReturningRow(table, table.DecodeRow(change.Old), assignments is null ? null : change.Values), meter).ConfigureAwait(false));
        return new(returning.Select(p => new Column(p.Name, p.Expression.Type)).ToArray(), results.ToArray(), changes.Count);
    }
    private static async ValueTask<SqlValue> AssignAsync(Expression expression, TypeSpec type, BoundColumn[] columns, SqlValue[] row, Meter meter, Catalog catalog)
    {
        var resolved = Resolve.Expr(Resolve.Bind(Resolve.BindTypes(expression, catalog, meter.Temporal), columns), type.Type);
        Assignable(resolved.Type, type.Type);
        var value = Resolve.Cast(await Evaluate.ExprAsync(resolved, meter, row: row).ConfigureAwait(false), type.Type);
        if (!value.IsNull && type.Type == SqlType.Text && type.Length is { } length && value.AsText().EnumerateRunes().Count() > length)
            throw new SqlException(SqlStates.StringDataRightTruncation, "value exceeds varchar length");
        return Resolve.ApplyModifier(value, type);
    }
    private static SqlValue Assign(Expression expression, TypeSpec type, BoundColumn[] columns, SqlValue[] row, Meter meter, Catalog catalog)
    {
        expression = Resolve.BindTypes(expression, catalog, meter.Temporal);
        var resolved = Resolve.Expr(Resolve.Bind(expression, columns), type.Type);
        Assignable(resolved.Type, type.Type);
        var value = Resolve.Cast(Evaluate.Expr(resolved, meter, row: row), type.Type);
        if (!value.IsNull && type.Type == SqlType.Text && type.Length is { } length && value.AsText().EnumerateRunes().Count() > length)
            throw new SqlException(SqlStates.StringDataRightTruncation, "value exceeds varchar length");
        return Resolve.ApplyModifier(value, type);
    }
    private static void Validate(CatalogTable table, SqlValue[] row, Meter meter, Catalog catalog)
    {
        for (var i = 0; i < row.Length; i++)
            if (table.Columns[i].NotNull && row[i].IsNull) throw new SqlException(SqlStates.NotNullViolation, $"NULL column: {table.Columns[i].Name}");
        foreach (var check in table.Checks ?? [])
        {
            var expression = Resolve.Expr(Resolve.Bind(Resolve.BindTypes(StoredExpression(check.Expression), catalog, meter.Temporal), Columns(table)));
            var result = Evaluate.Expr(expression, meter, row: row);
            if (!result.IsNull && !result.AsBoolean()) throw new SqlException(SqlStates.CheckViolation, $"check failed: {check.Name}");
        }
    }
}
