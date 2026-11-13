using System.Runtime.CompilerServices;

namespace Jed;

// Merged USING names can alias raw island slots. A semantic fence receives its
// ordinary column layout, so its binary join never mistakes aliases for raw
// stored positions.
internal static class RelationLayout
{
    internal static PreparedRelation Normalize(PreparedRelation source, Meter meter, CancellationToken token)
    {
        if (!source.RawLayout) return source;
        Resolved Expand(Resolved value) => value.Op == "column" ? source.Columns[value.Slot].Projection! :
            value with { Args = value.Args.Select(Expand).ToArray() };
        int Find(int slot)
        {
            var found = Array.FindIndex(source.Columns, column => column.Projection is { Op: "column" } expression && expression.Slot == slot);
            if (found < 0) throw new InvalidOperationException("raw island column is absent from fence layout");
            return found;
        }
        Resolved Contract(Resolved value) => value with
        { Slot = value.Op == "column" ? Find(value.Slot) : value.Slot, Args = value.Args.Select(Contract).ToArray() };
        return source with
        {
            Columns = source.Columns.Select(column => column with { Projection = null }).ToArray(),
            Rows = Rows(source, meter, token), RawLayout = false, WithQuery = null, Bounded = null,
            WithTouched = source.WithTouched is null ? null : mask => Normalize(source.WithTouched(mask
                .SelectMany(slot => PredicatePlan.Slots(source.Columns[slot].Projection!)).ToHashSet()), meter, token),
            PushFilter = source.PushFilter is null ? null : filter =>
            {
                var pushed = source.PushFilter(Expand(filter));
                return (Normalize(pushed.Relation, meter, token), pushed.Residual is null ? null : Contract(pushed.Residual));
            }
        };
    }
    private static async IAsyncEnumerable<SqlValue[]> Rows(PreparedRelation source, Meter meter,
        [EnumeratorCancellation] CancellationToken token)
    {
        using var ownership = new QueryInputMemory(meter);
        await foreach (var raw in source.Rows.WithCancellation(token).ConfigureAwait(false))
        {
            if (source.MemoryOutput) ownership.Take(raw, true);
            var values = new SqlValue[source.Columns.Length];
            for (var slot = 0; slot < values.Length; slot++)
                values[slot] = await Evaluate.ExprAsync(source.Columns[slot].Projection!, meter, row: raw).ConfigureAwait(false);
            if (source.MemoryOutput)
            {
                ownership.Release(raw); ownership.Reserve(values); ownership.Transfer(values);
            }
            yield return values;
        }
    }
}
