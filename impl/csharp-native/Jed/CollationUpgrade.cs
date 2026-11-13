namespace Jed;

internal static partial class StatementExecution
{
    internal static async ValueTask UpgradeCollations(Database database, PendingDatabase pending, Meter meter, CancellationToken token)
    {
        var bound = pending.Catalog.BindCollations(meter.Unicode, meter.Temporal.Data);
        var changed = (bound.Collations ?? []).Where(reference =>
        {
            var data = meter.Unicode!.Resolve(reference.Name);
            return data.UnicodeVersion != reference.UnicodeVersion || data.CldrVersion != reference.CldrVersion;
        }).Select(reference => reference.Name).ToHashSet(StringComparer.Ordinal);
        if (changed.Count == 0) return;
        bool Uses(Resolved expression) => expression.Collation?.Name is { } name && changed.Contains(name) || expression.Args.Any(Uses);
        bool Rebuild(CatalogTable table) => table.Columns.Any(column => column.Collation is { } name && changed.Contains(name)) ||
            (table.Indexes ?? []).Any(index => index.Keys.Any(key => key.Expression is { } text && Uses(Resolve.Expr(Resolve.Bind(Resolve.BindTypes(StoredExpression(text), bound, meter.Temporal), Columns(table))))));
        var selected = bound.Tables.Where(Rebuild).Select(table => table.Name).ToHashSet(StringComparer.Ordinal);
        pending.Catalog = bound with { Collations = (bound.Collations ?? []).Select(reference =>
        {
            if (!changed.Contains(reference.Name)) return reference;
            var data = meter.Unicode!.Resolve(reference.Name);
            return reference with { UnicodeVersion = data.UnicodeVersion, CldrVersion = data.CldrVersion, Description = data.Description };
        }).ToArray() };
        pending.Catalog = pending.Catalog.BindCollations(meter.Unicode, meter.Temporal.Data);
        foreach (var table in pending.Catalog.Tables.Where(table => selected.Contains(table.Name)).ToArray())
        {
            var source = database.Tree(table, pending, pending.Before);
            var target = await source.ClearAsync(token).ConfigureAwait(false);
            await foreach (var record in source.ScanAsync(token).ConfigureAwait(false))
            {
                var row = table.DecodeRow(await source.MaterializeRecordAsync(record, token).ConfigureAwait(false));
                var encoded = table.EncodeRow(row, table.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(record.Key) : 0);
                target = await target.UpsertAsync(target.PrepareRecord(encoded), token).ConfigureAwait(false);
            }
            Put(pending, table, target);
            ClearStatistics(pending, table.Name);
            foreach (var index in table.Indexes ?? [])
            {
                var old = await database.IndexTree(index, pending, pending.Before).ClearAsync(token).ConfigureAwait(false);
                pending.RetiredPages.UnionWith(old.RetiredPages); pending.IndexTrees.Remove(index.Name);
                await BuildIndex(database, pending, table, index, meter, token, chargeScan: false).ConfigureAwait(false);
            }
        }
        pending.Changed = true;
    }
}

public sealed partial class Session
{
    /// <summary>Atomically rebuild collated keys and update their persisted Unicode version pins.</summary>
    public async Task UpgradeCollationsAsync(CancellationToken cancellationToken = default)
    {
        using var operation = Enter();
        if (TransactionOpen) throw new SqlException(SqlStates.ActiveSqlTransaction, "collation upgrade requires no transaction");
        var db = await GetDatabase(cancellationToken).ConfigureAwait(false);
        var pending = await db.BeginWriteAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            var meter = new Meter(this, 0, cancellationToken);
            await StatementExecution.UpgradeCollations(db, pending, meter, cancellationToken).ConfigureAwait(false);
            await db.CommitAsync(pending, cancellationToken).ConfigureAwait(false);
        }
        finally { await db.EndWriteAsync(pending).ConfigureAwait(false); }
    }
}
