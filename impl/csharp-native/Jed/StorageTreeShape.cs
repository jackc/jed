namespace Jed;

// Only interior separators and child identities are resident. Leaf contents stay
// in the bounded pager; immutable shapes can be shared by committed generations.
internal sealed class StorageTreeShape(uint page, ReadOnlyMemory<byte> tag,
    byte[][]? separators = null, StorageTreeShape[]? children = null)
{
    internal uint Page { get; } = page;
    internal ReadOnlyMemory<byte> Tag { get; } = tag;
    internal byte[][] Separators { get; } = separators ?? [];
    internal StorageTreeShape[] Children { get; } = children ?? [];
    internal bool IsLeaf => Children.Length == 0;
    internal int Height { get; } = children is { Length: > 0 } ? checked(children[0].Height + 1) : 1;
    internal long NodeCount { get; } = checked(1 + (children?.Sum(child => child.NodeCount) ?? 0));
}

internal sealed partial class StorageTree
{
    private static Node ShapeNode(StorageTreeShape shape) => new(Children: shape.Children.Select(child =>
        new Ref(child.Page, Tag: child.Tag, Shape: child)).ToArray(), Keys: shape.Separators);

    private static bool HasShape(Ref? reference)
    {
        if (reference is null) return true;
        if (reference.Dirty is { } node) return node.IsLeaf || node.Children!.All(HasShape);
        return reference.Shape is not null;
    }

    internal int Height => HeightOf(root);
    internal long NodeCount => NodesOf(root);
    private static int HeightOf(Ref? reference) => reference is null ? 0 : reference.Dirty is { } node
        ? node.IsLeaf ? 1 : checked(1 + HeightOf(node.Children![0]))
        : reference.Shape?.Height ?? throw new InvalidOperationException("tree shape was not retained at open");
    private static long NodesOf(Ref? reference) => reference is null ? 0 : reference.Dirty is { } node
        ? node.IsLeaf ? 1 : checked(1 + node.Children!.Sum(NodesOf))
        : reference.Shape?.NodeCount ?? throw new InvalidOperationException("tree shape was not retained at open");

    internal long CountRangeNodes(byte[]? lower, bool lowerInclusive, byte[]? upper, bool upperInclusive)
    {
        if (root is null || EmptyRange(lower, lowerInclusive, upper, upperInclusive)) return 0;
        long Count(Ref reference)
        {
            if (lower is null && upper is null) return NodesOf(reference);
            var node = reference.Dirty;
            if (node is null)
            {
                var shape = reference.Shape ?? throw new InvalidOperationException("tree shape was not retained at open");
                if (shape.IsLeaf) return 1;
                node = ShapeNode(shape);
            }
            if (node.IsLeaf) return 1;
            long count = 1;
            for (var i = 0; i < node.Children!.Length; i++)
                if (Intersects(node, i, lower, upper, upperInclusive)) count = checked(count + Count(node.Children[i]));
            return count;
        }
        return Count(root);
    }
}
