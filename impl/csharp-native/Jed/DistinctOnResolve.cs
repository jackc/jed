namespace Jed;

internal static class DistinctOnResolve
{
    internal static (SortPlan[] Order, int Count) Prepare(Expression[] keys, SortPlan[] order, Projection[] projections, BoundColumn[] columns)
    {
        var resolved = new List<Resolved>();
        foreach (var key in keys)
        {
            Resolved expression;
            if (key.Op == "integer")
            {
                if (!int.TryParse(key.Text, out var ordinal) || ordinal < 1 || ordinal > projections.Length)
                    throw new SqlException(SqlStates.InvalidColumnReference, "DISTINCT ON position outside select list");
                expression = projections[ordinal - 1].Expression;
            }
            else
            {
                var named = key.Op == "name" ? projections.Where(projection => projection.Name == key.Text).ToArray() : [];
                if (named.Length > 1 && named.Skip(1).Any(item => !Resolve.Equivalent(item.Expression, named[0].Expression)))
                    throw new SqlException(SqlStates.AmbiguousColumn, "ambiguous DISTINCT ON name");
                expression = named.Length > 0 ? named[0].Expression : Resolve.Expr(Resolve.Bind(key, columns));
            }
            Resolve.RequireOrderable(expression.Type); CollationRules.Require(expression, true);
            if (!resolved.Any(prior => Resolve.Equivalent(prior, expression))) resolved.Add(expression);
        }
        var used = new HashSet<int>(); var suffix = false; var count = 0; var effective = order.ToList();
        foreach (var item in order)
        {
            var identity = item.OutputIndex is { } output ? projections[output].Expression : item.Expression;
            var slot = resolved.FindIndex(key => Resolve.Equivalent(key, identity));
            if (slot < 0) { suffix = true; continue; }
            if (suffix) throw Invalid();
            used.Add(slot); count++;
        }
        for (var i = 0; i < resolved.Count; i++)
        {
            if (used.Contains(i)) continue;
            if (suffix) throw Invalid();
            effective.Add(new(resolved[i], false, false)); count++;
        }
        return (effective.ToArray(), count);
    }
    private static SqlException Invalid() => new(SqlStates.InvalidColumnReference, "DISTINCT ON expressions must match initial ORDER BY expressions");
}
