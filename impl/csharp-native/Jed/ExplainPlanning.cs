namespace Jed;

internal sealed record ExplainStages(ExplainNode Root, ExplainNode? Filter = null, ExplainNode? Aggregate = null,
    ExplainNode? Window = null, ExplainNode? Sort = null, ExplainNode? Distinct = null, ExplainNode? Limit = null);

internal static class ExplainPlanning
{
    internal static PreparedRelation Attach(PreparedRelation relation, ExplainNode node, Meter meter) => relation with
    {
        Plan = node, EstimatedRows = node.EstimatedRows,
        Rows = ExplainObservation.Rows(relation.Rows, node, meter, meter.Cancellation, inclusive: true)
    };
    internal static PreparedRelation Wrapper(PreparedRelation relation, string label, Meter meter)
    {
        if (meter.Explain is null) return relation;
        var child = relation.Plan ?? throw new InvalidOperationException("missing child EXPLAIN plan");
        return Attach(relation, new(label, "-", child.EstimatedRows, child.EstimatedCost, child), meter);
    }
    internal static PreparedRelation Function(PreparedRelation relation, string name, Resolved[] arguments, Meter meter)
    {
        if (meter.Explain is null) return relation;
        var rows = PlannerFacts.SrfRows;
        if (name == "generate_series" && arguments.All(argument => argument.Op == "constant"))
        {
            if (arguments.Any(argument => argument.Constant.IsNull)) rows = 0;
            else
            {
                var start = arguments[0].Constant.AsInteger(); var finish = arguments[1].Constant.AsInteger();
                var step = arguments.Length > 2 ? arguments[2].Constant.AsInteger() : 1;
                rows = step == 0 || step > 0 && start > finish || step < 0 && start < finish ? 0 :
                    (long)Int128.Min(long.MaxValue, ((Int128)finish - start) / step + 1);
            }
        }
        return Attach(relation, new("SRF " + name, "-", rows, Estimate.Add(rows, Expressions(arguments))), meter);
    }
    internal static long Expressions(IEnumerable<Resolved> values) => values.Aggregate(0L, (cost, value) => Estimate.Add(cost, Expression(value)));
    private static long Expression(Resolved value)
    {
        var cost = PredicatePlan.Nodes(value);
        foreach (var subquery in Subqueries(value).Where(cell => cell.IsCorrelated)) cost = Estimate.Add(cost, subquery.Plan?.EstimatedCost ?? 0);
        return cost;
    }
    private static IEnumerable<SubqueryCell> Subqueries(Resolved value)
    {
        if (value.Subquery is { } cell) yield return cell;
        foreach (var argument in value.Args) foreach (var nested in Subqueries(argument)) yield return nested;
    }
    internal static PreparedRelation Set(PreparedRelation relation, PreparedRelation left, PreparedRelation right, string operation, bool all, Meter meter)
    {
        if (meter.Explain is null) return relation;
        var a = left.Plan ?? throw new InvalidOperationException("missing left set plan");
        var b = right.Plan ?? throw new InvalidOperationException("missing right set plan");
        var rows = Estimate.Add(a.EstimatedRows, b.EstimatedRows);
        if (!all) rows = Groups(rows, relation.Columns.Length);
        if (operation == "intersect") rows = Math.Min(rows, Math.Min(a.EstimatedRows, b.EstimatedRows));
        if (operation == "except") rows = Math.Min(rows, Estimate.Fraction(a.EstimatedRows, "opaque"));
        return Attach(relation, new(operation switch { "union" => "Union", "intersect" => "Intersect", _ => "Except" },
            all ? "all" : "distinct", rows, Estimate.Add(a.EstimatedCost, b.EstimatedCost), a, b), meter);
    }
    internal static PreparedRelation Join(PreparedRelation relation, PreparedRelation left, PreparedRelation right,
        string kind, Resolved? predicate, HashJoinPlan? hash, IndexNestedLoop? inl, Meter meter)
    {
        if (meter.Explain is null) return relation;
        var a = left.Plan ?? throw new InvalidOperationException("missing left join plan");
        var b = right.Plan ?? throw new InvalidOperationException("missing right join plan");
        var product = Estimate.Multiply(a.EstimatedRows, b.EstimatedRows);
        var rows = Estimate.Rows(product, predicate);
        if (kind is "left" or "full") rows = Math.Max(rows, a.EstimatedRows);
        if (kind is "right" or "full") rows = Math.Max(rows, b.EstimatedRows);
        var work = Estimate.Multiply(product, predicate is null ? 0 : Expressions([predicate]));
        if (hash is not null) work = HashJoinPlan.Work(a.EstimatedRows, b.EstimatedRows, hash.Types, predicate);
        if (inl is not null)
        {
            if (b.Label.StartsWith("Scan ", StringComparison.Ordinal))
            {
                var bounds = inl.Bounds.Select(term => term with { Args = term.Args.Select(arg => arg.Op == "column"
                    ? arg.Slot >= inl.OuterWidth ? arg with { Slot = arg.Slot - inl.OuterWidth } : new Resolved("join_source", arg.Type, default, []) : arg).ToArray() });
                var touched = b.Detail.IndexOf("; touched=", StringComparison.Ordinal);
                b.Detail = "Index-nested-loop PK bound: " + Bound(PredicatePlan.Combine(bounds), inl.Source.Table) + (touched < 0 ? "" : b.Detail[touched..]);
            }
            work = inl.EstimateWork(a.EstimatedRows, predicate);
        }
        var cost = inl is null ? Estimate.Add(Estimate.Add(a.EstimatedCost, b.EstimatedCost), work) : Estimate.Add(a.EstimatedCost, work);
        var detail = kind + (hash is null ? "" : "; keys=" + hash.Types.Length);
        if (predicate is not null) detail += "; " + Predicate(predicate, meter.Explain.Verbose, "on");
        return Attach(relation, new(hash is null ? "Nested Loop" : "Hash Join", detail, rows, cost, a, b), meter);
    }
    internal static long Groups(long rows, int keys)
    {
        long count = 1; for (var i = 0; i < keys; i++) count = Estimate.Multiply(count, PlannerFacts.DistinctValues);
        return Math.Min(rows, count);
    }
    private static long Groups(long rows, IEnumerable<Resolved> keys, ExplainNode input)
    {
        long count = 1;
        foreach (var key in keys)
        {
            long distinct = PlannerFacts.DistinctValues;
            if (input.Table is { } table && input.Catalog is { } catalog && key.Op == "column" &&
                (uint)key.Slot < table.Columns.Length && ColumnStatistics.DistributionEligible(key.Type) &&
                StatisticsView.Find(catalog, table, key.Slot) is { } facts)
                distinct = Estimate.Add(facts.DistinctValues, facts.NullRows > 0 ? 1 : 0);
            count = Estimate.Multiply(count, distinct);
        }
        return Math.Min(rows, count);
    }
    internal static void Touched(ExplainNode? node, int count)
    {
        if (node is null || !node.Label.StartsWith("Scan ", StringComparison.Ordinal)) return;
        var parts = node.Detail.Split("; ").Where(part => !part.StartsWith("touched=", StringComparison.Ordinal)).ToList();
        if (count != 0) parts.Insert(1, "touched=" + count);
        node.Detail = string.Join("; ", parts);
    }
    internal static string Predicate(Resolved filter, bool verbose, string kind = "filter") => verbose
        ? kind + "=" + ExplainExpression.Print(filter) : (kind == "filter" ? "" : kind + ":") + "conjuncts=" + PredicatePlan.Conjuncts(filter).Count();
    internal static ExplainNode Scan(CatalogTable table, AccessPlan access, Resolved? filter, IReadOnlySet<int> touched,
        bool pushed, Meter meter, Catalog? catalog = null, int height = 1)
    {
        var detail = access.Kind switch
        {
            AccessKind.Primary => "PK bound: " + Bound(access.Predicate, table),
            AccessKind.PrimaryIntervals => "PK interval set: " + string.Join(", ", table.PrimaryKey.Select(slot => table.Columns[slot].Name)) + "; intervals=" + (access.Ranges?.Length ?? 0),
            AccessKind.Btree => (access.Predicate is null ? "Index scan: using " : "Index bound: using ") + access.Index!.Name,
            AccessKind.IndexIntervals => "Index interval set: using " + access.Index!.Name + "; intervals=" + (access.Ranges?.Length ?? 0),
            AccessKind.Gin => "GIN bound: using " + access.Index!.Name,
            AccessKind.Gist => "GiST bound: using " + access.Index!.Name,
            _ => "Full scan"
        };
        if (touched.Count != 0) detail += "; touched=" + touched.Count;
        var rows = access.EstimatedRows;
        var cost = Math.Max(0, access.EstimatedCost - Estimate.Multiply(rows, filter is null ? 0 : PredicatePlan.Nodes(filter)));
        if (pushed && filter is not null)
        {
            detail += "; " + Predicate(filter, meter.Explain!.Verbose, "filter").Replace("conjuncts=", "filter:conjuncts=", StringComparison.Ordinal);
            cost = Estimate.Add(cost, Estimate.Multiply(rows, Expressions([filter])));
            rows = Math.Min(rows, catalog is null ? Estimate.Rows(table.RowCount, filter) : Estimate.Rows(table, catalog, filter));
        }
        return new("Scan " + table.Name, detail, rows, cost) { LogicalRows = pushed ? catalog is null ? Estimate.Rows(table.RowCount, filter) : Estimate.Rows(table, catalog, filter) : table.RowCount, Table = table, Catalog = catalog, Access = access, ScanHeight = height };
    }
    private static string Bound(Resolved? expression, CatalogTable table)
    {
        if (expression is null) return "-";
        var bounds = new List<string>();
        foreach (var term in PredicatePlan.Conjuncts(expression))
        {
            if (term.Args.Length != 2) continue;
            var left = term.Args[0]; var right = term.Args[1]; var operation = term.Op;
            if (left.Op != "column" && right.Op == "column")
            {
                (left, right) = (right, left);
                operation = operation switch { "<" => ">", "<=" => ">=", ">" => "<", ">=" => "<=", _ => operation };
            }
            if (left.Op == "column") bounds.Add(table.Columns[left.Slot].Name + " " + operation + " " + (right.Op == "outer" ? "outer" : ExplainExpression.Print(right)));
        }
        return bounds.Count == 0 ? "-" : string.Join(" and ", bounds);
    }
    internal static ExplainStages Select(SelectPlan plan, ExplainNode input, Meter meter)
    {
        var node = input; ExplainNode? filter = null, aggregate = null, window = null, sort = null, distinct = null, limit = null;
        if (plan.Limit is { } bounded && plan.Group is null && !plan.Distinct && plan.Order is not { Length: > 0 } &&
            (plan.Windows is null || plan.WindowPrefix) && input.Access is { } access && input.EstimatedRows != 0)
        {
            var needed = Estimate.Add(plan.Offset, bounded);
            var output = plan.Filter is null ? input.EstimatedRows : input.Table is { } table && input.Catalog is { } catalog
                ? Estimate.Rows(table, catalog, plan.Filter) : Estimate.Rows(input.LogicalRows, plan.Filter);
            var pulled = output == 0 ? input.EstimatedRows : Math.Min(input.EstimatedRows, Estimate.Scale(input.EstimatedRows, needed, output));
            if (bounded == 0) pulled = 0;
            var pages = pulled == 0 ? 0 : access.Kind == AccessKind.Full
                ? Math.Min(access.EstimatedPages, Math.Max(input.ScanHeight, Estimate.Scale(access.EstimatedPages, pulled, input.EstimatedRows)))
                : access.EstimatedPages;
            var fetch = access.Kind is AccessKind.Btree or AccessKind.Gin or AccessKind.Gist ? input.ScanHeight + 1L : 1;
            input.EstimatedRows = pulled;
            input.EstimatedCost = Estimate.Add(Estimate.Add(pages, access.EstimatedEntries), Estimate.Multiply(pulled, fetch));
        }
        if (plan.Empty) foreach (var (_, child) in node.Traverse()) { child.EstimatedRows = 0; child.EstimatedCost = 0; child.LogicalRows = 0; }
        if (plan.Filter is { } predicate)
        {
            var rows = Math.Min(node.EstimatedRows, node.Table is { } table && node.Catalog is { } catalog ? Estimate.Rows(table, catalog, predicate) : Estimate.Rows(node.LogicalRows, predicate));
            filter = node = new("Filter", Predicate(predicate, meter.Explain!.Verbose) + (plan.Empty ? "; contradiction" : ""), rows,
                Estimate.Add(node.EstimatedCost, Estimate.Multiply(node.EstimatedRows, Expressions([predicate]))), node);
            var hidden = Subqueries(predicate).Where(cell => !cell.IsCorrelated).Distinct().ToArray();
            filter.HiddenActual = () => hidden.Aggregate(0L, (cost, cell) => Estimate.Add(cost, cell.Plan?.ActualCost ?? 0));
        }
        if (plan.Group is { } group)
        {
            static IEnumerable<Resolved> Arguments(AggregatePlan value)
            {
                if (value.Argument is { } argument) yield return argument;
                foreach (var extra in value.Extra ?? []) yield return extra;
            }
            var rows = group.Keys.Length == 0 ? 1 : Groups(node.EstimatedRows, group.Keys, input);
            var cost = Estimate.Add(node.EstimatedCost, Estimate.Multiply(node.EstimatedRows, Estimate.Add(group.Aggregates.Length,
                Expressions(group.Keys.Concat(group.Aggregates.SelectMany(Arguments))))));
            var detail = "groups=" + group.Keys.Length + " aggs=" + group.Aggregates.Length;
            if (group.Having is { } having)
            {
                detail += "; " + Predicate(having, meter.Explain!.Verbose, "having");
                cost = Estimate.Add(cost, Estimate.Multiply(rows, Expressions([having]))); rows = Estimate.Rows(rows, having);
            }
            aggregate = node = new("Aggregate", detail, rows, cost, node);
        }
        if (plan.Windows is { } windows)
            window = node = new("Window", "funcs=" + windows.Plans.Length, node.EstimatedRows,
                Estimate.Add(node.EstimatedCost, Estimate.Multiply(node.EstimatedRows, windows.Plans.Length)), node);
        if (plan.Distinct && plan.DistinctOnCount == 0)
            distinct = node = new("Distinct", "-", Groups(node.EstimatedRows, plan.Projections.Select(projection => projection.Expression), input),
                Estimate.Add(node.EstimatedCost, Estimate.Multiply(node.EstimatedRows, Expressions(plan.Projections.Select(projection => projection.Expression)))), node);
        if (plan.Order is { Length: > 0 } order)
        {
            var detail = "keys=" + order.Length;
            if (plan.Limit is { } top && !plan.Distinct) detail += ", top-k=" + Estimate.Add(top, plan.Offset);
            sort = node = new("Sort", detail, node.EstimatedRows, Estimate.Add(node.EstimatedCost,
                Estimate.Multiply(node.EstimatedRows, Expressions(order.Select(key => key.Expression)))), node);
        }
        if (plan.DistinctOnCount != 0)
            distinct = node = new("Distinct", "on=" + plan.DistinctOnCount, Groups(node.EstimatedRows, (plan.Order ?? []).Take(plan.DistinctOnCount).Select(key => key.Expression), input), node.EstimatedCost, node);
        if (plan.Limit is not null || plan.Offset != 0)
        {
            var rows = Math.Max(0, node.EstimatedRows - plan.Offset);
            if (plan.Limit is { } maximum) rows = Math.Min(rows, maximum);
            var detail = plan.Limit is { } count ? "limit=" + count : "";
            if (plan.Offset != 0) detail += (detail.Length == 0 ? "" : " ") + "offset=" + plan.Offset;
            limit = node = new("Limit", detail, rows, node.EstimatedCost, node);
        }
        var projected = plan.Distinct && plan.DistinctOnCount == 0 ? 0 : Expressions(plan.Projections.Select(projection => projection.Expression));
        node.EstimatedCost = Estimate.Add(node.EstimatedCost, Estimate.Multiply(node.EstimatedRows, Estimate.Add(projected, plan.ChargeRows ? 1 : 0)));
        var expressions = plan.Projections.Select(projection => projection.Expression)
            .Concat(plan.Filter is null ? [] : [plan.Filter]).Concat((plan.Order ?? []).Select(key => key.Expression))
            .Concat(plan.Group?.Keys ?? []).Concat(plan.Group?.Aggregates.SelectMany(item => new[] { item.Argument }.OfType<Resolved>().Concat(item.Extra ?? [])) ?? [])
            .Concat(plan.Group?.Having is { } havingExpression ? [havingExpression] : []);
        node.EstimatedCost = Estimate.Add(node.EstimatedCost, Uncorrelated(expressions));
        if (meter.Explain!.Verbose) node.Append("output=[" + string.Join(", ", plan.Projections.Select(projection => ExplainExpression.Print(projection.Expression))) + "]");
        return new(node, filter, aggregate, window, sort, distinct, limit);
    }
    internal static long Uncorrelated(IEnumerable<Resolved> expressions) => expressions.SelectMany(Subqueries).Where(cell => !cell.IsCorrelated).Distinct()
        .Aggregate(0L, (cost, cell) => Estimate.Add(cost, cell.Plan?.EstimatedCost ?? 0));
}
