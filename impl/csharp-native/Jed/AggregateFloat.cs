namespace Jed;

// float.md §7: fold finite operands at input width; resolve special values at finalization.
internal sealed class AggregateFloat
{
    private bool nan, positiveInfinity, negativeInfinity;
    private double sum;
    internal void Add(double value, bool single)
    {
        if (double.IsNaN(value)) nan = true;
        else if (double.IsPositiveInfinity(value)) positiveInfinity = true;
        else if (double.IsNegativeInfinity(value)) negativeInfinity = true;
        else { if (value == 0) value = 0; sum = single ? (float)sum + (float)value : sum + value; }
    }
    internal SqlValue Result(AggregatePlan plan, long count)
    {
        if (count == 0) return SqlValue.Null(plan.Type);
        double result;
        if (nan || positiveInfinity && negativeInfinity) result = double.NaN;
        else if (positiveInfinity) result = double.PositiveInfinity;
        else if (negativeInfinity) result = double.NegativeInfinity;
        else { if (double.IsInfinity(sum)) throw Errors.Overflow(); result = sum; }
        if (plan.Name == "avg") result = plan.Type == SqlType.F32 ? (float)result / (float)count : result / count;
        return SqlValue.Float(result, plan.Type);
    }
}
