namespace Jed;

internal sealed record ExplainCte(string Name, bool Materialized, ExplainNode Body);

// One object per physical plan position. Repeated labels and expression trees
// never alias execution observations across derived queries or CTE references.
internal sealed class ExplainNode(string label, string detail, long rows, long cost, params ExplainNode[] children)
{
    internal string Label { get; } = label;
    internal string Detail { get; set; } = detail.Length == 0 ? "-" : detail;
    internal long EstimatedRows { get; set; } = Math.Max(0, rows);
    internal long LogicalRows { get; set; } = Math.Max(0, rows);
    internal long EstimatedCost { get; set; } = Math.Max(0, cost);
    internal CatalogTable? Table { get; init; }
    internal Catalog? Catalog { get; init; }
    internal AccessPlan? Access { get; init; }
    internal int ScanHeight { get; init; }
    internal IReadOnlyList<ExplainNode> Children { get; } = children;
    private long measuredActual;
    private long localActual;
    internal Func<long>? HiddenActual { get; set; }
    internal long ActualCost => Math.Max(measuredActual, Children.Aggregate(Estimate.Add(localActual, HiddenActual?.Invoke() ?? 0),
        (cost, child) => Estimate.Add(cost, child.ActualCost)));
    internal void Attribute(long units) => measuredActual = Estimate.Add(measuredActual, Math.Max(0, units));
    internal void AttributeLocal(long units) => localActual = Estimate.Add(localActual, Math.Max(0, units));
    internal void SetInclusive(long units) => measuredActual = Math.Max(measuredActual, Math.Max(0, units));
    internal void Append(string attribute) => Detail = Detail == "-" ? attribute : Detail + "; " + attribute;
    internal IEnumerable<(int Depth, ExplainNode Node)> Traverse(int depth = 0)
    {
        yield return (depth, this);
        foreach (var child in Children)
            foreach (var row in child.Traverse(depth + 1)) yield return row;
    }
}

internal static class ExplainRows
{
    internal static Column[] Columns(ExplainSyntax syntax)
    {
        var columns = new List<Column> { new("depth", SqlType.I32), new("node", SqlType.Text), new("detail", SqlType.Text) };
        if (syntax.Costs) { columns.Add(new("est_rows", SqlType.I64)); columns.Add(new("est_cost", SqlType.I64)); }
        if (syntax.Analyze) columns.Add(new("actual_cost", SqlType.I64));
        if (syntax.Lane) columns.Add(new("lane", SqlType.Text));
        return columns.ToArray();
    }
    internal static IEnumerable<Row> Render(ExplainNode root, ExplainSyntax syntax, string lane, long actual = 0, long affected = 0)
    {
        if (syntax.Analyze)
        {
            var wrapper = new ExplainNode("Analyze", $"cost={actual} rows={affected}", root.EstimatedRows, root.EstimatedCost, root);
            wrapper.SetInclusive(actual); root = wrapper;
        }
        foreach (var (depth, node) in root.Traverse())
        {
            var cells = new List<SqlValue> { SqlValue.Integer(depth, SqlType.I32), SqlValue.Text(node.Label), SqlValue.Text(node.Detail) };
            if (syntax.Costs) { cells.Add(SqlValue.Integer(node.EstimatedRows)); cells.Add(SqlValue.Integer(node.EstimatedCost)); }
            if (syntax.Analyze) cells.Add(SqlValue.Integer(node.ActualCost));
            if (syntax.Lane) cells.Add(SqlValue.Text(lane));
            yield return new(cells.ToArray());
        }
    }
}
