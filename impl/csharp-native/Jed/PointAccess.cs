namespace Jed;

internal static class PointAccess
{
    internal static TableScan[]? Ranges(Resolved? filter, CatalogTable table)
    {
        if (filter is null || table.PrimaryKey.Length != 1) return null;
        var slot = table.PrimaryKey[0]; var type = table.Columns[slot].Type.Type;
        List<byte[]>? Points(Resolved expression)
        {
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
            if (expression.Op == "=")
            {
                if (expression.Args[0].Op == "column" && expression.Args[1].Op == "constant") { column = expression.Args[0]; values = [expression.Args[1].Constant]; }
                else if (expression.Args[1].Op == "column" && expression.Args[0].Op == "constant") { column = expression.Args[1]; values = [expression.Args[0].Constant]; }
            }
            else if (expression.Op == "in_init" || expression.Op == "quantified_init" && expression.Text == "= any")
            {
                if (expression.Subquery!.Initialized) { column = expression.Args[0]; values = expression.Subquery.Values; }
            }
            else if (expression.Op == "quantified" && expression.Text == "= any" && expression.Args[1].Op == "constant")
            {
                column = expression.Args[0]; var value = expression.Args[1].Constant;
                values = value.IsNull ? [] : value.AsArray().Values;
            }
            if (column?.Op != "column" || column.Slot != slot || values is null) return null;
            var result = new List<byte[]>();
            foreach (var value in values)
            {
                if (value.IsNull) continue;
                if (value.Type != type && !(value.Type.IsInteger() && type.IsInteger())) return null;
                try { result.Add(KeyEncoding.Encode(Resolve.Cast(value, type))); }
                catch (SqlException error) when (error.SqlState == SqlStates.NumericValueOutOfRange) { }
            }
            return result;
        }
        var points = Points(filter);
        if (points is null) return null;
        var bounds = TableAccess.Bounds(filter, table);
        if (bounds.Empty) return [];
        points.Sort((a, b) => a.AsSpan().SequenceCompareTo(b));
        var ranges = new List<TableScan>(); byte[]? previous = null;
        foreach (var point in points)
        {
            if (previous is not null && previous.AsSpan().SequenceEqual(point)) continue;
            previous = point;
            if (bounds.Lower is not null && (point.AsSpan().SequenceCompareTo(bounds.Lower) < 0 || !bounds.LowerInclusive && point.AsSpan().SequenceEqual(bounds.Lower))) continue;
            if (bounds.Upper is not null && (point.AsSpan().SequenceCompareTo(bounds.Upper) > 0 || !bounds.UpperInclusive && point.AsSpan().SequenceEqual(bounds.Upper))) continue;
            ranges.Add(new(point, true, point, true));
        }
        return ranges.ToArray();
    }
}
