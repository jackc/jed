namespace Jed;

internal static class SyntaxIdentity
{
    internal static bool Same(Expression? a, Expression? b)
    {
        if (a is null || b is null) return a is null && b is null;
        if (a.Op != b.Op || a.Op != "bound" && a.Text != b.Text || a.Type != b.Type || !ReferenceEquals(a.Query, b.Query)) return false;
        if (a.Binding is { } binding && (b.Binding is null || !Resolve.Equivalent(binding, b.Binding))) return false;
        if (a.Binding is null && b.Binding is not null) return false;
        return Expressions(a.Args, b.Args) && Same(a.Predicate, b.Predicate) && Orders(a.AggregateOrder, b.AggregateOrder) && Windows(a.Window, b.Window);
    }
    private static bool Expressions(Expression[] a, Expression[] b) => a.Length == b.Length && a.Zip(b).All(pair => Same(pair.First, pair.Second));
    private static bool Orders(OrderSyntax[]? a, OrderSyntax[]? b) => (a ?? []).Length == (b ?? []).Length && (a ?? []).Zip(b ?? []).All(pair =>
        pair.First.Descending == pair.Second.Descending && pair.First.NullsFirst == pair.Second.NullsFirst && Same(pair.First.Expression, pair.Second.Expression));
    private static bool Windows(WindowSyntax? a, WindowSyntax? b)
    {
        if (a is null || b is null) return a is null && b is null;
        return a.Name == b.Name && a.Reference == b.Reference && Expressions(a.Partition, b.Partition) && Orders(a.Order, b.Order) && Frames(a.Frame, b.Frame);
    }
    private static bool Frames(WindowFrameSyntax? a, WindowFrameSyntax? b)
    {
        if (a is null || b is null) return a is null && b is null;
        return a.Mode == b.Mode && a.Exclude == b.Exclude && a.Start.Kind == b.Start.Kind && a.End.Kind == b.End.Kind && Same(a.Start.Offset, b.Start.Offset) && Same(a.End.Offset, b.End.Offset);
    }
}
