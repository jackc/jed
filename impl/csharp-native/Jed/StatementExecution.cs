namespace Jed;

internal sealed record StatementResult(Column[] Columns, Row[] Rows, long? Affected = null);

internal static partial class StatementExecution
{
    internal static CatalogTable ResolveTable(Catalog catalog, string name) => catalog.Scope?.Table(name) ?? Table(catalog, name);
    internal static CatalogTable Table(Catalog catalog, string name) => catalog.Tables.FirstOrDefault(t => t.Name == name)
        ?? throw new SqlException(SqlStates.UndefinedTable, $"table does not exist: {name}");
    internal static BoundColumn[] Columns(CatalogTable table, string? alias = null)
    {
        var source = new object();
        return table.Columns.Select((c, i) => new BoundColumn(c.Name, c.Type.Type, alias ?? table.Name,
            BaseSource: source, PrimaryKey: table.PrimaryKey.Contains(i), RowName: c.Name, NotNull: c.NotNull || table.PrimaryKey.Contains(i),
            Collation: CollationRules.Collatable(c.Type.Type) ? new(Name: c.Collation ?? "C", Table: c.CollationData) : null)).ToArray();
    }
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
        Meter meter, CancellationToken cancellationToken, StatementContext? context = null)
    {
        ValidateCatalogTarget(statement);
        var result = pending.Catalog.Scope is not null
            ? await ExecuteDomain(database, pending, statement, meter, cancellationToken, context).ConfigureAwait(false)
            : await ExecuteLocal(database, pending, statement, meter, cancellationToken, context).ConfigureAwait(false);
        meter.Memory.HoldPending(pending.Catalog.Scope is { } domains ? Session.DomainPendingBytes(domains) : pending.PendingBytes);
        return result;
    }
    private static void HoldPendingMemory(PendingDatabase pending, Meter meter) =>
        meter.Memory.HoldPending(pending.Catalog.Scope is { } domains ? Session.DomainPendingBytes(domains) : pending.PendingBytes);
    internal static void ValidateCatalogTarget(StatementSyntax statement)
    {
        if (statement is DropTablesSyntax drops) { foreach (var name in drops.Names) ValidateCatalogTarget(new DropTableSyntax(name, drops.IfExists, drops.Cascade)); return; }
        if (statement is WithStatementSyntax wrapped)
        {
            ValidateCatalogTarget(wrapped.Body); foreach (var definition in wrapped.Definitions) if (definition.Mutation is { } mutation) ValidateCatalogTarget(mutation);
            return;
        }
        if (statement is SelectSyntax query)
        {
            foreach (var definition in query.With ?? []) if (definition.Mutation is { } mutation) ValidateCatalogTarget(mutation);
            return;
        }
        if (statement is CreateTableSyntax or CreateTableAsSyntax or CreateViewSyntax or CreateSequenceSyntax or CreateTypeSyntax) return;
        if (DomainExecution.Target(statement) is { } target && CatalogRelations.IsCatalogName(target))
            throw new SqlException(SqlStates.WrongObjectType, "catalog relation is read-only");
    }
    private static async ValueTask<StatementResult> ExecuteLocal(Database database, PendingDatabase pending, StatementSyntax statement,
        Meter meter, CancellationToken cancellationToken, StatementContext? context = null)
    {
        if (pending.Catalog.Scope is null && (statement is CreateTableSyntax { Temporary: true } || statement is CreateTableAsSyntax { Temporary: true }))
            throw Errors.Unsupported("temporary tables require a session domain");
        var target = statement switch { InsertSyntax insert => insert.Table, UpdateSyntax update => update.Table, DeleteSyntax delete => delete.Table, MergeSyntax merge => merge.Table, _ => null };
        if (target is not null && (pending.Catalog.Views ?? []).FirstOrDefault(view => view.Name == target) is { } view)
            return await WriteView(database, pending, statement, view, meter, cancellationToken, context).ConfigureAwait(false);
        if (target is not null)
        {
            WritableCollations(Table(pending.Catalog, target), pending.Catalog);
            WritableIndexDependencies(Table(pending.Catalog, target), pending.Catalog);
        }
        switch (statement)
        {
            case WithStatementSyntax wrapped:
                return await WritableCteExecution.Prepare(database, pending, wrapped.Definitions, wrapped.Recursive, wrapped.Body, meter, cancellationToken).Run().ConfigureAwait(false);
            case ReindexSyntax reindex: return await Reindex(database, pending, reindex, meter, cancellationToken).ConfigureAwait(false);
            case AnalyzeSyntax analyze: return await Analyze(database, pending, analyze, meter, cancellationToken).ConfigureAwait(false);
            case CreateViewSyntax create: return CreateView(database, pending, create, meter, cancellationToken);
            case DropViewSyntax drop: return DropView(pending, drop);
            case AlterViewSyntax alter: return AlterView(database, pending, alter, meter, cancellationToken);
            case CreateSequenceSyntax create: return CreateSequence(pending, create);
            case AlterSequenceSyntax alter: return AlterSequence(pending, alter, meter);
            case DropSequenceSyntax drop: return DropSequence(pending, drop, meter);
            case CreateTypeSyntax create: return CreateType(pending, create);
            case DropTypeSyntax drop: return DropType(pending, drop);
            case CreateTableAsSyntax create: return await CreateTableAs(database, pending, create, meter, cancellationToken, context?.PreparedQuery).ConfigureAwait(false);
            case CreateTableSyntax create: return CreateTable(pending, create, meter);
            case CreateIndexSyntax index: return await CreateIndex(database, pending, index, meter, cancellationToken).ConfigureAwait(false);
            case DropIndexSyntax index: return await DropIndex(database, pending, index, cancellationToken).ConfigureAwait(false);
            case AlterBatchSyntax batch: return await AlterFrame(database, pending, batch.Actions, meter, cancellationToken).ConfigureAwait(false);
            case AlterTableSyntax alter: return await AlterTable(database, pending, alter, meter, cancellationToken).ConfigureAwait(false);
            case DropTablesSyntax drops:
            {
                var names = drops.Names.ToHashSet(StringComparer.Ordinal);
                foreach (var name in names)
                {
                    if (!pending.Catalog.Tables.Any(table => table.Name == name) && RelationExists(pending.Catalog, name))
                        throw new SqlException(SqlStates.WrongObjectType, "relation is not a table");
                    if (!pending.Catalog.Tables.Any(table => table.Name == name) && !drops.IfExists)
                        throw new SqlException(SqlStates.UndefinedTable, "table does not exist");
                }
                pending.Catalog = pending.Catalog with { Tables = pending.Catalog.Tables.Select(table => names.Contains(table.Name)
                    ? table with { ForeignKeys = table.ForeignKeys?.Where(key => !names.Contains(key.Parent)).ToArray() } : table).ToArray() };
                foreach (var name in names.Order(Utf8NameComparer.Instance))
                    await ExecuteLocal(database, pending, new DropTableSyntax(name, drops.IfExists, drops.Cascade), meter, cancellationToken, context).ConfigureAwait(false);
                return new([], []);
            }
            case DropTableSyntax drop:
            {
                var table = pending.Catalog.Tables.FirstOrDefault(t => t.Name == drop.Name);
                if (table is null) { if (RelationExists(pending.Catalog, drop.Name)) throw new SqlException(SqlStates.WrongObjectType, "relation is not a table"); if (!drop.IfExists) throw new SqlException(SqlStates.UndefinedTable, "table does not exist"); return new([], []); }
                DropDependentViews(pending, table.Name, 0, drop.Cascade);
                foreach (var owned in (pending.Catalog.Sequences ?? []).Where(sequence => sequence.OwnerTable == table.Name))
                    DropDependentViews(pending, owned.Name, 2, drop.Cascade);
                foreach (var child in pending.Catalog.Tables.Where(child => child.Name != table.Name).ToArray())
                {
                    if (!(child.ForeignKeys ?? []).Any(foreign => foreign.Parent == table.Name)) continue;
                    if (!drop.Cascade) throw new SqlException(SqlStates.DependentObjectsStillExist, "foreign key depends on table");
                    Put(pending, child with { ForeignKeys = child.ForeignKeys!.Where(foreign => foreign.Parent != table.Name).ToArray() });
                }
                foreach (var index in table.Indexes ?? [])
                {
                    var oldIndex = await database.IndexTree(index, pending, pending.Before).ClearAsync(cancellationToken).ConfigureAwait(false);
                    pending.RetiredPages.UnionWith(oldIndex.RetiredPages); if (index.Gist is { } gist) pending.RetiredPages.UnionWith(gist.Pages); pending.IndexTrees.Remove(index.Name);
                }
                var tree = database.Tree(table, pending, pending.Before);
                var cleared = await tree.ClearAsync(cancellationToken).ConfigureAwait(false);
                pending.RetiredPages.UnionWith(cleared.RetiredPages);
                pending.Trees.Remove(table.Name);
                ClearStatistics(pending, table.Name);
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
                var readState = context?.Read ?? pending.Fork();
                var inputBinder = new SubqueryBinder([], database, pending.Before, readState, readState.Catalog, meter, cancellationToken, context?.Scope);
                var input = insert.Values.Select(row => row.Select(value => value.Op == "default" ? value : inputBinder.Bind(value)).ToArray()).ToArray();
                if (insert.Source is not null)
                {
                    if (insert.Overriding is null && slots.Any(slot => table.Columns[slot].IdentityAlways)) throw new SqlException(SqlStates.GeneratedAlways, "explicit SELECT source for GENERATED ALWAYS identity");
                    var source = context?.PreparedQuery ?? RelationExecution.Query(InsertQuery(insert, table, pending.Catalog, meter), database, pending.Before, readState, readState.Catalog, meter, cancellationToken, scope: context?.Scope);
                    if (source.Columns.Length != slots.Length) throw Errors.Syntax("INSERT source width differs from target");
                    for (var i = 0; i < slots.Length; i++) Assignable(source.Columns[i].Type, table.Columns[slots[i]].Type.Type);
                    var rows = new List<Expression[]>();
                    if (source.Initialize is not null) await source.Initialize().ConfigureAwait(false);
                    await foreach (var row in source.Rows.WithCancellation(cancellationToken).ConfigureAwait(false))
                    {
                        if (!source.MemoryOutput) meter.Memory.ReserveRow(row);
                        rows.Add(row.Select(value => new Expression("bound", "", [], Binding: new Resolved("constant", value.Type, value, []))).ToArray());
                    }
                    input = rows.ToArray();
                }
                if (insert.Conflict is not null) return await Upsert(database, pending, table, tree, insert, slots, input, meter, cancellationToken, inputBinder, context).ConfigureAwait(false);
                var returnTable = insert.Alias is { } alias ? table with { Name = alias } : table;
                var returnBinder = new SubqueryBinder(insert.ViewBinding?.Returning ?? ReturningColumns(returnTable, insert.ReturningAliases), database, pending.Before, readState, readState.Catalog, meter, cancellationToken, context?.Scope);
                var returning = Returning(returnTable, insert.Returning, pending.Catalog, meter, returnBinder, insert.ReturningAliases);
                foreach (var expressions in input)
                {
                    if (expressions.Length != 0 && expressions.Length != slots.Length) throw Errors.Syntax("INSERT value count differs from column count");
                    for (var i = 0; i < expressions.Length; i++) if (expressions[i].Op != "default") Assignable(Resolve.Expr(AssignmentLiteral(expressions[i], table.Columns[slots[i]].Type), table.Columns[slots[i]].Type.Type).Type, table.Columns[slots[i]].Type.Type);
                }
                foreach (var check in insert.ViewChecks ?? []) _ = returnBinder.Bind(check);
                await inputBinder.Initialize().ConfigureAwait(false);
                var results = new List<Row>(); var inserted = new List<SqlValue[]>(); var exclusionRows = new List<(StorageRecord Record, SqlValue[] Values)>();
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
                        values[column] = useDefault ? InsertDefault(insert, column, declaration, meter, pending.Catalog) : await AssignAsync(expressions[supplied], declaration.Type, [], [], meter, pending.Catalog).ConfigureAwait(false);
                    }
                    Validate(table, values, meter, pending.Catalog); inserted.Add(values);
                    var encoded = tree.PrepareRecord(table.EncodeRow(values, checked(++nextRow)), meter);
                    exclusionRows.Add((encoded, values));
                    await ValidatePinnedKeys(database, context, table, [(encoded, values)], new HashSet<string>(StringComparer.Ordinal), meter, cancellationToken).ConfigureAwait(false);
                    if (table.PrimaryKey.Length != 0 && await tree.FindAsync(encoded.Key, cancellationToken).ConfigureAwait(false) is not null)
                        throw new SqlException(SqlStates.UniqueViolation, "duplicate primary key");
                    tree = await tree.UpsertAsync(encoded, cancellationToken).ConfigureAwait(false);
                    await AddIndexes(database, pending, table, encoded, values, meter, cancellationToken).ConfigureAwait(false);
                    await ValidateViewChecks(insert.ViewChecks, values, meter).ConfigureAwait(false);
                }
                meter.Checkpoint(); if (input.Length != 0) { Put(pending, table with { RowCount = checked(table.RowCount + input.LongLength) }, tree); StaleStatistics(pending, table.Name); }
                HoldPendingMemory(pending, meter);
                await ValidateExclusions(database, pending, table, exclusionRows, meter, cancellationToken, context).ConfigureAwait(false);
                await ForeignCheck(context, () => ValidateForeignKeys(database, pending, table, inserted, cancellationToken)).ConfigureAwait(false);
                await returnBinder.Initialize().ConfigureAwait(false);
                if (returning.Length != 0) foreach (var values in inserted) results.Add((await QueryExecution.ProjectAsync(returning, ReturningRow(table, null, values), meter).ConfigureAwait(false)).RetainMemory(meter));
                return new(returning.Select(p => new Column(p.Name, p.Expression.Type)).ToArray(), results.ToArray(), input.LongLength);
            }
            case MergeSyntax merge: return await PrepareMerge(database, pending, merge, meter, cancellationToken, context).Run().ConfigureAwait(false);
            case UpdateSyntax update: return await Mutate(database, pending, update.Table, update.Filter, update.Assignments, update.Returning, meter, cancellationToken, update.ViewChecks, update.ViewBinding, context, update.ReturningAliases).ConfigureAwait(false);
            case DeleteSyntax delete: return await Mutate(database, pending, delete.Table, delete.Filter, null, delete.Returning, meter, cancellationToken, viewBinding: delete.ViewBinding, context: context, aliases: delete.ReturningAliases).ConfigureAwait(false);
            default: throw Errors.Unsupported("statement execution");
        }
    }
    private static SqlValue InsertDefault(InsertSyntax insert, int slot, CatalogColumn column, Meter meter, Catalog catalog) =>
        !(column.Identity && insert.Overriding == "user") && insert.ViewDefaults is { } defaults && defaults.TryGetValue(slot, out var expression)
            ? Assign(expression, column.Type, [], [], meter, catalog) : DefaultValue(column, meter, catalog);

    internal static BoundColumn[] ReturningColumns(CatalogTable table, ReturningAliases? aliases = null)
    {
        if (aliases is not null && (aliases.Old == table.Name || aliases.New == table.Name || aliases.Old is not null && aliases.Old == aliases.New))
            throw new SqlException(SqlStates.DuplicateAlias, "RETURNING aliases collide");
        var old = aliases?.Old ?? (table.Name == "old" || aliases?.New == "old" ? null : "old");
        var current = aliases?.New ?? (table.Name == "new" || aliases?.Old == "new" ? null : "new");
        return Columns(table)
            .Concat(table.Columns.Select(column => new BoundColumn(column.Name, column.Type.Type, old, Hidden: true, Collation: CollationRules.Collatable(column.Type.Type) ? new(Name: column.Collation ?? "C", Table: column.CollationData) : null, WholeRow: false)))
            .Concat(table.Columns.Select(column => new BoundColumn(column.Name, column.Type.Type, current, Hidden: true, Collation: CollationRules.Collatable(column.Type.Type) ? new(Name: column.Collation ?? "C", Table: column.CollationData) : null, WholeRow: false))).ToArray();
    }
    internal static void ValidateReturning(SelectItem[] items)
    {
        void Check(Expression expression)
        {
            if (expression.Window is not null) throw new SqlException(SqlStates.WindowingError, "window function in RETURNING");
            if (AggregateResolve.IsAggregate(expression) || expression.Op == "call" && expression.Text == "grouping")
                throw new SqlException(SqlStates.GroupingError, "aggregate in RETURNING");
            foreach (var argument in expression.Args) Check(argument);
            if (expression.Predicate is { } predicate) Check(predicate);
        }
        foreach (var item in items) Check(item.Expression);
    }
    private static SqlValue[] ReturningRow(CatalogTable table, SqlValue[]? old, SqlValue[]? current) =>
        (current ?? old!).Concat(old ?? table.Columns.Select(column => SqlValue.Null(column.Type.Type))).Concat(current ?? table.Columns.Select(column => SqlValue.Null(column.Type.Type))).ToArray();
    private static Projection[] Returning(CatalogTable table, SelectItem[] items, Catalog catalog, Meter meter, SubqueryBinder? binder = null, ReturningAliases? aliases = null)
    {
        ValidateReturning(items);
        return items.Length == 0 ? [] : Resolve.Select(Resolve.BindTypes(new SelectSyntax(binder is null ? items : items.Select(item => item.Expression.Op == "star" ? item : item with { Expression = binder.Bind(item.Expression) }).ToArray(), null, null, 0), catalog, meter.Temporal), ReturningColumns(table, aliases)).Projections;
    }

    private static async ValueTask<StatementResult> Mutate(Database database, PendingDatabase pending, string name, Expression? where,
        AssignmentSyntax[]? assignments, SelectItem[] items, Meter meter, CancellationToken cancellationToken, Expression[]? viewChecks = null, ViewBinding? viewBinding = null, StatementContext? context = null, ReturningAliases? aliases = null)
    {
        var table = Table(pending.Catalog, name); var tree = database.Tree(table, pending, pending.Before);
        var readState = context?.Read ?? pending.Fork();
        var scanTree = database.Tree(table, readState, pending.Before);
        var columns = viewBinding?.Columns ?? Columns(table);
        var binder = new SubqueryBinder(columns, database, pending.Before, readState, readState.Catalog, meter, cancellationToken, context?.Scope);
        var filter = PredicatePlan.Order(where is null ? null : Resolve.Expr(binder.Bind(where)));
        if (filter is not null && filter.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("WHERE requires boolean");
        var targets = assignments?.Select(a => (Slot: ColumnIndex(table, a.Column), Value: a.Value.Op == "default" ? a.Value : binder.Bind(a.Value))).ToArray();
        if (targets is not null && targets.Select(t => t.Slot).Distinct().Count() != targets.Length) throw new SqlException(SqlStates.DuplicateColumn, "column assigned twice");
        if (targets is not null)
            foreach (var target in targets)
                if (target.Value.Op != "default" && table.Columns[target.Slot].IdentityAlways) throw new SqlException(SqlStates.GeneratedAlways, "explicit update of GENERATED ALWAYS identity");
                else if (target.Value.Op != "default") Assignable(Resolve.Expr(AssignmentLiteral(Resolve.Bind(target.Value, columns), table.Columns[target.Slot].Type), table.Columns[target.Slot].Type.Type).Type, table.Columns[target.Slot].Type.Type);
        var returnBinder = new SubqueryBinder(viewBinding?.Returning ?? ReturningColumns(table, aliases), database, pending.Before, readState, readState.Catalog, meter, cancellationToken, context?.Scope);
        var returning = Returning(table, items, pending.Catalog, meter, returnBinder, aliases);
        foreach (var check in viewChecks ?? []) _ = returnBinder.Bind(check);
        var initializationStart = meter.Accrued;
        await binder.Initialize().ConfigureAwait(false);
        context?.MutationObservation?.Filter?.AttributeLocal(meter.Accrued - initializationStart);
        if (filter is not null && Resolve.EmptyFilter(filter)) return new(returning.Select(projection => new Column(projection.Name, projection.Expression.Type)).ToArray(), [], 0);
        await returnBinder.Initialize().ConfigureAwait(false);
        var results = new List<Row>();
        var changes = new List<(StorageRecord Old, SqlValue[] Values)>();
        var storedRows = new Dictionary<string, StorageRecord>(StringComparer.Ordinal);
        var unchanged = Enumerable.Range(0, table.Columns.Length).Where(column => !table.PrimaryKey.Contains(column) && !(targets ?? []).Any(target => target.Slot == column)).ToHashSet();
        var storage = new QueryStorage(database, pending.Before, readState, readState.Catalog);
        var planningDomain = readState.Catalog.Scope?.Items.FirstOrDefault(domain => ReferenceEquals(domain.Database, database));
        var planningTable = planningDomain is { Name: not ("main" or "temp") } ? table with { Domain = planningDomain } : table;
        var access = AccessPlan.Select(planningTable, scanTree, filter is null ? null : PlanTransform.InitialValues(filter), storage, meter);
        var touched = new HashSet<int>();
        void Touch(Resolved? node)
        {
            if (node is null) return;
            if (node.Op == "column") touched.Add(node.Slot % table.Columns.Length);
            if (node.Subquery is { } cell) foreach (var slot in cell.ReferencedSlots) touched.Add(slot % table.Columns.Length);
            foreach (var child in node.Args) Touch(child);
        }
        Touch(filter); foreach (var projection in returning) Touch(projection.Expression);
        foreach (var target in targets ?? []) if (target.Value.Op != "default")
            Touch(Resolve.Expr(AssignmentLiteral(Resolve.Bind(target.Value, columns), table.Columns[target.Slot].Type), table.Columns[target.Slot].Type.Type));
        var sourceRecords = access.Records(table, scanTree, storage, meter, touched, cancellationToken);
        if (context?.MutationObservation is { } observation)
            sourceRecords = ExplainObservation.Rows(sourceRecords, observation.Scan, meter, cancellationToken);
        await foreach (var record in sourceRecords.ConfigureAwait(false))
        {
            var deferred = scanTree.DecodeDeferredRow(table, record);
            if (!await QueryExecution.KeepsAsync(filter, deferred, meter, context?.MutationObservation?.Filter).ConfigureAwait(false)) continue;
            var materialized = await scanTree.MaterializeRecordAsync(record, cancellationToken).ConfigureAwait(false);
            var original = table.DecodeRow(materialized);
            var values = original.ToArray();
            if (targets is not null)
                foreach (var target in targets.OrderBy(target => target.Slot))
                    values[target.Slot] = target.Value.Op == "default" ? DefaultValue(table.Columns[target.Slot], meter, pending.Catalog) :
                        await AssignAsync(target.Value, table.Columns[target.Slot].Type, columns, original, meter, pending.Catalog).ConfigureAwait(false);
            if (targets is not null) Validate(table, values, meter, pending.Catalog);
            storedRows.Add(Convert.ToHexString(record.Key), record);
            changes.Add((materialized, values));
        }
        if (targets is not null)
            await ValidatePinnedKeys(database, context, table, changes.Select(change =>
                (table.EncodeRow(change.Values, table.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(change.Old.Key) : 0), change.Values)),
                changes.Select(change => Convert.ToHexString(change.Old.Key)).ToHashSet(StringComparer.Ordinal), meter, cancellationToken).ConfigureAwait(false);
        var removed = 0;
        var inPlace = new HashSet<string>(StringComparer.Ordinal);
        var newKeys = new HashSet<string>(StringComparer.Ordinal);
        var changedIndexes = new Dictionary<string, CatalogTable>(StringComparer.Ordinal);
        if (targets is not null)
            foreach (var change in changes)
                if (!newKeys.Add(Convert.ToHexString(table.EncodeRow(change.Values, table.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(change.Old.Key) : 0).Key)))
                    throw new SqlException(SqlStates.UniqueViolation, "duplicate primary key");
        foreach (var change in changes)
        {
            var currentKey = context?.CurrentKey(table.Name, change.Old.Key) ?? (context is null ? change.Old.Key : null);
            var current = currentKey is null ? null : await tree.FindAsync(currentKey, cancellationToken).ConfigureAwait(false);
            if (current is not null)
            {
                var currentValues = await tree.MaterializeRecordAsync(current, cancellationToken).ConfigureAwait(false);
                var replacement = assignments is null ? null : table.EncodeRow(change.Values, table.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(change.Old.Key) : 0);
                if (replacement is not null && current.Key.AsSpan().SequenceEqual(replacement.Key)) inPlace.Add(Convert.ToHexString(change.Old.Key));
                else tree = await tree.DeleteAsync(current.Key, cancellationToken).ConfigureAwait(false);
                var oldValues = table.DecodeRow(currentValues);
                var indexTable = table with { Indexes = (table.Indexes ?? []).Where(index =>
                {
                    if (replacement is null || index.Kind == 1) return true;
                    var oldEntry = IndexRecord(table, index, currentValues, oldValues, meter, pending.Catalog);
                    var newEntry = IndexRecord(table, index, replacement, change.Values, meter, pending.Catalog);
                    return (oldEntry is null) != (newEntry is null) || oldEntry is not null && newEntry is not null &&
                        !oldEntry.Record.Key.AsSpan().SequenceEqual(newEntry.Record.Key);
                }).ToArray() };
                changedIndexes[Convert.ToHexString(change.Old.Key)] = indexTable;
                await RemoveIndexes(database, pending, indexTable, currentValues, meter, cancellationToken, assignments is null ? null : change.Values, replacement?.Key).ConfigureAwait(false);
                removed++;
            }
            context?.Move(table.Name, change.Old.Key, null);
        }
        foreach (var change in changes)
        {
            if (targets is not null)
            {
                var encoded = tree.KeepUnchangedChains(tree.PrepareRecord(table.EncodeRow(change.Values, table.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(change.Old.Key) : 0), meter), storedRows[Convert.ToHexString(change.Old.Key)], unchanged);
                if (await tree.FindAsync(encoded.Key, cancellationToken).ConfigureAwait(false) is not null && !inPlace.Contains(Convert.ToHexString(change.Old.Key))) throw new SqlException(SqlStates.UniqueViolation, "duplicate primary key");
                tree = await tree.UpsertAsync(encoded, cancellationToken).ConfigureAwait(false);
                await AddIndexes(database, pending, changedIndexes.GetValueOrDefault(Convert.ToHexString(change.Old.Key), table), encoded, change.Values, meter, cancellationToken).ConfigureAwait(false);
                context?.Move(table.Name, change.Old.Key, encoded.Key);
            }
        }
        if (targets is not null) foreach (var change in changes) await ValidateViewChecks(viewChecks, change.Values, meter).ConfigureAwait(false);
        meter.Checkpoint();
        if (changes.Count != 0) { Put(pending, table with { RowCount = table.RowCount - removed + (assignments is null ? 0 : changes.Count) }, tree); StaleStatistics(pending, table.Name); }
        HoldPendingMemory(pending, meter);
        if (changes.Count != 0)
        {
            if (assignments is not null) await ValidateExclusions(database, pending, table, changes.Select(change =>
                (table.EncodeRow(change.Values, table.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(change.Old.Key) : 0), change.Values)), meter, cancellationToken, context,
                changes.Select(change => Convert.ToHexString(change.Old.Key)).ToHashSet(StringComparer.Ordinal)).ConfigureAwait(false);
            await ApplyReferentialActions(database, pending, table, changes, assignments is null, meter, cancellationToken, context: context).ConfigureAwait(false);
            if (assignments is not null) await ForeignCheck(context, () => ValidateForeignKeys(database, pending, table, changes.Select(change => change.Values).ToArray(), cancellationToken)).ConfigureAwait(false);
        }
        if (returning.Length != 0) foreach (var change in changes) results.Add((await QueryExecution.ProjectAsync(returning, ReturningRow(table, table.DecodeRow(change.Old), assignments is null ? null : change.Values), meter).ConfigureAwait(false)).RetainMemory(meter));
        return new(returning.Select(p => new Column(p.Name, p.Expression.Type)).ToArray(), results.ToArray(), changes.Count);
    }
    private static Expression AssignmentLiteral(Expression expression, TypeSpec type)
    {
        if (expression.Op != "text") return expression;
        SqlValue value;
        if (type.Type.IsFloat() || type.Type.Kind == SqlKind.Composite) value = Resolve.ParseText(expression.Text, type.Type);
        else if (type.Type == SqlType.Interval) value = SqlValue.Interval(IntervalInput.Parse(expression.Text, type.IntervalFields, type.Precision));
        else return expression;
        return new("bound", "", [], Binding: new("constant", value.Type, value, []));
    }
    private static async ValueTask<SqlValue> AssignAsync(Expression expression, TypeSpec type, BoundColumn[] columns, SqlValue[] row, Meter meter, Catalog catalog)
    {
        expression = AssignmentLiteral(expression, type);
        var resolved = Resolve.Expr(Resolve.Bind(Resolve.BindTypes(expression, catalog, meter.Temporal), columns), type.Type);
        Assignable(resolved.Type, type.Type);
        var value = Resolve.Cast(await Evaluate.ExprAsync(resolved, meter, row: row).ConfigureAwait(false), type.Type);
        if (!value.IsNull && type.Type == SqlType.Text && type.Length is { } length && value.AsText().EnumerateRunes().Skip(length).Any(rune => rune.Value != 32))
            throw new SqlException(SqlStates.StringDataRightTruncation, "value exceeds varchar length");
        return Resolve.ApplyModifier(value, type);
    }
    private static SqlValue Assign(Expression expression, TypeSpec type, BoundColumn[] columns, SqlValue[] row, Meter meter, Catalog catalog)
    {
        expression = AssignmentLiteral(expression, type);
        expression = Resolve.BindTypes(expression, catalog, meter.Temporal);
        var resolved = Resolve.Expr(Resolve.Bind(expression, columns), type.Type);
        Assignable(resolved.Type, type.Type);
        var value = Resolve.Cast(Evaluate.Expr(resolved, meter, row: row), type.Type);
        if (!value.IsNull && type.Type == SqlType.Text && type.Length is { } length && value.AsText().EnumerateRunes().Skip(length).Any(rune => rune.Value != 32))
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
