namespace Jed;

internal static partial class Estimate
{
    internal static long Add(long a, long b) => a > long.MaxValue - b ? long.MaxValue : a + b;
    internal static long Multiply(long a, long b) => b != 0 && a > long.MaxValue / b ? long.MaxValue : a * b;
    internal static long Scale(long count, long numerator, long denominator)
    {
        var value = (Int128)count * numerator;
        return (long)Int128.Min(long.MaxValue, (value + denominator - 1) / denominator);
    }
    internal static long Fraction(long count, string name)
    {
        var fact = PlannerFacts.Selectivities.First(item => item.Name == name);
        return Scale(count, fact.Numerator, fact.Denominator);
    }
    internal static long Rows(long count, Resolved? predicate)
    {
        if (count == 0 || predicate is null) return count;
        if (Contradiction.Proves(predicate)) return 0;
        if (predicate.Op == "constant" && !predicate.ParameterUnbound) return predicate.Constant.IsNull || predicate.Type == SqlType.Boolean && !predicate.Constant.AsBoolean() ? 0 : count;
        if (predicate.Op == "and")
        {
            var terms = PredicatePlan.Conjuncts(predicate).ToArray(); var used = new bool[terms.Length];
            for (var i = 0; i < terms.Length; i++)
            {
                if (used[i]) continue;
                var paired = false;
                if (RangeOperand(terms[i]) is { } first)
                    for (var j = i + 1; j < terms.Length; j++)
                        if (!used[j] && RangeOperand(terms[j]) is { } second && first.Lower != second.Lower && Resolve.Equivalent(first.Value, second.Value))
                        { used[j] = true; paired = true; break; }
                count = paired ? Fraction(count, "paired_range") : Rows(count, terms[i]);
            }
            return count;
        }
        if (predicate.Op == "or")
        {
            var left = Rows(count, predicate.Args[0]); var right = Rows(count, predicate.Args[1]);
            return Math.Min(count, Add(left, right));
        }
        if (predicate.Op == "in_list")
        {
            var seen = new HashSet<string>(StringComparer.Ordinal); long rows = 0;
            foreach (var value in predicate.Args.Skip(1))
                if (!value.Constant.IsNull && seen.Add(GroupExecution.Identity([value.Constant]))) rows = Math.Min(count, Add(rows, Fraction(count, "equality")));
            return rows;
        }
        if (predicate.Op is "=" or "<>" or "<" or "<=" or ">" or ">=" && predicate.Args.Any(arg => arg.Op == "constant" && !arg.ParameterUnbound && arg.Constant.IsNull)) return 0;
        return predicate.Op switch
        {
            "=" => Fraction(count, "equality"), "<>" or "!=" => count - Fraction(count, "equality"),
            "<" or "<=" or ">" or ">=" => Fraction(count, "inequality"),
            "is_null" => Fraction(count, "null_test"), "is_not_null" => count - Fraction(count, "null_test"),
            "not" => count - Rows(count, predicate.Args[0]),
            "column" when predicate.Type == SqlType.Boolean => Fraction(count, "boolean"),
            _ => Fraction(count, "opaque")
        };
    }
    private static (Resolved Value, bool Lower)? RangeOperand(Resolved predicate)
    {
        if (predicate.Op is not ("<" or "<=" or ">" or ">=") || predicate.Args.Length != 2) return null;
        var left = predicate.Args[0]; var right = predicate.Args[1]; var lower = predicate.Op is ">" or ">=";
        if (left.Op == "constant") { (left, right) = (right, left); lower = !lower; }
        return right.Op == "constant" && left.Op != "constant" ? (left, lower) : null;
    }
    internal static long Pages(long nodes, int height, long rows, Resolved? predicate, bool unique = false)
    {
        if (rows == 0 || nodes == 0) return 0;
        return unique ? height : Math.Clamp(Rows(nodes, predicate), height, nodes);
    }

}
