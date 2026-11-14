namespace Jed;

internal sealed class StorageDeferral(StorageRemapEntry[] entries, HashSet<uint> retained, uint migration)
{
    private readonly SortedDictionary<(uint Parent, ushort Slot), StorageRemapEntry> remap =
        new(entries.ToDictionary(entry => (entry.Parent, entry.Slot)));
    internal HashSet<uint> Retained { get; } = retained;
    internal uint Migration { get; } = migration;
    internal StorageRemapEntry[] Entries => remap.Values.ToArray();
    internal void Set(uint parent, ushort slot, StoragePageReference target) => remap[(parent, slot)] = new(parent, slot, target.Page, target.Tag);
}

internal sealed partial class StorageTree
{
    private static Node Replacement(Ref reference, Node original, Node changed, bool sameKeys) => changed with
    {
        Origin = reference.Page != 0 ? reference.Page : original.Origin,
        OriginTag = reference.Page != 0 ? reference.Tag : original.OriginTag,
        SameKeys = sameKeys && (reference.Page != 0 || original.SameKeys)
    };
    private static bool Retainable(Node node, uint migration) => !node.IsLeaf && node.Origin != 0 && node.SameKeys && node.Origin != migration;

    internal static StorageDeferral? PlanDeferral(IEnumerable<StorageTree> trees, StorageRecovered before, IReadOnlySet<uint> retiredPages)
    {
        var all = trees.ToArray();
        var migration = before.FreePages.Count != 0 && !before.FreePages.Contains(before.Meta.PageCount - 1)
            ? before.Meta.PageCount - 1 : 0;
        var retained = new HashSet<uint>(); var retired = new HashSet<uint>(retiredPages);
        var additions = new HashSet<(uint, ushort)>();
        bool Visit(Ref? reference)
        {
            if (reference?.Dirty is not { } node) return false;
            if (node.IsLeaf) return true;
            var moves = node.Children!.Select(Visit).ToArray();
            if (!Retainable(node, migration)) return true;
            retained.Add(node.Origin);
            for (var i = 0; i < moves.Length; i++) if (moves[i]) additions.Add((node.Origin, checked((ushort)i)));
            return false;
        }
        foreach (var tree in all) { retired.UnionWith(tree.retired); Visit(tree.root); }
        var entries = before.Meta.Remap.Where(entry => !retired.Contains(entry.Parent) || retained.Contains(entry.Parent)).ToArray();
        var keys = entries.Select(entry => (entry.Parent, entry.Slot)).ToHashSet(); keys.UnionWith(additions);
        return keys.Count <= StorageRemap.Capacity(before.Meta.PageSize, before.Encrypted is not null) ? new(entries, retained, migration) : null;
    }

    internal StorageTree FoldRemap(StorageRemapEntry[] entries)
    {
        var parents = entries.Select(entry => entry.Parent).ToHashSet(); var dead = new HashSet<uint>(retired);
        Ref? Fold(Ref? reference)
        {
            if (reference is null) return null;
            var node = reference.Dirty ?? (reference.Shape is { IsLeaf: false } shape ? ShapeNode(shape) : null);
            if (node is null || node.IsLeaf) return reference;
            var children = node.Children!.Select(child => Fold(child)!).ToArray();
            if (reference.Dirty is null && !parents.Contains(reference.Page) && children.SequenceEqual(node.Children)) return reference;
            if (reference.Page != 0) dead.Add(reference.Page);
            return new(Dirty: node with { Children = children, SameKeys = false });
        }
        return new(pager, schema, Fold(root), pageCount, dead);
    }
}
