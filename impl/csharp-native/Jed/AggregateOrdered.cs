namespace Jed;

internal sealed class AggregateOrdered
{
    private readonly List<SqlValue[]> inputs = [];
    private readonly List<SortValue[]> decorations = [];
    private long memoryBytes;
    internal void Release(Meter meter) { meter.Memory.Release(memoryBytes); memoryBytes = 0; }
    internal static bool Hypothetical(string name) => name is "rank" or "dense_rank" or "percent_rank" or "cume_dist";
    internal static bool Known(string name) => Hypothetical(name) || name is "mode" or "percentile_cont" or "percentile_disc";
    internal void Add(AggregatePlan plan, SqlValue[] values, Meter meter)
    {
        if (!Hypothetical(plan.Name) && values[0].IsNull) return;
        var retainedBytes = meter.Memory.Enabled ? Hypothetical(plan.Name) ? MemorySize.RowBytes(values) : MemorySize.ValueBytes(values[0]) : 0;
        if (plan.Name == "percentile_cont" && values[0].Type != SqlType.Interval) values = [Resolve.Cast(values[0], SqlType.F64)];
        if (Hypothetical(plan.Name))
        {
            var keys = new SortValue[values.Length];
            for (var i = 0; i < values.Length; i++) keys[i] = CollationRules.Decorate(values[i], plan.WithinOrder![i].Expression, meter);
            decorations.Add(keys);
        }
        meter.Guard(); inputs.Add(values);
        meter.Memory.Reserve(retainedBytes); memoryBytes = Meter.Add(memoryBytes, retainedBytes);
    }
    internal SqlValue Result(AggregatePlan plan, SqlValue[] direct, Meter meter)
    {
        var order = plan.WithinOrder!;
        if (Hypothetical(plan.Name))
        {
            long before = 0, inclusive = 0; var distinct = new HashSet<string>(StringComparer.Ordinal);
            var directKeys = direct.Select((value, i) => CollationRules.Decorate(value, order[i].Expression)).ToArray();
            for (var i = 0; i < inputs.Count; i++)
            {
                var input = inputs[i]; var compare = Compare(order, decorations[i], directKeys);
                if (compare < 0) { before++; distinct.Add(GroupExecution.Identity(input)); }
                if (compare <= 0) inclusive++;
            }
            return plan.Name switch
            {
                "rank" => SqlValue.Integer(before + 1), "dense_rank" => SqlValue.Integer(distinct.Count + 1L),
                "percent_rank" => SqlValue.Float(inputs.Count == 0 ? 0 : (double)before / inputs.Count, SqlType.F64),
                _ => SqlValue.Float((double)(inclusive + 1) / (inputs.Count + 1L), SqlType.F64)
            };
        }
        using var keyMemory = new QueryInputMemory(meter);
        decorations.Clear();
        foreach (var input in inputs)
        {
            var keys = new SortValue[input.Length];
            for (var i = 0; i < input.Length; i++) keys[i] = CollationRules.Decorate(input[i], order[i].Expression, meter);
            keyMemory.ReserveBytes(MemorySize.SortKeyBytes(keys));
            decorations.Add(keys);
        }
        var sorted = inputs.Select((value, ordinal) => (value, ordinal)).ToList();
        sorted.Sort((a, b) => { var comparison = Compare(order, decorations[a.ordinal], decorations[b.ordinal]); return comparison != 0 ? comparison : a.ordinal.CompareTo(b.ordinal); });
        if (plan.Name == "mode")
        {
            if (sorted.Count == 0) { decorations.Clear(); return SqlValue.Null(plan.Type); }
            var best = 0; var bestCount = 0;
            for (var start = 0; start < sorted.Count;)
            {
                var end = start + 1;
                while (end < sorted.Count && Compare(order, decorations[sorted[start].ordinal], decorations[sorted[end].ordinal]) == 0) end++;
                if (end - start > bestCount) { best = start; bestCount = end - start; }
                start = end;
            }
            decorations.Clear(); keyMemory.Dispose();
            return sorted[best].value[0];
        }
        decorations.Clear(); keyMemory.Dispose();
        var fraction = direct[0];
        if (fraction.IsNull) return SqlValue.Null(plan.Type);
        static double? Fraction(SqlValue value)
        {
            if (value.IsNull) return null;
            var p = Resolve.Cast(value, SqlType.F64).AsFloat();
            if (double.IsNaN(p) || p < 0 || p > 1) throw Errors.Overflow();
            return p;
        }
        SqlValue At(double? p)
        {
            var scalarType = fraction.Type.Kind == SqlKind.Array ? plan.Type.Element! : plan.Type;
            if (p is null) return SqlValue.Null(scalarType);
            if (plan.Name == "percentile_disc") return sorted[Math.Max(0, (int)Math.Ceiling(p.Value * sorted.Count) - 1)].value[0];
            var position = p.Value * (sorted.Count - 1); var lo = (int)Math.Floor(position); var hi = (int)Math.Ceiling(position);
            var left = sorted[lo].value[0]; if (lo == hi) return left;
            var right = sorted[hi].value[0]; var proportion = position - lo;
            if (left.Type == SqlType.Interval) return SqlValue.Interval(IntervalLerp(left.AsInterval(), right.AsInterval(), proportion));
            var delta = right.AsFloat() - left.AsFloat(); var scaled = proportion * delta;
            return SqlValue.Float(left.AsFloat() + scaled, SqlType.F64);
        }
        if (fraction.Type.Kind == SqlKind.Array)
        {
            var array = fraction.AsArray(); var proportions = array.Values.Select(Fraction).ToArray();
            if (sorted.Count == 0) return SqlValue.Null(plan.Type);
            return SqlValue.Array(new(plan.Type.Element!, proportions.Select(At), array.Dimensions, array.LowerBounds));
        }
        var scalar = Fraction(fraction);
        return sorted.Count == 0 ? SqlValue.Null(plan.Type) : At(scalar);
    }
    private static int Compare(SortPlan[] order, SortValue[] left, SortValue[] right)
    {
        for (var i = 0; i < order.Length; i++)
        {
            var comparison = SortValue.Compare(left[i], right[i], order[i]);
            if (comparison != 0) return comparison;
        }
        return 0;
    }
    internal static SqlInterval IntervalLerp(SqlInterval first, SqlInterval second, double factor)
    {
        var difference = second.Subtract(first);
        static double RoundMicros(double value) => Math.Round(value * 1_000_000, MidpointRounding.ToEven) / 1_000_000;
        var monthProduct = difference.Months * factor; var dayProduct = difference.Days * factor;
        if (monthProduct < int.MinValue || monthProduct >= 2147483648d || dayProduct < int.MinValue || dayProduct >= 2147483648d) throw TemporalValue.Overflow();
        var months = (int)monthProduct; long days = (int)dayProduct;
        var monthDays = RoundMicros((monthProduct - months) * 30);
        var remainder = dayProduct - days + monthDays - (long)monthDays;
        var seconds = RoundMicros(remainder * 86400);
        if (Math.Abs(seconds) >= 86400) { var carry = (long)(seconds / 86400); days += carry; seconds -= carry * 86400d; }
        days += (long)monthDays;
        var microsProduct = difference.Micros * factor; var micros = Math.Round(microsProduct + seconds * 1_000_000, MidpointRounding.ToEven);
        if (double.IsNaN(micros) || micros < -9223372036854775808d || micros >= 9223372036854775808d) throw TemporalValue.Overflow();
        return first.Add(SqlInterval.Create(months, days, (long)micros));
    }
}
