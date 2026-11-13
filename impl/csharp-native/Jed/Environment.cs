using System.Security.Cryptography;

namespace Jed;

public interface IRandomSource { byte[] Fill(int count); }
public interface IClockSource { long NowMicroseconds(); }
public sealed class OsRandomSource : IRandomSource
{
    public byte[] Fill(int count) => RandomNumberGenerator.GetBytes(count);
}
public sealed class WallClock : IClockSource
{
    public long NowMicroseconds() => SessionOptions.HostClock();
}
public sealed class ManualClock(long microseconds = 0) : IClockSource
{
    private long value = microseconds;
    public long NowMicroseconds() => Interlocked.Read(ref value);
    public void Set(long microseconds) => Interlocked.Exchange(ref value, microseconds);
    public void Advance(long microseconds) => Interlocked.Add(ref value, microseconds);
}
public sealed class AdvancingClock(long start, long step) : IClockSource
{
    private readonly object gate = new();
    private long current = start, increment = step;
    public long NowMicroseconds() { lock (gate) { var result = current; current = unchecked(current + increment); return result; } }
    public void Reset(long start, long step) { lock (gate) { current = start; increment = step; } }
}

/// <summary>Immutable outside inputs shared explicitly by database handles.</summary>
public sealed class DatabaseEnvironment
{
    public UnicodeData? UnicodeData { get; }
    public TimeZoneData? TimeZones { get; }
    public IRandomSource RandomSource { get; }
    public IClockSource ClockSource { get; }
    public IReadOnlyList<HostFunction> Functions { get; }
    internal Func<IRandomSource> KeyEntropyFactory { get; }
    internal DatabaseEnvironment(UnicodeData? unicode, TimeZoneData? zones, IRandomSource random, IClockSource clock,
        Func<IRandomSource> keyEntropy, IEnumerable<HostFunction> functions)
    {
        UnicodeData = unicode; TimeZones = zones; RandomSource = random; ClockSource = clock;
        KeyEntropyFactory = keyEntropy; Functions = Array.AsReadOnly(functions.ToArray());
    }
    public static DatabaseEnvironmentBuilder Builder() => new();
    public DatabaseEnvironmentBuilder ToBuilder() => new(this);
    internal Func<int, byte[]> NewKeyEntropy() => KeyEntropyFactory().Fill;
    internal HostFunction? Find(string name, IReadOnlyList<SqlType> arguments) => Functions.FirstOrDefault(function => function.Signature(name, arguments));
}

public sealed class DatabaseEnvironmentBuilder
{
    private UnicodeData? unicode;
    private TimeZoneData? zones;
    private IRandomSource random = new OsRandomSource();
    private IClockSource clock = new WallClock();
    private Func<IRandomSource> keyEntropy = () => new OsRandomSource();
    private readonly List<HostFunction> functions = [];
    public DatabaseEnvironmentBuilder() { }
    internal DatabaseEnvironmentBuilder(DatabaseEnvironment source)
    {
        unicode = source.UnicodeData; zones = source.TimeZones; random = source.RandomSource; clock = source.ClockSource;
        keyEntropy = source.KeyEntropyFactory; functions.AddRange(source.Functions);
    }
    public DatabaseEnvironmentBuilder LoadUnicodeData(ReadOnlySpan<byte> bytes)
    { var loaded = Jed.UnicodeData.Load(bytes); unicode = unicode is null ? loaded : unicode.Add(loaded); return this; }
    public DatabaseEnvironmentBuilder LoadTimeZoneData(ReadOnlySpan<byte> bytes)
    { var loaded = TimeZoneData.Load(bytes); zones = zones is null ? loaded : zones.Add(loaded); return this; }
    public DatabaseEnvironmentBuilder RegisterFunction(HostFunction function)
    {
        ArgumentNullException.ThrowIfNull(function); function.Validate();
        if (functions.Any(existing => existing.Signature(function.Name, function.Arguments)))
            throw new SqlException("42723", "duplicate host function signature");
        functions.Add(function); return this;
    }
    public DatabaseEnvironmentBuilder WithRandomSource(IRandomSource source) { random = source ?? throw new ArgumentNullException(nameof(source)); return this; }
    public DatabaseEnvironmentBuilder WithClockSource(IClockSource source) { clock = source ?? throw new ArgumentNullException(nameof(source)); return this; }
    public DatabaseEnvironmentBuilder WithKeyEntropySource(Func<IRandomSource> factory) { keyEntropy = factory ?? throw new ArgumentNullException(nameof(factory)); return this; }
    public DatabaseEnvironmentBuilder WithInsecureKeySeed(ulong seed) { keyEntropy = () => new SeededRandomSource(seed); return this; }
    internal DatabaseEnvironmentBuilder WithLoadedData(UnicodeData? unicode, TimeZoneData? zones)
    { this.unicode = unicode; this.zones = zones; return this; }
    public DatabaseEnvironment Build() => new(unicode, zones, random, clock, keyEntropy, functions);
}
