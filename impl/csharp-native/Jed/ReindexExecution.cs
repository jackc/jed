namespace Jed;

internal static partial class StatementExecution
{
    private static async ValueTask<StatementResult> Reindex(Database database, PendingDatabase pending, ReindexSyntax statement, Meter meter, CancellationToken token)
    {
        if (statement.Kind is "database" or "system")
        {
            var name = statement.Name ?? "main"; var scope = pending.Catalog.Scope;
            if (name == "temp" || name != "main" && (scope is null || !scope.TryGet(name, out _)))
                throw Errors.Unsupported("can only reindex an open database");
            if (statement.Kind == "system") return new([], []);
            if (scope is null)
            {
                foreach (var table in pending.Catalog.Tables.OrderBy(table => table.Name, Utf8NameComparer.Instance))
                    await ReindexTable(database, pending, table, table.Indexes ?? [], meter, token).ConfigureAwait(false);
            }
            else
            {
                var domains = name == "main" ? new[] { scope.Get("main"), scope.Get("temp") } : [scope.Get(name)];
                foreach (var domain in domains)
                {
                    if (domain.ReadOnly || domain.Pending is null) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "reindex domain is read-only");
                    foreach (var table in domain.Catalog.Tables.OrderBy(table => table.Name, Utf8NameComparer.Instance).ToArray())
                        await ReindexTable(domain.Database, domain.Pending, table, table.Indexes ?? [], meter, token).ConfigureAwait(false);
                }
            }
            return new([], []);
        }
        var target = statement.Kind == "index" ? pending.Catalog.Tables.FirstOrDefault(table => (table.Indexes ?? []).Any(index => index.Name == statement.Name)) :
            pending.Catalog.Tables.FirstOrDefault(table => table.Name == statement.Name);
        if (target is null)
        {
            if (RelationExists(pending.Catalog, statement.Name!)) throw new SqlException(SqlStates.WrongObjectType, "REINDEX target has the wrong relation kind");
            throw new SqlException(SqlStates.UndefinedTable, "REINDEX relation does not exist");
        }
        await ReindexTable(database, pending, target, (target.Indexes ?? []).Where(index => statement.Kind == "table" || index.Name == statement.Name).ToArray(), meter, token).ConfigureAwait(false);
        return new([], []);
    }
    private static async ValueTask ReindexTable(Database database, PendingDatabase pending, CatalogTable table,
        CatalogIndex[] indexes, Meter meter, CancellationToken token)
    {
        WritableCollations(table, pending.Catalog);
        var rebuilt = (table.Indexes ?? []).ToDictionary(index => index.Name, StringComparer.Ordinal);
        foreach (var index in indexes.OrderBy(index => index.Name, Utf8NameComparer.Instance))
        {
            var dependencies = new List<Resolved>();
            foreach (var key in index.Keys)
            {
                var expression = key.Expression is { } text ? StoredExpression(text) : new Expression("name", table.Columns[key.Column].Name, []);
                ValidateStoredExpression(expression, immutable: true);
                var resolved = Resolve.Expr(Resolve.Bind(Resolve.BindTypes(expression, pending.Catalog, meter.Temporal), Columns(table)));
                ValidateIndexDependencies(resolved); PinExpressionCollations(pending, resolved, meter); dependencies.Add(resolved);
                if (index.Kind == 0) Keyable(resolved.Type); else if (index.Kind == 1) GinIndex.ValidateType(resolved.Type); else GistBounds.ValidateType(resolved.Type);
            }
            if (index.Predicate is { } predicate)
            {
                var expression = StoredExpression(predicate); ValidateStoredExpression(expression, immutable: true);
                var resolved = Resolve.Expr(Resolve.Bind(Resolve.BindTypes(expression, pending.Catalog, meter.Temporal), Columns(table)));
                if (resolved.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("index predicate requires boolean");
                ValidateIndexDependencies(resolved); PinExpressionCollations(pending, resolved, meter); dependencies.Add(resolved);
            }
            var old = database.IndexTree(index, pending, pending.Before);
            var cleared = await old.ClearAsync(token).ConfigureAwait(false);
            pending.RetiredPages.UnionWith(cleared.RetiredPages);
            if (index.Gist is { } gist) pending.RetiredPages.UnionWith(gist.Pages);
            pending.IndexTrees.Remove(index.Name);
            var replacement = PinIndexDependencies(index, dependencies, meter);
            await BuildIndex(database, pending, table, replacement, meter, token).ConfigureAwait(false);
            rebuilt[index.Name] = replacement;
        }
        if (indexes.Length != 0) Put(pending, table with { Indexes = rebuilt.Values.OrderBy(index => index.Name, Utf8NameComparer.Instance).ToArray() });
    }
}

public sealed partial class Session
{
    private void CheckReindexTransaction(StatementSyntax statement)
    {
        if (statement is not ReindexSyntax reindex || !TransactionOpen) return;
        if (transactionFailed) throw new SqlException(SqlStates.InFailedSqlTransaction, "transaction is aborted");
        if (reindex.Concurrently || reindex.Kind is "database" or "system")
        {
            FailStatement();
            throw new SqlException(SqlStates.ActiveSqlTransaction, "REINDEX form cannot run inside a transaction block");
        }
    }
}
