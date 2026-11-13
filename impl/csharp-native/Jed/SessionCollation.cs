namespace Jed;

public sealed partial class Session
{
    /// <summary>Set the persisted default for subsequently created text columns using host-loaded data.</summary>
    public async Task SetDefaultCollationAsync(string name, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrEmpty(name);
        using var operation = Enter();
        if (TransactionOpen) throw new SqlException(SqlStates.ActiveSqlTransaction, "default collation change requires no transaction");
        var db = await GetDatabase(cancellationToken).ConfigureAwait(false);
        var pending = await db.BeginWriteAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            _ = StatementExecution.ReferenceCollation(pending, name, unicodeData);
            var before = pending.Catalog.Collations ?? [];
            var changed = before.Any(reference => reference.IsDefault != (reference.Name == name));
            if (changed)
            {
                pending.Catalog = pending.Catalog with { Collations = before.Select(reference => reference with { IsDefault = reference.Name == name }).ToArray() };
                pending.Changed = true;
            }
            await db.CommitAsync(pending, cancellationToken).ConfigureAwait(false);
        }
        finally { await db.EndWriteAsync(pending).ConfigureAwait(false); }
    }
    /// <summary>Read the persisted default collation; existing columns retain their frozen choice.</summary>
    public async Task<string> DefaultCollationAsync(CancellationToken cancellationToken = default)
    {
        using var operation = Enter();
        var db = await GetDatabase(cancellationToken).ConfigureAwait(false);
        await using var pin = await db.PinAsync(cancellationToken).ConfigureAwait(false);
        var catalog = transaction?.Catalog ?? readTransaction?.Snapshot.Catalog ?? pin.Snapshot.Catalog;
        return catalog.Collations?.FirstOrDefault(reference => reference.IsDefault)?.Name ?? "C";
    }
}
