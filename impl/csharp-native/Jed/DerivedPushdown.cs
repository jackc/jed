namespace Jed;

internal static class DerivedPushdown
{
    internal static (Resolved? Pushed, Resolved? Residual) Split(SelectPlan body, Resolved filter)
    {
        if (body.DistinctOnCount != 0) return (null, filter);
        Resolved? Output(int slot)
        {
            if ((uint)slot >= body.Projections.Length) return null;
            var value = body.Projections[slot].Expression;
            if (value.Op != "column") return null;
            if (body.Windows is { } windows && windows.Plans.Any(window => !window.Partition.Any(key => Resolve.Equivalent(key, value)))) return null;
            if (body.Group is not { } group) return value;
            if (group.Keys.Length == 0 || group.Sets is { Length: > 1 } || (uint)value.Slot >= group.Keys.Length) return null;
            var key = group.Keys[value.Slot];
            return key.Op == "column" ? key : null;
        }
        Resolved Rewrite(Resolved value) => value.Op == "column" ? Output(value.Slot)! : value with { Args = value.Args.Select(Rewrite).ToArray() };
        var pushed = new List<Resolved>(); var residual = new List<Resolved>();
        foreach (var term in PredicatePlan.Conjuncts(filter))
        {
            var slots = PredicatePlan.Slots(term);
            if (PredicatePlan.Safe(term) && slots.Count > 0 && slots.All(slot => Output(slot) is not null)) pushed.Add(Rewrite(term));
            else residual.Add(term);
        }
        return (PredicatePlan.Combine(pushed), PredicatePlan.Combine(residual));
    }
}
