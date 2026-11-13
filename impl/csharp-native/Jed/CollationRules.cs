using System.Text;

namespace Jed;

internal sealed record CollationDerivation(string? Name = null, bool Explicit = false, bool Indeterminate = false, bool Conflict = false, Collation? Table = null)
{
    internal CollationDerivation Implicit() => Indeterminate || Conflict ? new(Conflict: true) : this with { Explicit = false };
}

internal static class CollationRules
{
    internal static void ChargeSort(SqlValue value, Resolved expression, Meter meter)
        => _ = Decorate(value, expression, meter);
    internal static SortValue Decorate(SqlValue value, Resolved expression, Meter? meter = null)
    {
        var table = Require(expression, true)?.Table;
        if (table is null || value.IsNull || !Collatable(value.Type))
        {
            var work = QueryExecution.KeyBytes(value);
            if (work != 0) { meter?.Charge(work * Costs.SortKey); meter?.Guard(); }
            return new(value, null);
        }
        var values = value.Type == SqlType.Text ? new[] { value } : value.AsArray().Values;
        foreach (var element in values)
        {
            if (element.IsNull) continue;
            meter?.Charge(element.AsText().EnumerateRunes().Count() * Costs.Collate); meter?.Guard();
        }
        return new(value, KeyEncoding.Encode(value, collation: table));
    }
    internal static bool Collatable(SqlType type) => type == SqlType.Text || type.Kind == SqlKind.Array && type.Element == SqlType.Text;
    internal static CollationDerivation? Merge(IEnumerable<Resolved> expressions) => Merge(expressions.Where(expression => Collatable(expression.Type)).Select(expression => expression.Collation));
    internal static CollationDerivation? Merge(IEnumerable<CollationDerivation?> derivations)
    {
        CollationDerivation? result = null; var explicitValues = new List<CollationDerivation>(); var implicitValues = new List<CollationDerivation>(); var indeterminate = false;
        foreach (var value in derivations)
        {
            if (value is null || value.Conflict) continue;
            if (value.Explicit) explicitValues.Add(value);
            else { indeterminate |= value.Indeterminate; if (value.Name is not null) implicitValues.Add(value); }
        }
        foreach (var value in explicitValues)
        {
            if (result is not null && result.Name != value.Name) throw new SqlException("42P21", "collation mismatch between explicit collations"); result = value;
        }
        if (result is not null) return result;
        foreach (var value in implicitValues) { if (result is not null && result.Name != value.Name) indeterminate = true; else result ??= value; }
        return indeterminate ? new(Indeterminate: true) : result;
    }
    internal static CollationDerivation? Require(Resolved expression, bool sortGroup = false) => Require(expression.Collation, sortGroup);
    internal static CollationDerivation? Require(CollationDerivation? value, bool sortGroup = false)
    {
        if (value?.Conflict == true || value?.Indeterminate == true)
            throw new SqlException(sortGroup && value.Indeterminate ? "42P21" : "42P22", "could not determine which collation to use");
        return value;
    }
    internal static Resolved Derive(Resolved expression)
    {
        if (expression.Op is "column" or "outer" or "bound" or "constant" or "collate" or "initplan" or "correlated" || expression.Args.Length == 0) return expression;
        var comparison = expression.Text == "width_bucket_array" && expression.Op == "call" || expression.Op is "=" or "<>" or "!=" or "<" or "<=" or ">" or ">=" or "distinct" or "not_distinct" or "nullif" or "in" or "quantified" or "open_predicate";
        if (!Collatable(expression.Type) && !comparison) return expression;
        CollationDerivation? derivation;
        if (expression.Op == "cast" && expression.Target?.Length is null || expression.Op is "promote" or "subscript") derivation = expression.Args[0].Collation;
        else if (expression.Op == "case") derivation = Merge(expression.Args.Where((_, index) => index % 2 != 0 || index == expression.Args.Length - 1));
        else derivation = Merge(expression.Args);
        if (comparison) Require(derivation);
        return expression with { Collation = derivation };
    }
    internal static int Compare(SqlValue left, SqlValue right, CollationDerivation? derivation, Meter? meter = null)
    {
        var table = Require(derivation)?.Table;
        if (table is null || !Collatable(left.Type)) return Evaluate.Compare(left, right, meter);
        if (left.IsNull || right.IsNull) return left.IsNull == right.IsNull ? 0 : left.IsNull ? 1 : -1;
        if (left.Type == SqlType.Text)
        {
            meter?.Charge((left.AsText().EnumerateRunes().Count() + (long)right.AsText().EnumerateRunes().Count()) * Costs.Collate); meter?.Guard();
            return table.SortKey(left.AsText()).AsSpan().SequenceCompareTo(table.SortKey(right.AsText()));
        }
        var a = left.AsArray(); var b = right.AsArray();
        for (var i = 0; i < Math.Min(a.Count, b.Count); i++) { var order = Compare(a.Values[i], b.Values[i], derivation, meter); if (order != 0) return order; }
        var count = a.Count.CompareTo(b.Count); if (count != 0) return count;
        var rank = a.Rank.CompareTo(b.Rank); if (rank != 0) return rank;
        for (var i = 0; i < a.Rank; i++)
        {
            var dimension = a.Dimensions[i].CompareTo(b.Dimensions[i]); if (dimension != 0) return dimension;
            var lower = a.LowerBounds[i].CompareTo(b.LowerBounds[i]); if (lower != 0) return lower;
        }
        return 0;
    }
}
