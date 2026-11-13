namespace Jed;

/// <summary>An owned rectangular SQL array, flattened in row-major order.</summary>
public sealed class SqlArray
{
    private readonly SqlValue[] values;
    private readonly int[] dimensions;
    private readonly int[] lowerBounds;
    public SqlType ElementType { get; }
    public IReadOnlyList<SqlValue> Values { get; }
    public IReadOnlyList<int> Dimensions { get; }
    public IReadOnlyList<int> LowerBounds { get; }
    public int Count => values.Length;
    public int Rank => dimensions.Length;
    public SqlArray(SqlType elementType, IEnumerable<SqlValue> values, IEnumerable<int>? dimensions = null, IEnumerable<int>? lowerBounds = null)
        : this(elementType, values, dimensions, lowerBounds, false) { }
    internal SqlArray(SqlType elementType, IEnumerable<SqlValue> values, IEnumerable<int>? dimensions, IEnumerable<int>? lowerBounds, bool stored)
    {
        if (elementType.Kind is SqlKind.Unknown or SqlKind.Array) throw Errors.Type("invalid array element type");
        ElementType = elementType;
        this.values = values.ToArray();
        this.dimensions = dimensions?.ToArray() ?? (this.values.Length == 0 ? [] : [this.values.Length]);
        this.lowerBounds = lowerBounds?.ToArray() ?? Enumerable.Repeat(1, this.dimensions.Length).ToArray();
        if (this.dimensions.Length > byte.MaxValue || this.dimensions.Length != this.lowerBounds.Length) throw new SqlException(SqlStates.ProgramLimitExceeded, "invalid array dimensionality");
        long count = this.dimensions.Length == 0 ? 0 : 1;
        for (var i = 0; i < this.dimensions.Length; i++)
        {
            if (this.dimensions[i] < 1) throw new SqlException(SqlStates.ArraySubscriptError, "array dimension must be positive");
            count *= this.dimensions[i];
            if (count > 134217727) throw new SqlException(SqlStates.ProgramLimitExceeded, "array is too large");
            if ((long)this.lowerBounds[i] + this.dimensions[i] - (stored ? 1 : 0) > int.MaxValue) throw new SqlException(SqlStates.ProgramLimitExceeded, "array upper bound is too large");
        }
        if (count != this.values.Length) throw Errors.Type("array shape does not match elements");
        for (var i = 0; i < this.values.Length; i++)
        {
            if (this.values[i].IsNull) this.values[i] = SqlValue.Null(elementType);
            else if (this.values[i].Type != elementType) throw Errors.Type("array element type mismatch");
        }
        Values = Array.AsReadOnly(this.values); Dimensions = Array.AsReadOnly(this.dimensions); LowerBounds = Array.AsReadOnly(this.lowerBounds);
    }
    public SqlValue Element(params long?[] indices)
    {
        if (indices.Length != Rank || Rank == 0) return SqlValue.Null(ElementType);
        long offset = 0;
        for (var i = 0; i < Rank; i++)
        {
            if (indices[i] is null) return SqlValue.Null(ElementType);
            var index = indices[i]!.Value;
            if (index < lowerBounds[i] || index >= (long)lowerBounds[i] + dimensions[i]) return SqlValue.Null(ElementType);
            offset = offset * dimensions[i] + index - lowerBounds[i];
        }
        return values[(int)offset];
    }
    public SqlArray Slice(IReadOnlyList<(long? Lower, long? Upper)> bounds)
    {
        if (Rank == 0 || bounds.Count > Rank) return new(ElementType, []);
        var starts = new int[Rank]; var sizes = new int[Rank];
        var total = 1;
        for (var i = 0; i < Rank; i++)
        {
            var low = i < bounds.Count ? bounds[i].Lower ?? lowerBounds[i] : lowerBounds[i];
            var high = i < bounds.Count ? bounds[i].Upper ?? (long)lowerBounds[i] + dimensions[i] - 1 : (long)lowerBounds[i] + dimensions[i] - 1;
            low = Math.Max(low, lowerBounds[i]); high = Math.Min(high, (long)lowerBounds[i] + dimensions[i] - 1);
            if (high < low) return new(ElementType, []);
            starts[i] = (int)(low - lowerBounds[i]); sizes[i] = (int)(high - low + 1); total *= sizes[i];
        }
        var output = new SqlValue[total];
        for (var i = 0; i < total; i++)
        {
            var remainder = i; var sourceOffset = 0; var stride = 1;
            for (var d = Rank - 1; d >= 0; d--)
            {
                sourceOffset += (starts[d] + remainder % sizes[d]) * stride;
                remainder /= sizes[d]; stride *= dimensions[d];
            }
            output[i] = values[sourceOffset];
        }
        return new(ElementType, output, sizes);
    }
    public override string ToString() => OpenInput.RenderArray(this);
}

/// <summary>An owned SQL record. A present record with null fields remains distinct from a whole-value NULL.</summary>
public sealed class SqlComposite
{
    public SqlType Type { get; }
    public IReadOnlyList<SqlValue> Fields { get; }
    public SqlComposite(SqlType type, IEnumerable<SqlValue> fields) : this(type, fields, false) { }
    internal SqlComposite(SqlType type, IEnumerable<SqlValue> fields, bool stored)
    {
        if (type.Kind != SqlKind.Composite) throw Errors.Type("record requires a composite type");
        var owned = fields.ToArray();
        if (owned.Length != type.Fields.Count) throw Errors.Type("record field count mismatch");
        for (var i = 0; i < owned.Length; i++)
        {
            if (owned[i].IsNull) owned[i] = SqlValue.Null(type.Fields[i].Type);
            else if (owned[i].Type != type.Fields[i].Type) throw Errors.Type("record field type mismatch");
            if (!stored)
            {
                var field = type.Fields[i];
                if (owned[i].IsNull && field.NotNull) throw new SqlException(SqlStates.NotNullViolation, "null composite field");
                if (!owned[i].IsNull) owned[i] = field.Type == SqlType.Interval ? SqlValue.Interval(IntervalInput.Adjust(owned[i].AsInterval(), field.IntervalFields, field.Precision)) :
                    Resolve.ApplyModifier(owned[i], new(field.Type, field.Precision, field.Scale, field.Length));
            }
        }
        Type = type; Fields = Array.AsReadOnly(owned);
    }
    public override string ToString() => OpenInput.RenderComposite(this);
}
