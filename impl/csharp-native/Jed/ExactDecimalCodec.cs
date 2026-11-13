using System.Globalization;
using System.Text;

namespace Jed;

public sealed partial class ExactDecimal
{
    internal string CoefficientDigits() => DecimalMagnitude.Render(coefficient);

    /// <summary>Unscaled coefficient groups, most significant first. Zero has no groups.</summary>
    internal ushort[] ExportBase10000()
    {
        if (IsZero) return [];
        var digits = DecimalMagnitude.Render(coefficient);
        var groups = new ushort[(digits.Length + 3) / 4];
        var position = 0;
        for (var i = 0; i < groups.Length; i++)
        {
            var width = i == 0 ? (digits.Length - 1) % 4 + 1 : 4;
            ushort value = 0;
            for (var j = 0; j < width; j++) value = (ushort)(value * 10 + digits[position++] - '0');
            groups[i] = value;
        }
        return groups;
    }

    // File decoding checks framing separately. The stored scalar's cap/typmod
    // is an integrity-check concern, as it is in the peer value decoders.
    internal static ExactDecimal FromBase10000(bool negative, int scale, ReadOnlySpan<ushort> groups)
    {
        if (scale < 0 || scale > ushort.MaxValue || groups.Length > ushort.MaxValue)
            throw new SqlException(SqlStates.DataCorrupted, "invalid decimal codec size");
        var digits = new StringBuilder(groups.Length * 4);
        foreach (var group in groups)
        {
            if (group > 9999) throw new SqlException(SqlStates.DataCorrupted, "decimal digit group out of range");
            digits.Append(group.ToString("D4", CultureInfo.InvariantCulture));
        }
        return new(negative, DecimalMagnitude.Parse(digits.ToString().AsSpan()), scale);
    }

    /// <summary>Exact fixed-slot coefficient at the column scale; never silently rounds.</summary>
    internal long ToUnscaledInt64(int scale)
    {
        if (scale < 0 || scale > MaxScale) throw Overflow();
        var rescaled = Rescale(scale, false);
        if (!rescaled.Equals(this)) throw Overflow();
        return new ExactDecimal(Negative, rescaled.coefficient, 0).ToInt64();
    }
}
