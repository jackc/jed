namespace Jed;

internal sealed record TableScan(byte[]? Lower = null, bool LowerInclusive = true, byte[]? Upper = null,
    bool UpperInclusive = true, bool Reverse = false, bool Empty = false);

internal static class TableAccess
{
    internal static (SelectPlan Plan, TableScan Scan) Prepare(SelectPlan plan, CatalogTable table)
    {
        var scan = Bounds(plan.Filter, table);
        var order = plan.Order ?? [];
        if (plan.DistinctOnCount == 0 && plan.Group is null && order.Length > 0 && order.Length <= table.PrimaryKey.Length && (!order[0].Descending || order.Length == table.PrimaryKey.Length) &&
            order.Select((key, i) => key.Expression.Op == "column" && key.Expression.Slot == table.PrimaryKey[i] && key.Descending == order[0].Descending && Compatible(key.Expression, table.Columns[table.PrimaryKey[i]])).All(b => b))
        {
            scan = scan with { Reverse = order[0].Descending };
            plan = plan with { Order = [], StreamDistinct = plan.Distinct, WindowPrefix = WindowPrefix.Eligible(plan, table, order[0].Descending) };
        }
        return (plan, scan);
    }
    internal static TableScan Bounds(Resolved? filter, CatalogTable table)
    {
        if (filter is null || table.PrimaryKey.Length == 0) return new();
        var constraints = new List<Resolved>();
        void Visit(Resolved node)
        {
            if (node.Op == "and") { Visit(node.Args[0]); Visit(node.Args[1]); }
            else if (node.Op is "=" or "<" or "<=" or ">" or ">=") constraints.Add(node);
        }
        Visit(filter);
        var prefix = new WireWriter();
        for (var ordinal = 0; ordinal < table.PrimaryKey.Length; ordinal++)
        {
            var slot = table.PrimaryKey[ordinal]; var type = table.Columns[slot].Type.Type;
            byte[]? equality = null, lower = null, upper = null; var lowerInclusive = true; var upperInclusive = true;
            foreach (var node in constraints)
            {
                var column = node.Args[0]; var constant = node.Args[1]; var op = node.Op;
                if (column.Op == "constant" && constant.Op == "column")
                {
                    (column, constant) = (constant, column);
                    op = op switch { "<" => ">", "<=" => ">=", ">" => "<", ">=" => "<=", _ => op };
                }
                if (column.Op != "column" || column.Slot != slot || constant.Op != "constant") continue;
                if (!Compatible(node, table.Columns[slot])) continue;
                if (constant.ParameterUnbound) continue;
                if (constant.Constant.IsNull) return new(Empty: true);
                if (constant.Type != type && !(constant.Type.IsInteger() && type.IsInteger())) continue;
                byte[] key;
                try { key = KeyEncoding.Encode(Resolve.Cast(constant.Constant, type), collation: table.Columns[slot].CollationData); }
                catch (SqlException) { continue; }
                if (op == "=")
                {
                    if (equality is not null && !equality.AsSpan().SequenceEqual(key)) return new(Empty: true);
                    equality = key;
                }
                if (op is ">" or ">=")
                {
                    var compare = lower is null ? 1 : key.AsSpan().SequenceCompareTo(lower);
                    if (compare > 0 || compare == 0 && op == ">") { lower = key; lowerInclusive = op == ">="; }
                }
                if (op is "<" or "<=")
                {
                    var compare = upper is null ? -1 : key.AsSpan().SequenceCompareTo(upper);
                    if (compare < 0 || compare == 0 && op == "<") { upper = key; upperInclusive = op == "<="; }
                }
            }
            if (equality is not null)
            {
                if (lower is not null && (equality.AsSpan().SequenceCompareTo(lower) < 0 || !lowerInclusive && equality.AsSpan().SequenceEqual(lower)) ||
                    upper is not null && (equality.AsSpan().SequenceCompareTo(upper) > 0 || !upperInclusive && equality.AsSpan().SequenceEqual(upper))) return new(Empty: true);
                prefix.Bytes(equality); continue;
            }
            var before = prefix.ToArray();
            var low = lower is null ? before.Length == 0 ? null : before : before.Concat(lower).ToArray();
            var high = upper is null ? before.Length == 0 ? null : Successor(before) : before.Concat(upper).ToArray();
            var complete = ordinal == table.PrimaryKey.Length - 1;
            if (!complete && low is not null && lower is not null && !lowerInclusive) { low = Successor(low); lowerInclusive = true; if (low is null) return new(Empty: true); }
            if (!complete && high is not null && upper is not null && upperInclusive) { high = Successor(high); upperInclusive = false; }
            if (upper is null) upperInclusive = false;
            return new(low, lowerInclusive, high, upperInclusive);
        }
        var full = prefix.ToArray(); return new(full, true, full, true);
    }
    internal static (Resolved? Predicate, bool Unique) Predicate(Resolved? filter, CatalogTable table)
    {
        if (filter is null || table.PrimaryKey.Length == 0) return (null, false);
        var terms = PredicatePlan.Conjuncts(filter).ToArray(); var used = new List<Resolved>(); var equalities = 0;
        foreach (var slot in table.PrimaryKey)
        {
            var equality = false;
            foreach (var term in terms)
            {
                if (term.Op is not ("=" or "<" or "<=" or ">" or ">=") || term.Args.Length != 2) continue;
                var a = term.Args[0]; var b = term.Args[1]; if (a.Op == "constant") (a, b) = (b, a);
                if (a.Op != "column" || a.Slot != slot || b.Op != "constant" || !Compatible(term, table.Columns[slot]) ||
                    a.Type != b.Type && !(a.Type.IsInteger() && b.Type.IsInteger())) continue;
                used.Add(term); equality |= term.Op == "=";
            }
            if (!equality) break;
            equalities++;
        }
        return (PredicatePlan.Combine(used), equalities == table.PrimaryKey.Length);
    }
    internal static bool Compatible(Resolved expression, CatalogColumn column) => !CollationRules.Collatable(column.Type.Type) ||
        !column.CollationSkew && (expression.Collation?.Name ?? "C") == (column.Collation ?? "C");
    private static byte[]? Successor(byte[] prefix)
    {
        for (var i = prefix.Length - 1; i >= 0; i--)
            if (prefix[i] != 255) { var next = prefix[..(i + 1)]; next[i]++; return next; }
        return null;
    }
}
