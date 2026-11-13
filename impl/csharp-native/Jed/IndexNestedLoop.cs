using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record BaseRelationAccess(CatalogTable Table, StorageTree Tree, Resolved? Filter, QueryStorage Storage);

internal sealed record IndexNestedLoop(BaseRelationAccess Source, Resolved[] Bounds, int OuterWidth)
{
    internal static IndexNestedLoop? Select(PreparedRelation inner, int outerWidth, string kind, Resolved? predicate)
    {
        if (inner.BaseAccess is not { } source || kind is not ("inner" or "cross" or "left") || predicate is null) return null;
        var terms = new List<Resolved>(); var sibling = false;
        foreach (var term in PredicatePlan.Conjuncts(predicate))
        {
            if (term.Op is not ("=" or "<" or "<=" or ">" or ">=" or "open_predicate") || term.Args.Length != 2) continue;
            if (term.Op == "open_predicate" && term.Text is not ("&&" or "@>" or "<@")) continue;
            var a = term.Args[0]; var b = term.Args[1];
            if (a.Op != "column" || a.Slot < outerWidth) (a, b) = (b, a);
            if (a.Op != "column" || a.Slot < outerWidth || !source.Table.PrimaryKey.Contains(a.Slot - outerWidth) && !(source.Table.Indexes ?? []).Any(index => StatementExecution.IndexDependenciesUsable(index, source.Storage.Catalog) && index.Kind is 0 or 2 && index.Keys.Any(key => key.Expression is null && key.Column == a.Slot - outerWidth))) continue;
            if (b.Op != "constant" && (b.Op != "column" || b.Slot >= outerWidth)) continue;
            var column = source.Table.Columns[a.Slot - outerWidth];
            if (!TableAccess.Compatible(term, column)) continue;
            if (a.Type != b.Type && !(a.Type.IsInteger() && b.Type.IsInteger()) && !(term.Op == "open_predicate" && a.Type.Kind == SqlKind.Range && a.Type.Element == b.Type)) continue;
            terms.Add(term);
            sibling |= b.Op == "column" && (source.Table.PrimaryKey.Length != 0 && a.Slot - outerWidth == source.Table.PrimaryKey[0] || (source.Table.Indexes ?? []).Any(index => StatementExecution.IndexDependenciesUsable(index, source.Storage.Catalog) && index.Kind is 0 or 2 && index.Keys.Length != 0 && index.Keys[0].Expression is null && index.Keys[0].Column == a.Slot - outerWidth));
        }
        foreach (var term in PredicatePlan.Conjuncts(predicate))
        {
            if (term.Args.Length != 2) continue;
            for (var side = 0; side < 2; side++)
            {
                var indexed = term.Args[side]; var query = term.Args[1 - side];
                if (indexed.Op != "column" || indexed.Slot < outerWidth || query.Op != "column" || query.Slot >= outerWidth) continue;
                var column = indexed.Slot - outerWidth;
                if (!(source.Table.Indexes ?? []).Any(index => StatementExecution.IndexDependenciesUsable(index, source.Storage.Catalog) && index.Kind == 1 && index.Keys.Any(key => key.Column == column))) continue;
                var args = term.Args.ToArray(); args[side] = indexed with { Slot = column };
                args[1 - side] = new("constant", query.Type, SqlValue.Null(query.Type), []);
                if (GinQuery.Match(term with { Args = args }, column, indexed.Type) is null) continue;
                if (!terms.Contains(term)) terms.Add(term);
                sibling = true; break;
            }
        }
        return sibling ? new(source, terms.ToArray(), outerWidth) : null;
    }

    internal long EstimateWork(long outerRows, Resolved? residual)
    {
        var table = Source.Table; var tree = Source.Tree;
        var nodes = residual is null ? 0 : PredicatePlan.Nodes(residual);
        var sourceNodes = Source.Filter is null ? 0 : PredicatePlan.Nodes(Source.Filter);
        long best = long.MaxValue;
        foreach (var term in Bounds)
        {
            if (term.Args.Length != 2) continue;
            var column = term.Args.FirstOrDefault(arg => arg.Op == "column" && arg.Slot >= OuterWidth);
            if (column is null) continue;
            var slot = column.Slot - OuterWidth;
            var matching = term.Op == "open_predicate" || column.Type.Kind is SqlKind.Array or SqlKind.Jsonb;
            var rows = Estimate.Fraction(table.RowCount, matching ? "matching" : term.Op == "=" ? "equality" : "inequality");
            if (table.PrimaryKey.Length != 0 && table.PrimaryKey[0] == slot && !matching)
            {
                if (table.PrimaryKey.Length == 1 && term.Op == "=") rows = Math.Min(rows, 1);
                var pages = rows == 0 ? 0 : Math.Clamp(Estimate.Fraction(tree.NodeCount, term.Op == "=" ? "equality" : "inequality"), tree.Height, tree.NodeCount);
                best = Math.Min(best, Estimate.Add(pages, Estimate.Multiply(rows, 1 + nodes + sourceNodes)));
            }
            foreach (var index in table.Indexes ?? [])
            {
                if (index.Keys.Length == 0 || index.Keys[0].Column != slot || !StatementExecution.IndexDependenciesUsable(index, Source.Storage.Catalog)) continue;
                var indexTree = Source.Storage.Database.IndexTree(index, Source.Storage.Pending, Source.Storage.Snapshot);
                var height = indexTree.Height; var count = indexTree.NodeCount;
                if (index.Kind == 2 && index.Gist?.Root is { } root)
                {
                    height = 1; for (var node = root; !node.Leaf; node = node.Entries[0].Child!) height++;
                    count = root.Count;
                }
                var pages = rows == 0 ? 0 : Math.Clamp(Estimate.Fraction(count, matching ? "matching" : term.Op == "=" ? "equality" : "inequality"), height, count);
                var extra = index.Kind == 1 ? rows : index.Kind == 2 ? Math.Max(0, height - 1) : 0;
                best = Math.Min(best, Estimate.Add(Estimate.Add(pages, extra), Estimate.Multiply(rows, tree.Height + 1L + nodes + sourceNodes)));
            }
        }
        return Estimate.Multiply(outerRows, best);
    }

    internal async IAsyncEnumerable<SqlValue[]> Rows(SqlValue[] outer, Meter meter,
        [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        var terms = new List<Resolved>();
        foreach (var term in Bounds)
        {
            var args = new Resolved[2];
            for (var i = 0; i < args.Length; i++)
            {
                var arg = term.Args[i];
                if (arg.Op == "column" && arg.Slot < OuterWidth)
                {
                    var value = await outer[arg.Slot].ResolveAsync(cancellationToken).ConfigureAwait(false);
                    args[i] = new("constant", arg.Type, value, [], Collation: arg.Collation);
                }
                else args[i] = arg.Op == "column" ? arg with { Slot = arg.Slot - OuterWidth } : arg;
            }
            terms.Add(term with { Args = args });
        }
        var filter = PredicatePlan.Combine(terms);
        var access = AccessPlan.Select(Source.Table, Source.Tree, filter, Source.Storage, meter);
        await foreach (var row in access.Rows(Source.Table, Source.Tree, Source.Storage, meter, null, cancellationToken).ConfigureAwait(false))
            if (await QueryExecution.KeepsAsync(Source.Filter, row, meter).ConfigureAwait(false)) yield return row;
    }
}
