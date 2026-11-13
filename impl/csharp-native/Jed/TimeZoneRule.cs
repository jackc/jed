using System.Buffers.Binary;
using System.Text;

namespace Jed;

public readonly record struct TimeZoneOffset(int Seconds, string Abbreviation, bool IsDaylight);

/// <summary>An immutable loaded zone. No operating-system time-zone database is consulted.</summary>
public sealed class TimeZoneRule
{
    private readonly long[] transitions;
    private readonly byte[] transitionTypes;
    private readonly TimeZoneOffset[] types;
    private readonly PosixZone? future;
    public string Name { get; }
    internal bool IsFixed { get; }
    public string Version { get; }
    public uint Checksum { get; }
    internal TimeZoneRule(string name, int offset)
    { Name = name; IsFixed = true; Version = ""; transitions = []; transitionTypes = []; types = [new(offset, FixedAbbreviation(offset), false)]; }
    private static string FixedAbbreviation(int offset)
    {
        if (offset == 0) return "UTC";
        var magnitude = Math.Abs((long)offset);
        var result = string.Create(System.Globalization.CultureInfo.InvariantCulture, $"{(offset < 0 ? "-" : "+")}{magnitude / 3600:D2}:{magnitude / 60 % 60:D2}");
        return magnitude % 60 == 0 ? result : result + ":" + (magnitude % 60).ToString("D2", System.Globalization.CultureInfo.InvariantCulture);
    }
    internal TimeZoneRule(string name, string version, uint checksum, ReadOnlySpan<byte> data)
    {
        Name = name; Version = version; Checksum = checksum;
        var position = 0; var header = Header(data, ref position); var width = 4;
        if (header.Version != 0)
        {
            var length = BlockLength(header, 4); Need(data, position, length); position += length;
            header = Header(data, ref position); width = 8;
        }
        Need(data, position, BlockLength(header, width));
        transitions = new long[header.Times]; transitionTypes = new byte[header.Times];
        for (var i = 0; i < transitions.Length; i++)
        {
            transitions[i] = width == 8 ? BinaryPrimitives.ReadInt64BigEndian(data[position..]) : BinaryPrimitives.ReadInt32BigEndian(data[position..]); position += width;
            if (i != 0 && transitions[i] <= transitions[i - 1]) throw Bad("unordered TZif transitions");
        }
        for (var i = 0; i < transitions.Length; i++) { transitionTypes[i] = data[position++]; if (transitionTypes[i] >= header.Types) throw Bad("invalid TZif transition type"); }
        var records = data.Slice(position, header.Types * 6); position += records.Length;
        var designations = data.Slice(position, header.Characters); position += designations.Length;
        types = new TimeZoneOffset[header.Types];
        for (var i = 0; i < types.Length; i++)
        {
            var record = records[(i * 6)..]; var abbreviation = record[5];
            if (record[4] > 1 || abbreviation >= designations.Length) throw Bad("invalid TZif local type");
            var end = designations[abbreviation..].IndexOf((byte)0); if (end < 0) throw Bad("unterminated TZif abbreviation");
            types[i] = new(BinaryPrimitives.ReadInt32BigEndian(record), WireReader.Utf8(designations.Slice(abbreviation, end)), record[4] != 0);
        }
        position += header.Leaps * (width + 4) + header.Standard + header.Universal;
        if (width == 8)
        {
            if (position + 2 > data.Length || data[position] != 10 || data[^1] != 10) throw Bad("invalid TZif footer");
            var footer = data[(position + 1)..^1];
            if (footer.ContainsAnyExceptInRange((byte)32, (byte)126)) throw Bad("invalid TZif footer character");
            if (!footer.IsEmpty) future = PosixZone.Parse(Encoding.ASCII.GetString(footer));
        }
        else if (position != data.Length) throw Bad("trailing TZif bytes");
    }
    private readonly record struct Counts(byte Version, int Universal, int Standard, int Leaps, int Times, int Types, int Characters);
    private static Counts Header(ReadOnlySpan<byte> data, ref int position)
    {
        Need(data, position, 44); var bytes = data.Slice(position, 44); position += 44;
        if (!bytes[..4].SequenceEqual("TZif"u8) || bytes[4] is not (0 or 50 or 51)) throw Bad("invalid TZif header");
        var counts = new int[6];
        for (var i = 0; i < 6; i++) { var count = BinaryPrimitives.ReadUInt32BigEndian(bytes[(20 + i * 4)..]); if (count > 1_000_000) throw Bad("TZif count too large"); counts[i] = (int)count; }
        if (counts[4] is < 1 or > 256 || counts[5] < 1) throw Bad("invalid TZif type table");
        return new(bytes[4], counts[0], counts[1], counts[2], counts[3], counts[4], counts[5]);
    }
    private static int BlockLength(Counts c, int width) => c.Times * (width + 1) + c.Types * 6 + c.Characters + c.Leaps * (width + 4) + c.Standard + c.Universal;
    private static void Need(ReadOnlySpan<byte> input, int at, int count) { if (count < 0 || at < 0 || at > input.Length - count) throw Bad("truncated TZif data"); }
    internal static SqlException Bad(string message) => new(SqlStates.InvalidParameterValue, message);
    private TimeZoneOffset Initial => types.FirstOrDefault(type => !type.IsDaylight, types[0]);
    public TimeZoneOffset OffsetAt(long instantMicros)
    {
        var seconds = TemporalValue.FloorDiv(instantMicros, TemporalValue.MicrosPerSecond);
        if (future is not null && (transitions.Length == 0 || seconds >= transitions[^1])) return future.OffsetAt(seconds);
        if (transitions.Length == 0 || seconds < transitions[0]) return Initial;
        var at = Array.BinarySearch(transitions, seconds); if (at < 0) at = ~at - 1;
        return types[transitionTypes[at]];
    }
    public long ToLocal(long instantMicros) => TemporalValue.IsInfinite(instantMicros) ? instantMicros :
        TemporalValue.FiniteTimestamp((Int128)instantMicros + (long)OffsetAt(instantMicros).Seconds * TemporalValue.MicrosPerSecond);
    public long ToInstant(long localMicros)
    {
        if (TemporalValue.IsInfinite(localMicros)) return localMicros;
        long? best = null;
        var offsets = types.Select(type => type.Seconds).Concat(future?.Offsets ?? []).Distinct();
        foreach (var offset in offsets)
        {
            var candidate = (Int128)localMicros - (long)offset * TemporalValue.MicrosPerSecond;
            if (candidate <= long.MinValue || candidate >= long.MaxValue) continue;
            var instant = (long)candidate;
            if (OffsetAt(instant).Seconds == offset && (best is null || instant > best.Value)) best = instant;
        }
        if (best is not null) return best.Value;
        var before = Initial.Seconds;
        for (var i = 0; i < transitions.Length; i++)
        {
            var after = types[transitionTypes[i]].Seconds;
            if (InGap(localMicros, transitions[i], before, after)) return TemporalValue.FiniteTimestamp((Int128)localMicros - (long)before * TemporalValue.MicrosPerSecond);
            before = after;
        }
        if (future is not null)
        {
            var year = TemporalValue.CivilFromDays(TemporalValue.FloorDiv(localMicros, TemporalValue.MicrosPerDay)).Year;
            foreach (var transition in future.Transitions(year))
                if (InGap(localMicros, transition.Instant, transition.Before, transition.After)) return TemporalValue.FiniteTimestamp((Int128)localMicros - (long)transition.Before * TemporalValue.MicrosPerSecond);
        }
        throw TemporalValue.Overflow();
    }
    private static bool InGap(long local, long instantSeconds, int before, int after) => after > before &&
        (Int128)local >= ((Int128)instantSeconds + before) * TemporalValue.MicrosPerSecond && (Int128)local < ((Int128)instantSeconds + after) * TemporalValue.MicrosPerSecond;
}
