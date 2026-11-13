namespace Jed;

/// <summary>Host-owned session configuration. Clock callbacks return Unix microseconds.</summary>
public sealed record SessionOptions
{
    public long LifetimeMaxCost { get; init; }
    public long MaxSqlLength { get; init; } = 1024 * 1024;
    public Func<long>? Clock { get; init; }
    public string TimeZone { get; init; } = "UTC";
    public TimeZoneData? TimeZones { get; init; }
    // The operating-system clock is confined to the host configuration seam.
    internal static long HostClock() => (TimeProvider.System.GetUtcNow().Ticks - DateTime.UnixEpoch.Ticks) / 10;
}

internal sealed class TemporalContext(Func<long> clock, string zone, TimeZoneData? data = null)
{
    private long? statementMicros;
    internal string Zone { get; } = zone;
    internal long StatementMicros => statementMicros ??= ClockMicros();
    internal long ClockMicros() => TemporalValue.FiniteTimestamp(clock());
    internal TimeZoneRule ResolveZone(string? name = null) => TimeZoneData.Resolve(name ?? Zone, data);
    internal static int Offset(string zone) => TimeZoneData.Resolve(zone, null).OffsetAt(0).Seconds;
    internal int OffsetSeconds => ResolveZone().OffsetAt(StatementMicros).Seconds;
}
