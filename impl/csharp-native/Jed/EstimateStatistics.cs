namespace Jed;

internal static partial class Estimate
{
    internal static long Rows(CatalogTable table, Catalog catalog, Resolved? predicate) =>
        StatisticalRows(table.RowCount, table, catalog, predicate);
    internal static long Rows(CatalogTable table, Catalog catalog, Resolved? predicate, long population) =>
        StatisticalRows(population, table, catalog, predicate);

    internal static long JoinRows(PreparedRelation left, PreparedRelation right, Resolved? predicate)
    {
        var a = left.EstimatedRows ?? left.Plan?.EstimatedRows ?? 0;
        var b = right.EstimatedRows ?? right.Plan?.EstimatedRows ?? 0;
        var population = Multiply(a, b); var rows = population;
        if (predicate is null) return rows;
        foreach (var term in PredicatePlan.Conjuncts(predicate))
        {
            var refined = false;
            if (term.Op == "=" && left.BaseAccess is { } first && right.BaseAccess is { } second)
            {
                var x = term.Args[0]; var y = term.Args[1];
                if (x.Op == "column" && y.Op == "column" && x.Slot >= left.Columns.Length) (x, y) = (y, x);
                if (x.Op == "column" && y.Op == "column" && (uint)x.Slot < first.Table.Columns.Length &&
                    y.Slot >= left.Columns.Length && (uint)(y.Slot - left.Columns.Length) < second.Table.Columns.Length &&
                    ColumnStatistics.DistributionEligible(x.Type) && ColumnStatistics.DistributionEligible(y.Type) &&
                    StatisticsView.Find(first.Storage.Catalog, first.Table, x.Slot) is { } xs &&
                    StatisticsView.Find(second.Storage.Catalog, second.Table, y.Slot - left.Columns.Length) is { } ys)
                {
                    var nonnullA = first.Table.RowCount == 0 ? 0 : Scale(a, xs.NonnullRows, first.Table.RowCount);
                    var nonnullB = second.Table.RowCount == 0 ? 0 : Scale(b, ys.NonnullRows, second.Table.RowCount);
                    var ndv = Math.Max(1, Math.Max(xs.DistinctValues, ys.DistinctValues));
                    var matches = Scale(nonnullA, nonnullB, ndv);
                    rows = population == 0 ? 0 : Math.Min(rows, Scale(rows, matches, population)); refined = true;
                }
            }
            if (!refined) rows = Rows(rows, term);
        }
        return rows;
    }

    private static long StatisticalRows(long count, CatalogTable table, Catalog catalog, Resolved? predicate)
    {
        if (count == 0 || predicate is null) return count;
        if (Contradiction.Proves(predicate)) return 0;
        if (predicate.Op == "and")
        {
            var terms = PredicatePlan.Conjuncts(predicate).ToArray(); var used = new bool[terms.Length];
            for (var i = 0; i < terms.Length; i++)
            {
                if (used[i]) continue;
                if (RangeOperand(terms[i]) is { } structural && Comparison(terms[i], table, catalog) is null or { Unknown: true })
                {
                    var partner = -1;
                    for (var j = i + 1; j < terms.Length; j++)
                        if (!used[j] && RangeOperand(terms[j]) is { } other && structural.Lower != other.Lower && Resolve.Equivalent(structural.Value, other.Value))
                        { partner = j; break; }
                    if (partner >= 0) { used[partner] = true; count = Fraction(count, "paired_range"); continue; }
                }
                if (Comparison(terms[i], table, catalog) is { } first && first.Op is "<" or "<=" or ">" or ">=" && !first.Unknown)
                {
                    var paired = false;
                    for (var j = i + 1; j < terms.Length; j++)
                    {
                        if (used[j] || Comparison(terms[j], table, catalog) is not { } second || second.Unknown || first.Column != second.Column ||
                            second.Op is not ("<" or "<=" or ">" or ">=") || (first.Op is ">" or ">=") == (second.Op is ">" or ">=")) continue;
                        var lower = first.Op is ">" or ">=" ? first : second;
                        var upper = first.Op is "<" or "<=" ? first : second;
                        count = Scale(count, Range(first.Statistics, first.Derivation, lower, upper), table.RowCount);
                        used[j] = true; paired = true; break;
                    }
                    if (paired) continue;
                }
                count = StatisticalRows(count, table, catalog, terms[i]);
            }
            return count;
        }
        if (predicate.Op == "or") return Math.Min(count, Add(StatisticalRows(count, table, catalog, predicate.Args[0]), StatisticalRows(count, table, catalog, predicate.Args[1])));
        if (predicate.Op == "not")
        {
            if (predicate.Args[0].Op == "in_list" && predicate.Args[0].Args.Skip(1).Any(value => value.Op == "constant" && !value.ParameterUnbound && value.Constant.IsNull)) return 0;
            var population = NonnullPopulation(predicate.Args[0], table, catalog) ?? table.RowCount;
            return Scale(count, Math.Max(0, population - StatisticalRows(table.RowCount, table, catalog, predicate.Args[0])), table.RowCount);
        }
        long? result = null;
        if (predicate.Op is "is_null" or "is_not_null" && Column(predicate.Args[0], table, catalog) is { } nullColumn)
            result = predicate.Op == "is_null" ? nullColumn.Statistics.NullRows : nullColumn.Statistics.NonnullRows;
        else if (Comparison(predicate, table, catalog) is { } comparison)
            result = CompareRows(comparison);
        else if (predicate.Op == "column" && predicate.Type == SqlType.Boolean && Column(predicate, table, catalog) is { } boolean)
            result = Equality(boolean.Statistics, boolean.Derivation, SqlValue.Boolean(true), false);
        else if (predicate.Op == "in_list" && Column(predicate.Args[0], table, catalog) is { } listColumn)
            result = Membership(listColumn.Statistics, listColumn.Derivation, predicate.Args.Skip(1), listColumn.Type, false);
        else if (ArrayMembership(predicate, out var elements, out var complement, out var nullArray) && Column(predicate.Args[0], table, catalog) is { } arrayColumn)
        {
            result = nullArray ? 0 : Membership(arrayColumn.Statistics, arrayColumn.Derivation, elements, arrayColumn.Type, complement);
        }
        if (result is null && predicate.Op is "open_predicate" or "json_binary" or "json_path_binary" or "quantified")
        {
            foreach (var slot in PredicatePlan.Slots(predicate).Order())
            {
                if ((uint)slot >= table.Columns.Length || StatisticsView.Find(catalog, table, slot) is not { } statistics) continue;
                if (predicate.Op == "open_predicate" && predicate.Text == "=") continue;
                var arms = GinQuery.Match(predicate, slot, table.Columns[slot].Type.Type);
                if (arms is null || arms.Any(arm => arm.Unbound)) continue;
                result = arms.Aggregate(0L, (rows, arm) => Math.Min(statistics.NonnullRows, Add(rows, GinAccess.EstimateNode(arm, statistics).Rows))); break;
            }
        }
        return result is { } rows ? table.RowCount == 0 ? 0 : Math.Min(count, Scale(count, rows, table.RowCount)) : Rows(count, predicate);
    }
    private sealed record ColumnFact(int Slot, SqlType Type, StatisticsView Statistics, CollationDerivation? Derivation);
    private sealed record ComparisonFact(int Column, string Op, SqlValue Value, bool Unknown, StatisticsView Statistics, CollationDerivation? Derivation);
    private static ColumnFact? Column(Resolved expression, CatalogTable table, Catalog catalog)
    {
        if (expression.Op != "column" || (uint)expression.Slot >= table.Columns.Length) return null;
        var column = table.Columns[expression.Slot];
        if (!TableAccess.Compatible(expression, column) || StatisticsView.Find(catalog, table, expression.Slot) is not { } facts) return null;
        return new(expression.Slot, column.Type.Type, facts, expression.Collation);
    }
    private static ComparisonFact? Comparison(Resolved expression, CatalogTable table, Catalog catalog)
    {
        if (expression.Op is not ("=" or "<>" or "!=" or "<" or "<=" or ">" or ">=") || expression.Args.Length != 2) return null;
        var left = expression.Args[0]; var right = expression.Args[1]; var op = expression.Op;
        if (left.Op != "column" && right.Op == "column") { (left, right) = (right, left); op = op switch { "<" => ">", "<=" => ">=", ">" => "<", ">=" => "<=", _ => op }; }
        if (Column(left, table, catalog) is not { } column || !ColumnStatistics.DistributionEligible(column.Type)) return null;
        if (right.ParameterUnbound) return new(column.Slot, op, SqlValue.Null(column.Type), true, column.Statistics, expression.Collation);
        if (!GinQuery.Value(right, out var value)) return null;
        try { value = Resolve.Cast(value, column.Type); } catch (SqlException) { return null; }
        return new(column.Slot, op, value, false, column.Statistics, expression.Collation);
    }
    private static long? NonnullPopulation(Resolved predicate, CatalogTable table, Catalog catalog)
    {
        if (predicate.Op is "is_null" or "is_not_null" or "distinct" or "not_distinct") return null;
        var slots = PredicatePlan.Slots(predicate);
        return slots.Count == 1 && StatisticsView.Find(catalog, table, slots.Single()) is { } facts ? facts.NonnullRows : null;
    }
    private static long Equality(StatisticsView statistics, CollationDerivation? derivation, SqlValue value, bool unknown)
    {
        if (unknown) return Scale(statistics.NonnullRows, 1, Math.Max(1, statistics.DistinctValues));
        if (value.IsNull) return 0;
        for (var i = 0; i < statistics.Mcv.Length; i++)
            if (CollationRules.Compare(StatisticsView.Value(statistics.Mcv[i], value.Type), value, derivation) == 0) return statistics.McvRows[i];
        var remaining = Scale(statistics.HistogramRows, 1, Math.Max(1, statistics.DistinctValues - statistics.Mcv.Length));
        return statistics.FreshComplete || statistics.NonnullRows == 0 ? remaining : Math.Max(1, remaining);
    }
    private static long CompareRows(ComparisonFact comparison)
    {
        var facts = comparison.Statistics;
        if (!comparison.Unknown && comparison.Value.IsNull) return 0;
        var equal = Equality(facts, comparison.Derivation, comparison.Value, comparison.Unknown);
        return comparison.Op switch
        {
            "=" => equal, "<>" or "!=" => facts.NonnullRows - equal,
            "<" or "<=" => comparison.Unknown ? Fraction(facts.NonnullRows, "inequality") : Cdf(facts, comparison.Derivation, comparison.Value, comparison.Op == "<="),
            ">" or ">=" => comparison.Unknown ? Fraction(facts.NonnullRows, "inequality") : facts.NonnullRows - Cdf(facts, comparison.Derivation, comparison.Value, comparison.Op == ">"),
            _ => throw new InvalidOperationException("unknown statistics comparison")
        };
    }
    private static long Cdf(StatisticsView facts, CollationDerivation? derivation, SqlValue value, bool inclusive)
    {
        long rows = 0;
        for (var i = 0; i < facts.Mcv.Length; i++)
        {
            var order = CollationRules.Compare(StatisticsView.Value(facts.Mcv[i], value.Type), value, derivation);
            if (order < 0 || inclusive && order == 0) rows = Add(rows, facts.McvRows[i]);
        }
        return Math.Min(facts.NonnullRows, Add(rows, HistogramCdf(facts, derivation, value, inclusive)));
    }
    private static long HistogramCdf(StatisticsView facts, CollationDerivation? derivation, SqlValue value, bool inclusive)
    {
        if (facts.Histogram.Length < 2) return Fraction(facts.HistogramRows, "inequality");
        return Scale(facts.HistogramRows, HistogramOrdinal(facts, derivation, value, inclusive), facts.Histogram.Length - 1);
    }
    private static int HistogramOrdinal(StatisticsView facts, CollationDerivation? derivation, SqlValue value, bool inclusive)
    {
        var ordinal = 0;
        foreach (var bound in facts.Histogram)
        {
            var order = CollationRules.Compare(StatisticsView.Value(bound, value.Type), value, derivation);
            if (order > 0 || !inclusive && order == 0) break;
            ordinal++;
        }
        return Math.Min(ordinal, facts.Histogram.Length - 1);
    }
    private static long Range(StatisticsView facts, CollationDerivation? derivation, ComparisonFact lower, ComparisonFact upper)
    {
        if (lower.Value.IsNull || upper.Value.IsNull) return 0;
        long rows = 0;
        for (var i = 0; i < facts.Mcv.Length; i++)
        {
            var value = StatisticsView.Value(facts.Mcv[i], lower.Value.Type);
            var a = CollationRules.Compare(value, lower.Value, derivation); var b = CollationRules.Compare(value, upper.Value, derivation);
            if ((a > 0 || a == 0 && lower.Op == ">=") && (b < 0 || b == 0 && upper.Op == "<=")) rows = Add(rows, facts.McvRows[i]);
        }
        var residual = facts.Histogram.Length < 2 ? Fraction(facts.HistogramRows, "paired_range") :
            Scale(facts.HistogramRows, Math.Max(0, HistogramOrdinal(facts, derivation, upper.Value, upper.Op == "<=") -
                HistogramOrdinal(facts, derivation, lower.Value, lower.Op == ">")), facts.Histogram.Length - 1);
        return Math.Min(facts.NonnullRows, Add(rows, residual));
    }
    private static long Membership(StatisticsView facts, CollationDerivation? derivation, IEnumerable<Resolved> elements, SqlType type, bool complement)
    {
        var known = new List<SqlValue>(); long rows = 0;
        foreach (var element in elements)
        {
            if (element.ParameterUnbound) { rows = Math.Min(facts.NonnullRows, Add(rows, Equality(facts, derivation, default, true))); continue; }
            if (!GinQuery.Value(element, out var value)) { rows = Math.Min(facts.NonnullRows, Add(rows, Equality(facts, derivation, default, true))); continue; }
            if (value.IsNull) { if (complement) return 0; continue; }
            try
            {
                var converted = Resolve.Cast(value, type);
                if (value.Type.IsNumeric() && type.IsNumeric() && Evaluate.Compare(value, converted) != 0) continue;
                value = converted;
            }
            catch (SqlException) { continue; }
            if (known.Any(previous => CollationRules.Compare(previous, value, derivation) == 0)) continue;
            known.Add(value); rows = Math.Min(facts.NonnullRows, Add(rows, Equality(facts, derivation, value, false)));
        }
        return complement ? facts.NonnullRows - rows : rows;
    }
}
