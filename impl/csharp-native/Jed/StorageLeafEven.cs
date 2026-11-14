namespace Jed;

internal sealed partial class StorageTree
{
    // An overflowing leaf first redistributes with a sibling having at least
    // one eighth of its payload free. Append splits never inspect the left
    // sibling, preserving the specified packed right-edge shape.
    private async ValueTask<Node?> EvenLeaves(Node parent, int at, Split split,
        byte[] writtenKey, HashSet<uint> dead, CancellationToken token)
    {
        if (split.Nodes.Length != 2 || split.Nodes[0].Dirty is not { IsLeaf: true } left ||
            split.Nodes[1].Dirty is not { IsLeaf: true } right) return null;
        var rightEdge = right.Key(right.Count - 1).AsSpan().SequenceEqual(writtenKey);
        var limit = pager.PayloadCapacity - pager.PayloadCapacity / 8;
        foreach (var siblingAt in new[] { at + 1, at - 1 })
        {
            if (siblingAt < 0 || siblingAt >= parent.Children!.Length || siblingAt < at && rightEdge) continue;
            var siblingRef = parent.Children[siblingAt];
            var sibling = await Load(siblingRef, token).ConfigureAwait(false);
            if (!sibling.IsLeaf) throw StorageFormat.Corrupt("tree children have inconsistent heights");
            if (Payload(sibling) > limit) continue;
            var rows = siblingAt < at
                ? sibling.Records().Concat(left.Records()).Concat(right.Records()).ToArray()
                : left.Records().Concat(right.Records()).Concat(sibling.Records()).ToArray();
            var replacement = Divide(new(Rows: rows), -1, multiway: false);
            var first = Math.Min(at, siblingAt);
            if (replacement is null || replacement.Nodes.Length != 2 ||
                Payload(parent) - parent.Keys![first].Length + replacement.Keys[0].Length > pager.PayloadCapacity) continue;
            var children = (Ref[])parent.Children.Clone();
            var keys = (byte[][])parent.Keys!.Clone();
            children[first] = replacement.Nodes[0]; children[first + 1] = replacement.Nodes[1];
            keys[first] = replacement.Keys[0];
            if (siblingRef.Page != 0) dead.Add(siblingRef.Page);
            return new(Children: children, Keys: keys);
        }
        return null;
    }
}
