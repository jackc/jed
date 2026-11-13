namespace Jed;

internal static partial class StatementExecution
{
    private static bool ExclusionPredicate(CatalogTable table, CatalogIndex index, SqlValue[] values, Meter meter, Catalog catalog)
    {
        using var unmetered = meter.SuppressCost();
        return index.Predicate is not { } predicate || QueryExecution.Keeps(Resolve.Expr(Resolve.Bind(
            Resolve.BindTypes(StoredExpression(predicate), catalog, meter.Temporal), Columns(table))), values, meter);
    }
    private static bool ExclusionParticipates(CatalogTable table, CatalogIndex index, CatalogExclusion exclusion,
        SqlValue[] values, Meter meter, Catalog catalog)
    {
        if (!ExclusionPredicate(table, index, values, meter, catalog)) return false;
        for (var i = 0; i < exclusion.Columns.Length; i++)
        {
            var value = values[exclusion.Columns[i]];
            if (value.IsNull || exclusion.Operators[i] == 0 && value.AsRange().IsEmpty) return false;
        }
        return true;
    }
    private static bool ExclusionPair(CatalogTable table, CatalogIndex index, CatalogExclusion exclusion,
        StorageRecord left, SqlValue[] a, StorageRecord right, SqlValue[] b, Meter meter, Catalog catalog)
    {
        if (!ExclusionParticipates(table, index, exclusion, a, meter, catalog) || !ExclusionParticipates(table, index, exclusion, b, meter, catalog)) return false;
        if (index.Kind == 0) return IndexRecord(table, index, left, a, meter, catalog)!.Prefix.AsSpan()
            .SequenceEqual(IndexRecord(table, index, right, b, meter, catalog)!.Prefix);
        return GistBounds.FromValues(exclusion.Columns.Select(column => a[column]).ToArray())!
            .Matches(exclusion.Columns.Select(column => b[column]).ToArray(), exclusion.Operators, leaf: true);
    }
    private static async ValueTask<byte[]?> ExclusionConflict(Database database, PendingDatabase pending, CatalogTable table,
        CatalogExclusion exclusion, StorageRecord record, SqlValue[] values, ISet<string> ignored, Meter meter, CancellationToken token)
    {
        var index = (table.Indexes ?? []).First(index => index.Name == exclusion.Index);
        if (!ExclusionParticipates(table, index, exclusion, values, meter, pending.Catalog)) return null;
        if (index.Kind == 2)
        {
            var tree = await database.GistAsync(table, index, pending, pending.Before, token).ConfigureAwait(false);
            return tree.Probe(exclusion.Columns.Select(column => values[column]).ToArray(), exclusion.Operators)
                .FirstOrDefault(key => !ignored.Contains(Convert.ToHexString(key)));
        }
        var entry = IndexRecord(table, index, record, values, meter, pending.Catalog)!;
        await foreach (var existing in database.IndexTree(index, pending, pending.Before)
            .ScanRangeAsync(entry.Prefix, true, PrefixEnd(entry.Prefix), false, cancellationToken: token).ConfigureAwait(false))
        {
            var key = IndexStorageKey(table, index, existing.Key, pending.Catalog);
            if (!ignored.Contains(Convert.ToHexString(key))) return key;
        }
        return null;
    }
    private static SqlException ExclusionError(CatalogExclusion exclusion) => new(SqlStates.ExclusionViolation,
        $"conflicting key value violates exclusion constraint \"{exclusion.Name}\"");

    // All candidate PK/unique checks have completed before this pass. The pending
    // indexes represent the final image, allowing updates that exchange keys.
    private static async ValueTask ValidateExclusions(Database database, PendingDatabase pending, CatalogTable table,
        IEnumerable<(StorageRecord Record, SqlValue[] Values)> rows, Meter meter, CancellationToken token,
        StatementContext? context = null, ISet<string>? replaced = null)
    {
        if ((table.Exclusions?.Length ?? 0) == 0) return;
        var candidates = rows.ToArray();
        foreach (var exclusion in table.Exclusions!.OrderBy(item => item.Name, Utf8NameComparer.Instance))
            foreach (var (record, values) in candidates)
            {
                var ignored = new HashSet<string>(StringComparer.Ordinal) { Convert.ToHexString(record.Key) };
                if (await ExclusionConflict(database, pending, table, exclusion, record, values, ignored, meter, token).ConfigureAwait(false) is not null)
                    throw ExclusionError(exclusion);
                if (context is not null && await ExclusionConflict(database, context.Read, table, exclusion, record, values,
                    replaced ?? new HashSet<string>(StringComparer.Ordinal), meter, token).ConfigureAwait(false) is not null) throw ExclusionError(exclusion);
            }
    }
    private static void ValidateAddedExclusions(CatalogTable table, CatalogTable original, SqlValue[][] rows, Meter meter, Catalog catalog)
    {
        foreach (var exclusion in (table.Exclusions ?? []).Where(item => !(original.Exclusions ?? []).Any(prior => prior == item)).OrderBy(item => item.Name, Utf8NameComparer.Instance))
        {
            var index = table.Indexes!.First(index => index.Name == exclusion.Index);
            var participating = rows.Where(row => ExclusionPredicate(table, index, row, meter, catalog)).ToArray();
            for (var i = 0; i < participating.Length; i++) for (var j = i + 1; j < participating.Length; j++)
            {
                meter.Charge(Costs.ConstraintCheck); meter.Checkpoint();
                if (ExclusionPair(table, index, exclusion, new([], []), participating[i], new([], []), participating[j], meter, catalog)) throw ExclusionError(exclusion);
            }
        }
    }
}
