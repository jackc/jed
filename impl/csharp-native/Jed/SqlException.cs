namespace Jed;

/// <summary>A structured SQL error. SQLSTATE is stable; prose is informational.</summary>
public interface ISqlError
{
    string SqlState { get; }
    long Cost { get; }
}

public sealed class SqlException(string sqlState, string message) : Exception(message), ISqlError
{
    public string SqlState { get; } = sqlState;
    public long Cost { get; internal set; }
}

/// <summary>Idiomatic task cancellation that also retains jed's SQLSTATE.</summary>
public sealed class SqlCanceledException(CancellationToken token) : OperationCanceledException("query canceled", token), ISqlError
{
    public string SqlState => SqlStates.QueryCanceled;
    public long Cost { get; internal set; }
}

internal static class Errors
{
    internal static SqlException Type(string message) => new(SqlStates.DatatypeMismatch, message);
    internal static SqlException Syntax(string message) => new(SqlStates.SyntaxError, message);
    internal static SqlException Overflow() => new(SqlStates.NumericValueOutOfRange, "integer out of range");
    internal static SqlException Unsupported(string feature) => new(SqlStates.FeatureNotSupported, $"C# experiment has not implemented {feature}");
}
