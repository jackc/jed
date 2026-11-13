using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record GistAccess(CatalogIndex Index, SqlValue Query, byte Strategy, QueryStorage Storage, Resolved Predicate)
{
    internal static GistAccess? Select(CatalogTable table, Resolved? filter, QueryStorage storage, Meter meter)
    => Candidates(table, filter, storage, meter).FirstOrDefault();
    internal static IEnumerable<GistAccess> Candidates(CatalogTable table, Resolved? filter, QueryStorage storage, Meter meter)
    {
        if (filter is null) yield break;
        foreach (var index in (table.Indexes ?? []).OrderBy(item => item.Name, Utf8NameComparer.Instance))
        {
            if (!StatementExecution.IndexDependenciesUsable(index, storage.Catalog)) continue;
            if (index.Kind != 2 || index.Keys.Length != 1 || index.Predicate is not null) continue;
            var column = index.Keys[0].Column; var type = table.Columns[column].Type.Type;
            foreach (var term in PredicatePlan.Conjuncts(filter))
            {
                if (term.Args.Length != 2) continue;
                var left = term.Args[0]; var right = term.Args[1]; var op = term.Op == "open_predicate" ? term.Text : term.Op;
                if (Evaluate.Constant(left) && right.Op == "column")
                { (left, right) = (right, left); op = op == "<@" ? "@>" : op == "@>" ? "<@" : op; }
                if (left.Op != "column" || left.Slot != column || !right.ParameterUnbound && !Evaluate.Constant(right)) continue;
                SqlValue query;
                using (meter.SuppressCost()) query = right.ParameterUnbound ? SqlValue.Null(right.Type) : Evaluate.Expr(right, meter);
                if (type.Kind == SqlKind.Range)
                {
                    if (op is not ("&&" or "@>")) continue;
                    if (op == "@>" && right.Type.Kind == SqlKind.Range && !query.IsNull && query.AsRange().IsEmpty) continue;
                    yield return new(index, query, op == "&&" ? (byte)0 : (byte)2, storage, term);
                }
                if (op == "=" && right.Type == type) yield return new(index, query, 1, storage, term);
            }
        }
    }
    internal async IAsyncEnumerable<SqlValue[]> Rows(CatalogTable table, StorageTree primary, Meter meter,
        IReadOnlySet<int>? touched, [EnumeratorCancellation] CancellationToken token, bool reverse = false)
    {
        await foreach (var record in Records(table, primary, meter, touched, token, reverse).ConfigureAwait(false))
            yield return primary.DecodeDeferredRow(table, record);
    }
    internal async IAsyncEnumerable<StorageRecord> Records(CatalogTable table, StorageTree primary, Meter meter,
        IReadOnlySet<int>? touched, [EnumeratorCancellation] CancellationToken token, bool reverse = false)
    {
        if (Query.IsNull || Strategy == 0 && Query.Type.Kind == SqlKind.Range && Query.AsRange().IsEmpty) yield break;
        var gist = await Storage.Database.GistAsync(table, Index, Storage.Pending, Storage.Snapshot, token).ConfigureAwait(false);
        var keys = gist.Probe([Query], [Strategy], meter);
        foreach (var key in reverse ? keys.Reverse() : keys)
        {
            meter.Charge(primary.Height); meter.Checkpoint(token);
            var row = await primary.FindAsync(key, token).ConfigureAwait(false) ?? throw StorageFormat.Corrupt("GiST index names a missing row");
            meter.Charge(primary.ValueReadUnits(row, touched)); meter.Charge(Costs.StorageRowRead); meter.Checkpoint(token);
            yield return row;
        }
    }
}
