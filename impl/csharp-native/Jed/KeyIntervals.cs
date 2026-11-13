namespace Jed;

// Byte-ordered interval algebra shared by point and tuple access paths.
internal static class KeyIntervals
{
    internal static bool Point(TableScan range) => range.Lower is not null && range.Upper is not null &&
        range.LowerInclusive && range.UpperInclusive && range.Lower.AsSpan().SequenceEqual(range.Upper);
    internal static TableScan? Intersect(TableScan a, TableScan b)
    {
        var lower = a.Lower; var li = a.LowerInclusive;
        if (b.Lower is not null)
        {
            var order = lower is null ? -1 : lower.AsSpan().SequenceCompareTo(b.Lower);
            if (order < 0) { lower = b.Lower; li = b.LowerInclusive; }
            else if (order == 0) li &= b.LowerInclusive;
        }
        var upper = a.Upper; var ui = a.UpperInclusive;
        if (b.Upper is not null)
        {
            var order = upper is null ? 1 : upper.AsSpan().SequenceCompareTo(b.Upper);
            if (order > 0) { upper = b.Upper; ui = b.UpperInclusive; }
            else if (order == 0) ui &= b.UpperInclusive;
        }
        if (lower is not null && upper is not null)
        {
            var order = lower.AsSpan().SequenceCompareTo(upper);
            if (order > 0 || order == 0 && !(li && ui)) return null;
        }
        return new(lower, li, upper, ui);
    }
    internal static TableScan[] Canonical(IEnumerable<TableScan> source, bool adjacent)
    {
        var input = source.Where(range => !range.Empty).ToList();
        input.Sort((a, b) => a.Lower is null ? b.Lower is null ? 0 : -1 : b.Lower is null ? 1 :
            a.Lower.AsSpan().SequenceCompareTo(b.Lower) is var comparison && comparison != 0 ? comparison : b.LowerInclusive.CompareTo(a.LowerInclusive));
        var result = new List<TableScan>();
        foreach (var next in input)
        {
            if (result.Count == 0) { result.Add(next); continue; }
            var last = result[^1];
            var comparison = last.Upper is null || next.Lower is null ? 1 : last.Upper.AsSpan().SequenceCompareTo(next.Lower);
            var join = comparison > 0 || comparison == 0 && (last.UpperInclusive || next.LowerInclusive);
            if (!join && adjacent && last.UpperInclusive && next.LowerInclusive && last.Upper is not null && next.Lower is not null)
                join = SecondaryAccess.Successor(last.Upper) is { } successor && successor.AsSpan().SequenceEqual(next.Lower);
            if (!join) { result.Add(next); continue; }
            comparison = last.Upper is null ? 1 : next.Upper is null ? -1 : last.Upper.AsSpan().SequenceCompareTo(next.Upper);
            if (comparison < 0) result[^1] = last with { Upper = next.Upper, UpperInclusive = next.UpperInclusive };
            else if (comparison == 0) result[^1] = last with { UpperInclusive = last.UpperInclusive || next.UpperInclusive };
        }
        return result.ToArray();
    }
}
