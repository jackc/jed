namespace Jed;

/// <summary>Host-owned session configuration. Clock callbacks return Unix microseconds.</summary>
public sealed record SessionOptions
{
    public TablePrivileges DefaultPrivileges { get; init; } = TablePrivileges.All;
    public bool AllowDdl { get; init; } = true;
    public bool? AllowTempDdl { get; init; }
    public long MaxQueryMemoryBytes { get; init; }
    public long LifetimeMaxCost { get; init; }
    public long TempBuffers { get; init; } = 32L * 1024 * 1024;
    public long MaxTempStorageBytes { get; init; } = 1024L * 1024 * 1024;
    public long MaxSqlLength { get; init; } = 1024 * 1024;
    public Func<long>? Clock { get; init; }
    public Func<int, byte[]>? RandomSource { get; init; }
    public string TimeZone { get; init; } = "UTC";
    public TimeZoneData? TimeZones { get; init; }
    public UnicodeData? UnicodeData { get; init; }
    // The operating-system clock is confined to the host configuration seam.
    internal static long HostClock() => (TimeProvider.System.GetUtcNow().Ticks - DateTime.UnixEpoch.Ticks) / 10;
}

internal sealed class TemporalContext(Func<long> clock, string zone, TimeZoneData? data = null, UnicodeData? unicode = null)
{
    internal UnicodeData? Unicode { get; } = unicode;
    internal TimeZoneData? Data => data;
    internal bool UsesSessionZone { get; private set; }
    internal TemporalContext LiteralContext() => new(clock, Zone, data, Unicode);
    private long? statementMicros;
    internal string Zone { get; } = zone;
    internal long StatementMicros => statementMicros ??= ClockMicros();
    internal long ClockMicros() => TemporalValue.FiniteTimestamp(clock());
    internal TimeZoneRule ResolveZone(string? name = null)
    { if (name is null) UsesSessionZone = true; return TimeZoneData.Resolve(name ?? Zone, data); }
    internal static int Offset(string zone) => TimeZoneData.Resolve(zone, null).OffsetAt(0).Seconds;
    internal int OffsetSeconds => ResolveZone().OffsetAt(StatementMicros).Seconds;
}
