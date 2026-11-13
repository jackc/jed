namespace Jed;

/// <summary>Options captured for one statement and retained by its cursor.</summary>
public sealed record QueryOptions
{
    public IReadOnlyList<SqlValue> Parameters { get; init; } = [];
    public long MaxCost { get; init; }
    public long MaxScalarBytes { get; init; }
    public long? MaxSqlLength { get; init; }
    public Func<long>? Clock { get; init; }
    public string? TimeZone { get; init; }
}
