namespace Jed;

internal static class KeySetEstimate
{
    internal static long Rows(CatalogTable table, Catalog catalog, Resolved filter, Resolved[] keys, bool unique, long population)
    {
        var terms = PredicatePlan.Conjuncts(filter).ToArray(); var rows = population;
        long probes = 1; var consumed = 0; var allPoints = true;
        foreach (var key in keys)
        {
            var sets = terms.Select(term => (Term: term, Expanded: Expand(term, key))).Where(item => item.Expanded is not null).ToArray();
            var clips = terms.Where(term => Comparison(term, key)).ToArray();
            if (sets.Length == 0 && clips.Length == 0) break;
            var alternatives = sets.FirstOrDefault().Expanded;
            var count = alternatives?.Length ?? 1;
            if (count == 0 || Contradiction.Proves(PredicatePlan.Combine(clips) ?? new Resolved("constant", SqlType.Boolean, SqlValue.Boolean(true), []))) return 0;
            if (consumed != 0 && count > 1 && probes > 65536 / count) break;
            probes = Estimate.Multiply(probes, count); consumed++;
            var points = alternatives is null ? clips.Any(term => term.Op == "=") : alternatives.All(term => PredicatePlan.Conjuncts(term).All(part => part.Op == "="));
            if (alternatives is not null)
            {
                long sum = 0;
                foreach (var alternative in alternatives)
                    sum = Math.Min(rows, Estimate.Add(sum, unique && keys.Length == 1 && points
                        ? Math.Min(1, MemberRows(table, catalog, alternative, rows)) : MemberRows(table, catalog, alternative, rows)));
                rows = sum;
            }
            if (clips.Length != 0) rows = MemberRows(table, catalog, PredicatePlan.Combine(clips)!, rows);
            allPoints &= points;
            if (!points) break;
        }
        return unique && allPoints && consumed == keys.Length ? Math.Min(rows, probes) : rows;
    }
    internal static Resolved[]? Expand(Resolved term, Resolved key)
    {
        if (IndexSetAccess.Expand(term, key) is { } known) return known;
        if (term.Args.Length == 0 || !Resolve.Equivalent(term.Args[0], key) ||
            !Estimate.ArrayMembership(term, out var elements, out var complement, out var nullArray) || complement) return null;
        if (nullArray) return [];
        if (!elements.Any(element => element.ParameterUnbound)) return null;
        if (elements.Any(element => element.Type != key.Type && !AccessConstant.NumericFamilies(element.Type, key.Type))) return null;
        return elements.Select(element => new Resolved("=", SqlType.Boolean, default, [key, element], Collation: term.Collation)).ToArray();
    }
    private static long MemberRows(CatalogTable table, Catalog catalog, Resolved predicate, long population)
    {
        if (Contradiction.Proves(predicate)) return 0;
        var equality = PredicatePlan.Conjuncts(predicate).FirstOrDefault(term => term.Op == "=");
        return Estimate.Rows(table, catalog, equality ?? predicate, population);
    }
    private static bool Comparison(Resolved term, Resolved key) => term.Op is "=" or "<" or "<=" or ">" or ">=" && term.Args.Length == 2 &&
        (Resolve.Equivalent(term.Args[0], key) && term.Args[1].Op == "constant" || Resolve.Equivalent(term.Args[1], key) && term.Args[0].Op == "constant");
}

internal static class AccessConstant
{
    internal static bool IntegerLiteralOrNull(Resolved expression) =>
        expression.Op == "constant" && !expression.ParameterUnbound && (expression.Type.IsInteger() || expression.Constant.IsNull) ||
        expression.Op is "promote" or "cast" && Try(expression.Args[0], out var value) && value.IsNull;
    internal static bool NumericFamilies(SqlType left, SqlType right) =>
        (left.IsInteger() || left == SqlType.Decimal) && (right.IsInteger() || right == SqlType.Decimal);
    internal static bool Compatible(Resolved value, SqlType key) => value.Type == key || value.Type.IsInteger() && key.IsInteger() || value.BoundSource && NumericFamilies(value.Type, key);
    internal static bool Point(SqlValue value, SqlType type, out SqlValue? point)
    {
        point = null;
        if (value.IsNull) return true;
        if (value.Type != type && !((value.Type.IsInteger() || value.Type == SqlType.Decimal) && (type.IsInteger() || type == SqlType.Decimal))) return false;
        try
        {
            var converted = Resolve.Cast(value, type);
            if (value.Type == SqlType.Decimal && type.IsInteger() && value.AsDecimal().CompareTo(Resolve.Cast(converted, SqlType.Decimal).AsDecimal()) != 0) return true;
            point = converted; return true;
        }
        catch (SqlException error) when (error.SqlState == SqlStates.NumericValueOutOfRange) { return true; }
    }
    internal static bool Try(Resolved expression, out SqlValue value)
    {
        if (expression.Op == "constant" && !expression.ParameterUnbound) { value = expression.Constant; return true; }
        if (expression.Op == "promote" && Try(expression.Args[0], out var inner))
        { value = Resolve.Cast(inner, expression.Type); return true; }
        if (expression.Op == "cast" && Try(expression.Args[0], out var operand) && operand.IsNull)
        { value = SqlValue.Null(expression.Type); return true; }
        value = default; return false;
    }
}
