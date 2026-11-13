namespace Jed;

public sealed partial class Database
{
    internal ValueTask<GistTree> GistAsync(CatalogTable table, CatalogIndex index, PendingDatabase? pending, DatabaseSnapshot snapshot, CancellationToken token)
    {
        if (index.Domain is { } domain) return domain.Database.GistAsync(table with { Domain = null }, index with { Domain = null }, domain.Pending, domain.Snapshot, token);
        return GistTree.BuildAsync(IndexTree(index, pending, snapshot), index.Keys.Select(key => table.Columns[key.Column].Type.Type).ToArray(), token);
    }
}
internal static partial class StatementExecution
{
    private static IndexedRecord? GistRecord(CatalogTable table, CatalogIndex index, StorageRecord record, SqlValue[] values, Meter meter, Catalog catalog)
    {
        using var unmetered = meter.SuppressCost();
        if (ResolveIndexPredicate(table, index, catalog) is { } predicate && !QueryExecution.Keeps(predicate, values, meter)) return null;
        var bound = GistBounds.FromValues(index.Keys.Select(key => values[key.Column]).ToArray());
        return bound is null ? null : new(new(GistTree.LeafKey(bound, record.Key), []), [], false);
    }
    private static async ValueTask<StatementResult> CreateGist(Database database, PendingDatabase pending, CatalogTable table,
        CreateIndexSyntax create, Meter meter, CancellationToken token)
    {
        var keys = new List<CatalogIndexKey>();
        foreach (var key in create.Keys)
        {
            var resolved = Resolve.Expr(Resolve.Bind(Resolve.BindTypes(key.Expression, pending.Catalog, meter.Temporal), Columns(table)));
            if (resolved.Op != "column") throw Errors.Unsupported("GiST expression index");
            GistBounds.ValidateType(resolved.Type);
            if (key.Options != 0) throw Errors.Unsupported("GiST ordering options");
            keys.Add(new(resolved.Slot));
        }
        if (create.Unique || keys.Count != 1 || create.Predicate is not null) throw Errors.Unsupported("GiST unique, multi-column or partial CREATE INDEX");
        if (create.Name is { } explicitName && RelationExists(pending.Catalog, explicitName))
        { if (create.IfNotExists) return new([], []); throw new SqlException(SqlStates.DuplicateTable, "relation already exists"); }
        var name = create.Name ?? GeneratedName(table.Name, table.Columns[keys[0].Column].Name, "idx", candidate => RelationExists(pending.Catalog, candidate));
        var index = new CatalogIndex(name, keys.ToArray(), Kind: 2);
        await BuildIndex(database, pending, table, index, meter, token).ConfigureAwait(false);
        Put(pending, table with { Indexes = (table.Indexes ?? []).Append(index).OrderBy(item => item.Name, Utf8NameComparer.Instance).ToArray() });
        return new([], []);
    }
}
