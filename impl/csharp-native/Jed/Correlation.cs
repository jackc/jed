namespace Jed;

internal sealed class OuterFrame(BoundColumn[] columns)
{
    internal BoundColumn[] Columns { get; } = columns;
    internal SqlValue[] Row { get; private set; } = [];
    internal long Version { get; private set; }
    private readonly List<HashSet<int>> captures = [];
    internal HashSet<int> StartCapture() { var slots = new HashSet<int>(); captures.Add(slots); return slots; }
    internal void EndCapture(HashSet<int> slots) => captures.Remove(slots);
    internal void Reference(int slot) { foreach (var capture in captures) capture.Add(slot); }
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
