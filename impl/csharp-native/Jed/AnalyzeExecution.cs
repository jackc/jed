namespace Jed;

internal static partial class StatementExecution
{
    private static async ValueTask<StatementResult> Analyze(Database database, PendingDatabase pending, AnalyzeSyntax statement,
        Meter meter, CancellationToken token)
    {
        var table = pending.Catalog.Tables.FirstOrDefault(table => table.Name == statement.Table);
        if (table is null)
        {
            if (RelationExists(pending.Catalog, statement.Table)) throw new SqlException(SqlStates.WrongObjectType, "ANALYZE requires a table");
            throw new SqlException(SqlStates.UndefinedTable, "table does not exist");
        }
        var columns = statement.Columns is null ? Enumerable.Range(0, table.Columns.Length).ToArray() : statement.Columns.Select(name => ColumnIndex(table, name)).ToArray();
        if (columns.Distinct().Count() != columns.Length) throw new SqlException(SqlStates.DuplicateColumn, "ANALYZE column specified more than once");
        WritableCollations(table, pending.Catalog);
        var tree = database.Tree(table, pending, pending.Before); var facts = new List<CatalogStatistic>();
        foreach (var column in columns)
        {
            var touched = new HashSet<int> { column };
            meter.Charge(await tree.CountPagesAsync(token).ConfigureAwait(false));
            await foreach (var record in tree.ScanAsync(token).ConfigureAwait(false)) meter.Charge(tree.ValueReadUnits(record, touched));
            meter.Checkpoint(); var collector = new ColumnStatistics(table, column, meter);
            await foreach (var record in tree.ScanAsync(token).ConfigureAwait(false))
            {
                meter.Charge(Costs.StorageRowRead); meter.Checkpoint();
                var cell = await tree.MaterializeCellAsync(record, column, token).ConfigureAwait(false);
                var value = cell is null ? SqlValue.Null(table.Columns[column].Type.Type) : StoredValues.FromLeaf(cell.AsMemory(), table.Columns[column].Type);
                collector.Add(record.Key, value);
            }
            facts.AddRange(collector.Finish());
        }
        pending.Catalog = pending.Catalog with { Statistics = (pending.Catalog.Statistics ?? []).Where(stat => stat.Table != table.Name || !columns.Contains(stat.Column)).Concat(facts).ToArray() };
        pending.Changed = true;
        return new([], []);
    }
    private static void StaleStatistics(PendingDatabase pending, string table) => pending.Catalog = pending.Catalog with
    { Statistics = pending.Catalog.Statistics?.Select(stat => stat.Table == table && stat.Kind == 0 ? stat with { Flags = (byte)(stat.Flags | 1) } : stat).ToArray() };
    private static void ClearStatistics(PendingDatabase pending, string table) => pending.Catalog = pending.Catalog with
    { Statistics = pending.Catalog.Statistics?.Where(stat => stat.Table != table).ToArray() };
}
