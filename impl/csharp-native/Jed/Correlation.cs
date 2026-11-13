namespace Jed;

internal sealed class OuterFrame(BoundColumn[] columns)
{
    internal BoundColumn[] Columns { get; } = columns;
    internal SqlType[] RowTypes { get; } = RowShape(columns);
    private static SqlType[] RowShape(BoundColumn[] columns)
    {
        var types = new Dictionary<int, SqlType>();
        void Visit(Resolved expression)
        {
            if (expression.Op == "column") types[expression.Slot] = expression.Type;
            foreach (var child in expression.Args) Visit(child);
        }
        for (var slot = 0; slot < columns.Length; slot++)
            if (columns[slot].Projection is { } projection) Visit(projection); else types[slot] = columns[slot].Type;
        var result = Enumerable.Repeat(SqlType.Unknown, types.Count == 0 ? 0 : types.Keys.Max() + 1).ToArray();
        foreach (var (slot, type) in types) result[slot] = type;
        return result;
    }
    internal Resolved Lift(Resolved expression) => expression.Op == "column"
        ? new("outer", expression.Type, default, [], Outer: new(this, expression.Slot), Collation: expression.Collation)
        : expression with { Args = expression.Args.Select(Lift).ToArray() };
    internal SqlValue[] Row { get; private set; } = [];
    internal long Version { get; private set; }
    private readonly List<HashSet<int>> captures = [];
    internal HashSet<int> StartCapture() { var slots = new HashSet<int>(); captures.Add(slots); return slots; }
    internal void EndCapture(HashSet<int> slots) => captures.Remove(slots);
    internal HashSet<int> ReferencedSlots { get; } = [];
    internal void Reference(int slot) { ReferencedSlots.Add(slot); foreach (var capture in captures) capture.Add(slot); }
    internal void Assign(SqlValue[]? row) { Row = row ?? []; Version++; }
}
internal sealed record OuterBinding(OuterFrame Frame, int Slot)
{
    internal SqlValue Value => Frame.Row[Slot];
}

internal static class Correlation
{
    internal static HashSet<OuterFrame> Dependencies(Resolved expression)
    {
        var frames = new HashSet<OuterFrame>();
        void Visit(Resolved node)
        {
            if (node.Outer is { } outer) frames.Add(outer.Frame);
            if (node.Subquery is { } cell) frames.UnionWith(cell.Dependencies);
            foreach (var child in node.Args) Visit(child);
        }
        Visit(expression); return frames;
    }
    internal static HashSet<OuterFrame> Dependencies(SelectPlan plan)
    {
        var frames = new HashSet<OuterFrame>();
        _ = PlanTransform.Map(plan, expression => { frames.UnionWith(Dependencies(expression)); return expression; });
        return frames;
    }
}
