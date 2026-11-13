using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record BaseRelationAccess(CatalogTable Table, StorageTree Tree, Resolved? Filter, QueryStorage Storage);

internal sealed record IndexNestedLoop(BaseRelationAccess Source, Resolved[] Bounds, int OuterWidth)
{
    internal AccessPlan? PinnedAccess { get; set; }
    internal Resolved? GenericPredicate() => PredicatePlan.Combine(Bounds.Select(Generic).Concat(Source.Filter is null ? [] : [Source.Filter]));
    private Resolved Generic(Resolved expression) => expression.Op == "column"
        ? expression.Slot >= OuterWidth ? expression with { Slot = expression.Slot - OuterWidth }
            : new("constant", expression.Type, SqlValue.Null(expression.Type), [], Collation: expression.Collation, ParameterUnbound: true, BoundSource: true)
        : expression with { Args = expression.Args.Select(Generic).ToArray() };
    internal long EstimateRows(long outerRows, Resolved? predicate, Meter meter)
    {
        var access = PinnedAccess ?? AccessPlan.Select(Source.Table, Source.Tree, GenericPredicate(), Source.Storage, meter);
        var inner = Estimate.Rows(Source.Table, Source.Storage.Catalog, Source.Filter, access.EstimatedRows);
        var residual = predicate is null ? null : PredicatePlan.Combine(PredicatePlan.Conjuncts(predicate).Where(term => !Bounds.Any(bound => Resolve.Equivalent(bound, term))));
        return Estimate.Rows(Estimate.Multiply(outerRows, inner), residual);
    }
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
            if (a.Type != b.Type && !AccessConstant.NumericFamilies(a.Type, b.Type) && !(term.Op == "open_predicate" && a.Type.Kind == SqlKind.Range && a.Type.Element == b.Type)) continue;
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
        var probe = new IndexNestedLoop(source, [], outerWidth);
        var keys = source.Table.PrimaryKey.Select(slot => new Resolved("column", source.Table.Columns[slot].Type.Type, default, [], Slot: slot))
            .Concat((source.Table.Indexes ?? []).Where(index => index.Kind == 0 && StatementExecution.IndexDependenciesUsable(index, source.Storage.Catalog))
                .SelectMany(index => StatementExecution.ResolveIndexKeys(source.Table, index, source.Storage.Catalog))).ToArray();
        foreach (var term in PredicatePlan.Conjuncts(predicate))
        {
            var generic = probe.Generic(term);
            if (!keys.Any(key => KeySetEstimate.Expand(generic, key) is not null)) continue;
            if (!terms.Contains(term)) terms.Add(term);
            sibling |= PredicatePlan.Slots(term).Any(slot => slot < outerWidth);
        }
        return sibling ? new(source, terms.ToArray(), outerWidth) : null;
    }

    internal long EstimateWork(long outerRows, Resolved? residual, Meter meter)
    {
        var nodes = residual is null ? 0 : PredicatePlan.Nodes(residual);
        var sourceNodes = Source.Filter is null ? 0 : PredicatePlan.Nodes(Source.Filter);
        var best = AccessPlan.Inventory(Source.Table, Source.Tree, GenericPredicate(), Source.Storage, meter).Min(access =>
        {
            var fetch = access.Kind is AccessKind.Btree or AccessKind.IndexIntervals or AccessKind.Gin or AccessKind.Gist ? Source.Tree.Height + 1L : 1;
            return Estimate.Add(Estimate.Add(access.EstimatedPages, access.EstimatedEntries), Estimate.Multiply(access.EstimatedRows, fetch + nodes + sourceNodes));
        });
        return Estimate.Multiply(outerRows, best);
    }

    internal ExplainNode? Observation { get; set; }
    internal IAsyncEnumerable<SqlValue[]> Rows(SqlValue[] outer, Meter meter, CancellationToken cancellationToken)
    {
        var source = Read(outer, meter, cancellationToken);
        return Observation is null ? source : ExplainObservation.Rows(source, Observation, meter, cancellationToken, inclusive: true);
    }
    private async IAsyncEnumerable<SqlValue[]> Read(SqlValue[] outer, Meter meter,
        [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        async ValueTask<Resolved> Bind(Resolved expression)
        {
            if (expression.Op == "column") return expression.Slot >= OuterWidth ? expression with { Slot = expression.Slot - OuterWidth } :
                new("constant", expression.Type, await outer[expression.Slot].ResolveAsync(cancellationToken).ConfigureAwait(false), [], Collation: expression.Collation, BoundSource: true);
            var args = new Resolved[expression.Args.Length];
            for (var i = 0; i < args.Length; i++) args[i] = await Bind(expression.Args[i]).ConfigureAwait(false);
            return expression with { Args = args };
        }
        var terms = new List<Resolved>();
        foreach (var term in Bounds) terms.Add(await Bind(term).ConfigureAwait(false));
        var filter = PredicatePlan.Combine(terms.Concat(Source.Filter is null ? [] : [Source.Filter]));
        var selected = PinnedAccess ?? AccessPlan.Select(Source.Table, Source.Tree, GenericPredicate(), Source.Storage, meter);
        var access = AccessPlan.Rebind(selected, Source.Table, Source.Tree, filter, Source.Storage, meter);
        await foreach (var row in access.Rows(Source.Table, Source.Tree, Source.Storage, meter, null, cancellationToken).ConfigureAwait(false))
            if (await QueryExecution.KeepsAsync(Source.Filter, row, meter).ConfigureAwait(false)) yield return row;
    }
}
