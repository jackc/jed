namespace Jed;

internal static class PointAccess
{
    internal static TableScan[]? Ranges(Resolved? filter, CatalogTable table)
    {
        if (filter is null || table.PrimaryKey.Length == 0) return null;
        var prefixes = new List<byte[]> { Array.Empty<byte>() }; var consumed = 0; var hasSet = false;
        TableScan[]? final = null;
        foreach (var slot in table.PrimaryKey)
        {
            var memberTable = table with { PrimaryKey = [slot] };
            var points = Member(filter, memberTable, false);
            var memberHasSet = HasSet(filter, slot);
            var ranges = points ?? [TableAccess.Bounds(filter, memberTable)];
            if (ranges.Any(range => range.Empty)) ranges = [];
            if (ranges.Length == 0) return memberHasSet || hasSet ? [] : null;
            if (consumed != 0 && ranges.Length > 1 && prefixes.Count > 65536 / ranges.Length) break;
            if (points is null && ranges.Length == 1 && ranges[0].Lower is null && ranges[0].Upper is null) break;
            hasSet |= memberHasSet;
            if (ranges.All(KeyIntervals.Point))
            {
                prefixes = prefixes.SelectMany(prefix => ranges.Select(range => prefix.Concat(range.Lower!).ToArray())).ToList();
                consumed++; continue;
            }
            final = ranges; consumed++; break;
        }
        if (!hasSet) return null;
        var result = new List<TableScan>();
        foreach (var prefix in prefixes)
        {
            if (final is null)
            {
                result.Add(consumed == table.PrimaryKey.Length ? new(prefix, true, prefix, true) :
                    new(prefix.Length == 0 ? null : prefix, true, SecondaryAccess.Successor(prefix), false));
                continue;
            }
            foreach (var range in final)
            {
                var lower = range.Lower is null ? prefix.Length == 0 ? null : prefix : prefix.Concat(range.Lower).ToArray();
                var upper = range.Upper is null ? SecondaryAccess.Successor(prefix) : prefix.Concat(range.Upper).ToArray();
                var complete = consumed == table.PrimaryKey.Length;
                var lowerInclusive = range.LowerInclusive; var upperInclusive = range.Upper is not null && range.UpperInclusive;
                if (!complete && lower is not null && !lowerInclusive) { lower = SecondaryAccess.Successor(lower); lowerInclusive = true; if (lower is null) continue; }
                if (!complete && upper is not null && upperInclusive) { upper = SecondaryAccess.Successor(upper); upperInclusive = false; }
                result.Add(new(lower, lowerInclusive, upper, upperInclusive));
            }
        }
        return KeyIntervals.Canonical(result, table.LeafSchema().FixedKeyWidth != 0);
    }
    private static bool HasSet(Resolved expression, int slot) => expression.Op == "and" ? expression.Args.Any(arg => HasSet(arg, slot)) :
        expression.Op == "or" && PredicatePlan.Slots(expression).SetEquals([slot]) ||
        expression.Text != "not" && expression.Op is "in_list" or "in_init" or "quantified_init" or "quantified" && expression.Args[0].Op == "column" && expression.Args[0].Slot == slot;
    private static TableScan[]? Member(Resolved filter, CatalogTable table, bool adjacent)
    {
        var slot = table.PrimaryKey[0]; var type = table.Columns[slot].Type.Type;
        bool InvalidLeaf(Resolved expression)
        {
            if (expression.Op == "or") return expression.Args.Any(InvalidLeaf);
            return PredicatePlan.Conjuncts(expression).Count(term => term.Op == "quantified" && term.Text == "= any") > 1;
        }
        List<byte[]>? Points(Resolved expression)
        {
            if (InvalidLeaf(expression)) return null;
            if (expression.Op == "quantified" && expression.Args[1] is { Op: "array" } literalArray && literalArray.Type.Element != type && !(type.IsInteger() && literalArray.Args.All(AccessConstant.IntegerLiteralOrNull))) return null;
            if (expression.Op == "and")
            {
                var a = Points(expression.Args[0]); var b = Points(expression.Args[1]);
                if (a is null) return b;
                if (b is null) return a;
                var keys = b.Select(Convert.ToHexString).ToHashSet(StringComparer.Ordinal);
                return a.Where(key => keys.Contains(Convert.ToHexString(key))).ToList();
            }
            if (expression.Op == "or")
            {
                var a = Points(expression.Args[0]); var b = Points(expression.Args[1]);
                return a is null || b is null ? null : a.Concat(b).ToList();
            }
            Resolved? column = null; IEnumerable<SqlValue>? values = null;
            if (expression.Op == "in_list") { column = expression.Args[0]; values = expression.Args.Skip(1).Select(item => item.Constant); }
            else if (expression.Op == "=")
            {
                if (expression.Args[0].Op == "column" && expression.Args[1].Op == "constant") { column = expression.Args[0]; values = [expression.Args[1].Constant]; }
                else if (expression.Args[1].Op == "column" && expression.Args[0].Op == "constant") { column = expression.Args[1]; values = [expression.Args[0].Constant]; }
            }
            else if (expression.Op == "in_init" && expression.Text != "not" || expression.Op == "quantified_init" && expression.Text == "= any")
            {
                if (expression.Subquery!.Initialized) { column = expression.Args[0]; values = expression.Subquery.Values; }
            }
            else if (expression.Op == "quantified" && expression.Text == "= any" && expression.Args[1] is { Op: "array", Text: not "nested" } array && array.Args.All(arg => AccessConstant.Try(arg, out _)))
            { column = expression.Args[0]; values = array.Args.Select(arg => { AccessConstant.Try(arg, out var value); return value; }); }
            else if (expression.Op == "quantified" && expression.Text == "= any" && expression.Args[1].Op == "constant")
            {
                column = expression.Args[0]; var value = expression.Args[1].Constant;
                values = value.IsNull ? [] : value.AsArray().Values;
            }
            if (column?.Op != "column" || column.Slot != slot || values is null) return null;
            if (!TableAccess.Compatible(expression, table.Columns[slot])) return null;
            var result = new List<byte[]>();
            foreach (var value in values)
            {
                if (!AccessConstant.Point(value, type, out var point)) return null;
                if (point is not null) result.Add(KeyEncoding.Encode(point.Value, collation: table.Columns[slot].CollationData));
            }
            return result;
        }
        TableScan[]? Reduce(Resolved expression)
        {
            if (InvalidLeaf(expression)) return null;
            if (expression.Op is "or" or "and")
            {
                var left = Reduce(expression.Args[0]); var right = Reduce(expression.Args[1]);
                if (left is null || right is null) return null;
                return expression.Op == "or" ? KeyIntervals.Canonical(left.Concat(right), adjacent) :
                    KeyIntervals.Canonical(left.SelectMany(a => right.Select(b => KeyIntervals.Intersect(a, b))).OfType<TableScan>(), adjacent);
            }
            if (expression.Op is "in_list" or "in_init" or "quantified_init" or "quantified")
                return Points(expression)?.Select(key => new TableScan(key, true, key, true)).ToArray();
            if (expression.Op is not ("=" or "<" or "<=" or ">" or ">=")) return null;
            var column = expression.Args[0]; var value = expression.Args[1];
            if (column.Op == "constant") (column, value) = (value, column);
            if (column.Op != "column" || column.Slot != slot || value.Op != "constant" || !TableAccess.Compatible(expression, table.Columns[slot])) return null;
            if (value.Constant.IsNull) return [];
            if (value.Type != type && !(value.Type.IsInteger() && type.IsInteger())) return null;
            var range = TableAccess.Bounds(expression, table);
            return range.Empty ? [] : [range];
        }
        var bounds = TableAccess.Bounds(filter, table);
        var memberTerms = PredicatePlan.Conjuncts(filter).Where(term => PredicatePlan.Slots(term).SetEquals([slot])).ToArray();
        if (memberTerms.Any(term => HasSet(term, slot)) && PredicatePlan.Combine(memberTerms) is { } member && Reduce(member) is { } reduced)
            return bounds.Empty ? [] : KeyIntervals.Canonical(reduced.Select(range => KeyIntervals.Intersect(range, bounds)).OfType<TableScan>(), adjacent);
        var points = Points(filter);
        if (points is null)
        {
            foreach (var term in PredicatePlan.Conjuncts(filter))
                if (HasSet(term, slot) && Reduce(term) is { } intervals)
                    return bounds.Empty ? [] : KeyIntervals.Canonical(intervals.Select(range => KeyIntervals.Intersect(range, bounds)).OfType<TableScan>(), adjacent);
            return null;
        }
        if (bounds.Empty) return [];
        points.Sort((a, b) => a.AsSpan().SequenceCompareTo(b));
        var ranges = new List<TableScan>(); byte[]? previous = null;
        foreach (var point in points)
        {
            if (previous is not null && previous.AsSpan().SequenceEqual(point)) continue;
            previous = point;
            if (bounds.Lower is not null && (point.AsSpan().SequenceCompareTo(bounds.Lower) < 0 || !bounds.LowerInclusive && point.AsSpan().SequenceEqual(bounds.Lower))) continue;
            if (bounds.Upper is not null && (point.AsSpan().SequenceCompareTo(bounds.Upper) > 0 || !bounds.UpperInclusive && point.AsSpan().SequenceEqual(bounds.Upper))) continue;
            if (ranges.Count != 0 && adjacent && table.LeafSchema().FixedKeyWidth != 0 && SecondaryAccess.Successor(ranges[^1].Upper!) is { } next && next.AsSpan().SequenceEqual(point))
                ranges[^1] = ranges[^1] with { Upper = point };
            else ranges.Add(new(point, true, point, true));
        }
        return ranges.ToArray();
    }
}
