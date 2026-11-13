namespace Jed;

internal static class GinIndex
{
    internal static void ValidateType(SqlType type)
    {
        if (StatisticsTerms.Eligible(type)) return;
        if (type.Kind == SqlKind.Array) throw Errors.Unsupported("GIN array element type");
        throw new SqlException(SqlStates.UndefinedObject, "type has no GIN operator class");
    }
    internal static byte[] Tag(CatalogIndex index, int slot, byte[] term) => index.Keys.Length == 1 ? term : [(byte)slot, ..term];
    internal static SortedSet<byte[]> Terms(CatalogIndex index, SqlValue[] values)
    {
        var result = new SortedSet<byte[]>(StatisticsBytes.Instance);
        for (var i = 0; i < index.Keys.Length; i++)
            foreach (var term in StatisticsTerms.Extract(values[index.Keys[i].Column])) result.Add(Tag(index, i, term));
        return result;
    }
}

internal static partial class StatementExecution
{
    private static bool GinIncludes(CatalogTable table, CatalogIndex index, SqlValue[] values, Meter meter, Catalog catalog)
    {
        using var suppressed = meter.SuppressCost();
        return index.Predicate is null || QueryExecution.Keeps(ResolveIndexPredicate(table, index, catalog), values, meter);
    }
    private static async ValueTask AddGin(Database database, PendingDatabase pending, CatalogTable table, CatalogIndex index,
        StorageRecord record, SqlValue[] values, Meter meter, CancellationToken token)
    {
        if (!GinIncludes(table, index, values, meter, pending.Catalog)) return;
        var tree = database.IndexTree(index, pending, pending.Before);
        foreach (var term in GinIndex.Terms(index, values)) tree = await GinRuns.Edit(tree, term, record.Key, true, token).ConfigureAwait(false);
        pending.IndexTrees[index.Name] = tree;
    }
    private static async ValueTask RemoveGin(Database database, PendingDatabase pending, CatalogTable table, CatalogIndex index,
        StorageRecord record, SqlValue[] values, Meter meter, CancellationToken token, SqlValue[]? replacementValues, byte[]? replacementKey)
    {
        if (!GinIncludes(table, index, values, meter, pending.Catalog)) return;
        var terms = GinIndex.Terms(index, values);
        if (replacementValues is not null && record.Key.AsSpan().SequenceEqual(replacementKey) && GinIncludes(table, index, replacementValues, meter, pending.Catalog))
            terms.ExceptWith(GinIndex.Terms(index, replacementValues));
        var tree = database.IndexTree(index, pending, pending.Before);
        foreach (var term in terms) tree = await GinRuns.Edit(tree, term, record.Key, false, token).ConfigureAwait(false);
        pending.IndexTrees[index.Name] = tree;
    }
    private static async ValueTask BuildGin(Database database, PendingDatabase pending, CatalogTable table, CatalogIndex index,
        Meter meter, CancellationToken token, bool chargeScan)
    {
        var source = database.Tree(table, pending, pending.Before);
        var touched = new HashSet<int>(index.Keys.Select(key => key.Column));
        void Touch(Resolved expression)
        {
            if (expression.Op == "column") touched.Add(expression.Slot);
            foreach (var argument in expression.Args) Touch(argument);
        }
        if (ResolveIndexPredicate(table, index, pending.Catalog) is { } predicate) Touch(predicate);
        if (chargeScan)
        {
            meter.Charge(await source.CountPagesAsync(token).ConfigureAwait(false));
            await foreach (var record in source.ScanAsync(token).ConfigureAwait(false)) meter.Charge(source.ValueReadUnits(record, touched));
            meter.Checkpoint();
        }
        var terms = new SortedDictionary<byte[], List<byte[]>>(StatisticsBytes.Instance);
        await foreach (var record in source.ScanAsync(token).ConfigureAwait(false))
        {
            if (chargeScan) meter.Charge(Costs.StorageRowRead); meter.Checkpoint();
            var values = table.Columns.Select(column => SqlValue.Null(column.Type.Type)).ToArray();
            foreach (var column in touched)
            {
                var cell = await source.MaterializeCellAsync(record, column, token).ConfigureAwait(false);
                if (cell is not null) values[column] = StoredValues.FromLeaf(cell.AsMemory(), table.Columns[column].Type);
            }
            if (!GinIncludes(table, index, values, meter, pending.Catalog)) continue;
            foreach (var term in GinIndex.Terms(index, values))
            { if (!terms.TryGetValue(term, out var postings)) terms.Add(term, postings = []); postings.Add(record.Key); }
        }
        var tree = database.IndexTree(index with { Root = 0 }, null, pending.Before);
        foreach (var (term, postings) in terms)
            foreach (var run in GinRuns.Pack(term, postings)) tree = await tree.UpsertAsync(new(run, []), token).ConfigureAwait(false);
        pending.IndexTrees[index.Name] = tree;
    }
    private static async ValueTask<StatementResult> CreateGin(Database database, PendingDatabase pending, CatalogTable table,
        CreateIndexSyntax create, Meter meter, CancellationToken token)
    {
        var keys = new List<CatalogIndexKey>(); var parts = new List<string>();
        foreach (var key in create.Keys)
        {
            var resolved = Resolve.Expr(Resolve.Bind(Resolve.BindTypes(key.Expression, pending.Catalog, meter.Temporal), Columns(table)));
            if (resolved.Op != "column") throw Errors.Unsupported("GIN expression index");
            GinIndex.ValidateType(resolved.Type);
            if (key.Options != 0) throw Errors.Unsupported("GIN ordering options");
            keys.Add(new(resolved.Slot)); parts.Add(table.Columns[resolved.Slot].Name);
        }
        if (create.Unique) throw Errors.Unsupported("unique GIN index");
        if (keys.Count > 32) throw new SqlException(SqlStates.TooManyColumns, "index has more than 32 keys");
        if (create.Predicate is { } predicate)
        {
            ValidateStoredExpression(predicate, immutable: true);
            var resolved = Resolve.Expr(Resolve.Bind(Resolve.BindTypes(predicate, pending.Catalog, meter.Temporal), Columns(table)));
            if (resolved.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("index predicate requires boolean");
            ValidateIndexDependencies(resolved); PinExpressionCollations(pending, resolved, meter);
        }
        if (create.Name is { } explicitName && RelationExists(pending.Catalog, explicitName))
        { if (create.IfNotExists) return new([], []); throw new SqlException(SqlStates.DuplicateTable, "relation already exists"); }
        var used = new HashSet<string>(StringComparer.Ordinal);
        for (var i = 0; i < parts.Count; i++)
        { var original = parts[i]; var suffix = 0; while (!used.Add(parts[i])) parts[i] = original + (++suffix).ToString(System.Globalization.CultureInfo.InvariantCulture); }
        var name = create.Name ?? GeneratedName(table.Name, string.Join('_', parts), "idx", candidate => RelationExists(pending.Catalog, candidate));
        var index = new CatalogIndex(name, keys.ToArray(), Kind: 1, Predicate: create.PredicateText);
        await BuildGin(database, pending, table, index, meter, token, true).ConfigureAwait(false);
        Put(pending, table with { Indexes = (table.Indexes ?? []).Append(index).OrderBy(item => item.Name, Utf8NameComparer.Instance).ToArray() });
        return new([], []);
    }
}
