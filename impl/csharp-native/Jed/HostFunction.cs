namespace Jed;

public enum FunctionVolatility { Immutable, Stable, Volatile }
public delegate SqlValue HostScalarKernel(IReadOnlyList<SqlValue> arguments);
public delegate void HostBatchKernel(IReadOnlyList<IReadOnlyList<SqlValue>> arguments, IList<SqlValue> output);

/// <summary>A strict host scalar overload. Host code owns its declared semantics and cost.</summary>
public sealed class HostFunction
{
    public string Name { get; }
    public IReadOnlyList<SqlType> Arguments { get; }
    public SqlType Result { get; }
    public FunctionVolatility Volatility { get; init; } = FunctionVolatility.Volatile;
    public bool CrossCoreDeterministic { get; init; }
    public long Cost { get; init; } = 1;
    public string? ComponentId { get; init; }
    public uint? SemanticVersion { get; init; }
    internal HostBatchKernel Kernel { get; }
    internal bool Batched { get; }
    public HostFunction(string name, IEnumerable<SqlType> arguments, SqlType result, HostScalarKernel kernel)
        : this(name, arguments, result, (columns, output) =>
        {
            var count = columns.Count == 0 ? 1 : columns[0].Count;
            for (var row = 0; row < count; row++) output.Add(kernel(columns.Select(column => column[row]).ToArray()));
        }, false)
    { ArgumentNullException.ThrowIfNull(kernel); }
    public HostFunction(string name, IEnumerable<SqlType> arguments, SqlType result, HostBatchKernel kernel)
        : this(name, arguments, result, kernel, true) { }
    private HostFunction(string name, IEnumerable<SqlType> arguments, SqlType result, HostBatchKernel kernel, bool batched)
    {
        ArgumentNullException.ThrowIfNull(name); ArgumentNullException.ThrowIfNull(arguments);
        ArgumentNullException.ThrowIfNull(result); ArgumentNullException.ThrowIfNull(kernel);
        Name = name; Arguments = Array.AsReadOnly(arguments.ToArray()); Result = result; Kernel = kernel; Batched = batched;
    }
    public static HostFunction Batch(string name, IEnumerable<SqlType> arguments, SqlType result, HostBatchKernel kernel,
        FunctionVolatility volatility = FunctionVolatility.Volatile, long cost = 1, string? componentId = null, uint? semanticVersion = null, bool crossCoreDeterministic = false)
        => new(name, arguments, result, kernel, true)
        { Volatility = volatility, Cost = cost, ComponentId = componentId, SemanticVersion = semanticVersion, CrossCoreDeterministic = crossCoreDeterministic };
    internal void Validate()
    {
        if (Name.Length == 0 || Name.Contains('\0') || Cost < 0 || !Enum.IsDefined(Volatility))
            throw new SqlException(SqlStates.InvalidParameterValue, "invalid host function declaration");
        if (Arguments.Append(Result).Any(type => type.Kind is SqlKind.Unknown or SqlKind.Array or SqlKind.Range or SqlKind.Composite))
            throw new SqlException(SqlStates.InvalidParameterValue, "host functions require exact scalar types");
    }
    internal bool Signature(string name, IReadOnlyList<SqlType> arguments) => Name == name && Arguments.SequenceEqual(arguments);
}
