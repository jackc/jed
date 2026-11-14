using System.Globalization;

namespace Jed;

// An integer SUM/AVG has at most Int64.MaxValue inputs, each an Int64. Its
// mathematical sum fits in 127 signed bits, even when intermediate sums cancel.
// Decimal storage, finalization and logical accounting still use the same
// scale-zero decimal representation as the arbitrary-precision fold.
internal sealed class AggregateIntegerSum
{
    private Int128 sum;
    internal int Groups { get; private set; }
    internal long LogicalBytes => MemorySchedule.Value + Groups * MemorySchedule.DecimalGroup;
    internal ExactDecimal Result => ExactDecimal.Parse(sum.ToString(CultureInfo.InvariantCulture));

    internal void Add(long value, Meter meter)
    {
        var magnitude = value < 0 ? (ulong)(-(value + 1)) + 1 : (ulong)value;
        var addendGroups = CountGroups(magnitude);
        meter.Charge(Math.Max(1, Math.Max(Groups, addendGroups)) - 1); meter.Guard();
        sum += value;
        var total = sum < 0 ? (UInt128)(-(sum + 1)) + 1 : (UInt128)sum;
        Groups = CountGroups(total);
    }

    private static readonly UInt128 Group6 = (UInt128)10000000000000000000UL * 10;
    private static readonly UInt128 Group7 = Group6 * 10000;
    private static readonly UInt128 Group8 = Group7 * 10000;
    private static readonly UInt128 Group9 = Group8 * 10000;
    private static readonly UInt128 Group10 = Group9 * 10000;

    internal static int CountGroups(UInt128 magnitude)
    {
        // Logical decimal groups change only at powers of 10,000. Compare
        // thresholds rather than divide both the input and sum for every row.
        if (magnitude <= ulong.MaxValue)
        {
            var small = (ulong)magnitude;
            if (small == 0) return 0;
            if (small < 10000) return 1;
            if (small < 100000000) return 2;
            if (small < 1000000000000) return 3;
            if (small < 10000000000000000) return 4;
            return 5;
        }
        if (magnitude < Group6) return 5;
        if (magnitude < Group7) return 6;
        if (magnitude < Group8) return 7;
        if (magnitude < Group9) return 8;
        if (magnitude < Group10) return 9;
        return 10;
    }
}
