namespace Jed;

// aggregates.md §23: exact numeric moments and separately rounded float8 moments.
internal sealed class AggregateMoments
{
    internal long Count;
    private ExactDecimal sum = ExactDecimal.Zero;
    private ExactDecimal squares = ExactDecimal.Zero;
    private double sx, sy, sxx, syy, sxy;

    internal static bool Single(string name) => name is "var_pop" or "var_samp" or "variance" or "stddev" or "stddev_samp" or "stddev_pop";
    internal static bool Pair(string name) => name is "covar_pop" or "covar_samp" or "corr" or "regr_count" or "regr_avgx" or "regr_avgy" or "regr_sxx" or "regr_syy" or "regr_sxy" or "regr_slope" or "regr_intercept" or "regr_r2";
    internal void Add(AggregatePlan plan, SqlValue value, SqlValue[] extra, Meter meter)
    {
        if (value.IsNull || Pair(plan.Name) && extra[0].IsNull) return;
        if (plan.Name == "regr_count") { Count++; return; }
        if (plan.Type == SqlType.Decimal)
        {
            var x = Resolve.Cast(value, SqlType.Decimal).AsDecimal();
            var square = x.AggregateProduct(x);
            meter.Charge(x.DecimalWork(x, "*") + sum.DecimalWork(x, "+") + squares.DecimalWork(square, "+")); meter.Guard();
            sum = sum.AddUncapped(x); squares = squares.AddUncapped(square); Count++; return;
        }
        var paired = Pair(plan.Name);
        var xValue = Resolve.Cast(paired ? extra[0] : value, SqlType.F64).AsFloat();
        var yValue = paired ? Resolve.Cast(value, SqlType.F64).AsFloat() : 0;
        var nextCount = (double)Count + 1;
        var nextX = sx + xValue; var nextY = sy + yValue;
        var xx = sxx; var yy = syy; var xy = sxy;
        if (Count != 0)
        {
            var txProduct = xValue * nextCount; var tx = txProduct - nextX;
            if (paired)
            {
                var tyProduct = yValue * nextCount; var ty = tyProduct - nextY;
                var scale = 1 / (nextCount * Count);
                var xxProduct = tx * tx; var yyProduct = ty * ty; var xyProduct = tx * ty;
                xx += xxProduct * scale; yy += yyProduct * scale; xy += xyProduct * scale;
            }
            else { var product = tx * tx; xx += product / (nextCount * Count); }
            if (double.IsInfinity(nextX) || double.IsInfinity(xx))
            {
                if (!double.IsInfinity(sx) && !double.IsInfinity(xValue)) throw Errors.Overflow();
                xx = double.NaN;
            }
            if (paired && (double.IsInfinity(nextY) || double.IsInfinity(yy)))
            {
                if (!double.IsInfinity(sy) && !double.IsInfinity(yValue)) throw Errors.Overflow();
                yy = double.NaN;
            }
            if (paired && double.IsInfinity(xy))
            {
                if (!double.IsInfinity(sx) && !double.IsInfinity(xValue) && !double.IsInfinity(sy) && !double.IsInfinity(yValue)) throw Errors.Overflow();
                xy = double.NaN;
            }
        }
        else
        {
            if (!double.IsFinite(xValue)) { xx = double.NaN; xy = double.NaN; }
            if (paired && !double.IsFinite(yValue)) { yy = double.NaN; xy = double.NaN; }
        }
        Count++; sx = nextX; sy = nextY; sxx = xx; syy = yy; sxy = xy;
    }
    internal SqlValue Result(AggregatePlan plan)
    {
        var name = plan.Name;
        if (name == "regr_count") return SqlValue.Integer(Count);
        if (Count == 0) return SqlValue.Null(plan.Type);
        if (Single(name))
        {
            var sample = name is "var_samp" or "variance" or "stddev" or "stddev_samp";
            var root = name.StartsWith("stddev", StringComparison.Ordinal);
            if (sample && Count < 2) return SqlValue.Null(plan.Type);
            if (plan.Type == SqlType.Decimal) return SqlValue.Decimal(ExactDecimal.AggregateVariance(sum, squares, Count, sample, root));
            var variance = sxx / (sample ? Count - 1 : Count);
            return SqlValue.Float(root ? Math.Sqrt(variance) : variance, SqlType.F64);
        }
        if (name == "covar_samp" && Count < 2 || name is "corr" or "regr_slope" or "regr_intercept" or "regr_r2" && sxx == 0 || name == "corr" && syy == 0) return SqlValue.Null(plan.Type);
        var result = name switch
        {
            "covar_pop" => sxy / Count, "covar_samp" => sxy / (Count - 1),
            "corr" => sxy / Math.Sqrt(sxx * syy), "regr_avgx" => sx / Count, "regr_avgy" => sy / Count,
            "regr_sxx" => sxx, "regr_syy" => syy, "regr_sxy" => sxy,
            "regr_slope" => sxy / sxx, "regr_intercept" => (sy - sx * sxy / sxx) / Count,
            "regr_r2" => syy == 0 ? 1 : sxy * sxy / (sxx * syy),
            _ => throw new InvalidOperationException("unknown statistical aggregate")
        };
        return SqlValue.Float(result, SqlType.F64);
    }
}

public sealed partial class ExactDecimal
{
    internal ExactDecimal AggregateProduct(ExactDecimal other) => MultiplyExact(other);
    internal static ExactDecimal AggregateVariance(ExactDecimal sum, ExactDecimal squares, long count, bool sample, bool root)
    {
        var n = FromInt64(count); var scale = sum.Scale * 2;
        var numerator = n.MultiplyAtScale(squares, scale).SubtractUncapped(sum.MultiplyAtScale(sum, scale));
        if (numerator.Negative || numerator.IsZero) return Zero;
        var denominator = n.MultiplyExact(sample ? FromInt64(count - 1) : n);
        var divisionScale = numerator.DivisionScale(denominator);
        var result = numerator.DivideUncapped(denominator, divisionScale);
        return (root ? result.SquareRootCore(divisionScale, null) : result).CheckCap();
    }
}
