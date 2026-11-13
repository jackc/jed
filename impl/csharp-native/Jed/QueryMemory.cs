namespace Jed;

// Representation-independent logical bytes. No size is computed on the
// unlimited path; callers test Enabled before measuring buffered values.
internal static class MemorySize
{
    internal static long SortKeyBytes(IEnumerable<SortValue> keys) => keys.Aggregate(0L, (bytes, key) => key.Key is null ? bytes : Meter.Add(bytes, MemorySchedule.Value + key.Key.Length));
    internal static long RowBytes(IReadOnlyList<SqlValue> row, IReadOnlyList<bool>? touched = null)
    {
        var bytes = MemorySchedule.Row;
        for (var i = 0; i < row.Count; i++) bytes = Meter.Add(bytes,
            touched is not null && i < touched.Count && !touched[i] ? MemorySchedule.Value : ValueBytes(row[i]));
        return bytes;
    }
    internal static long ValueBytes(SqlValue value)
    {
        if (value.IsNull || value.IsDeferred) return MemorySchedule.Value;
        var bytes = MemorySchedule.Value;
        switch (value.Type.Kind)
        {
            case SqlKind.Text: case SqlKind.Json: case SqlKind.Jsonpath: return bytes + value.TextBytes;
            case SqlKind.Bytea: return bytes + value.ByteSpan.Length;
            case SqlKind.Decimal: return bytes + DecimalBytes(value.AsDecimal());
            case SqlKind.Jsonb: return JsonBytes(value.AsJsonb());
            case SqlKind.Array:
                bytes += value.AsArray().Rank * MemorySchedule.ArrayDim;
                foreach (var element in value.AsArray().Values) bytes = Meter.Add(bytes, ValueBytes(element));
                return bytes;
            case SqlKind.Composite:
                foreach (var field in value.AsComposite().Fields) bytes = Meter.Add(bytes, ValueBytes(field));
                return bytes;
            case SqlKind.Range:
                if (value.AsRange().Lower is { } lower) bytes = Meter.Add(bytes, ValueBytes(lower));
                if (value.AsRange().Upper is { } upper) bytes = Meter.Add(bytes, ValueBytes(upper));
                return bytes;
            default: return bytes;
        }
    }
    private static long DecimalBytes(ExactDecimal value) => value.IsZero ? 0 : (value.Digits + 3L) / 4 * MemorySchedule.DecimalGroup;
    private static long JsonBytes(SqlJsonNode value)
    {
        var bytes = MemorySchedule.Value;
        switch (value.Kind)
        {
            case SqlJsonKind.Number: return bytes + DecimalBytes(value.Number!);
            case SqlJsonKind.String: return bytes + TextEncoding.ByteCount(value.Text!);
            case SqlJsonKind.Array:
                foreach (var child in value.Elements) bytes = Meter.Add(bytes, JsonBytes(child));
                return bytes;
            case SqlJsonKind.Object:
                foreach (var child in value.Members) bytes = Meter.Add(bytes, Meter.Add(TextEncoding.ByteCount(child.Key), JsonBytes(child.Value)));
                return bytes;
            default: return bytes;
        }
    }
    internal static long KeyBytes(IReadOnlyList<SqlValue> values)
    {
        long bytes = 0; foreach (var value in values) bytes = Meter.Add(bytes, ValueBytes(value)); return bytes;
    }
    internal static long EntryBytes(IReadOnlyList<SqlValue> values) => Meter.Add(MemorySchedule.Entry, KeyBytes(values));
}

internal sealed class QueryMemoryAccount(long limit, Meter meter)
{
    internal bool Enabled => limit > 0;
    internal long Used { get; private set; }
    internal long Peak { get; private set; }
    internal long Pending { get; private set; }
    internal int ReleaseUnderflows { get; private set; }
    internal bool TryReserve(long bytes)
    {
        if (!Enabled) return true;
        if (bytes < 0) throw new ArgumentOutOfRangeException(nameof(bytes));
        if (bytes > limit - Used) return false;
        Used += bytes; Peak = Math.Max(Peak, Used); return true;
    }
    internal void Reserve(long bytes)
    {
        if (TryReserve(bytes)) return;
        meter.Guard();
        throw new SqlException(SqlStates.QueryMemoryLimitExceeded, $"query memory exceeded the limit of {limit} bytes");
    }
    internal void Release(long bytes)
    {
        if (!Enabled) return;
        if (bytes < 0) throw new ArgumentOutOfRangeException(nameof(bytes));
        if (bytes > Used - Pending) { ReleaseUnderflows++; Used = Pending; }
        else Used -= bytes;
    }
    internal void HoldPending(long bytes)
    {
        if (!Enabled) return;
        if (bytes < 0) throw new ArgumentOutOfRangeException(nameof(bytes));
        if (bytes > Pending) Reserve(bytes - Pending);
        else Used -= Pending - bytes;
        Pending = bytes;
    }
    internal long ReserveRow(IReadOnlyList<SqlValue> row, IReadOnlyList<bool>? touched = null)
    {
        if (!Enabled) return 0;
        var bytes = MemorySize.RowBytes(row, touched); Reserve(bytes); return bytes;
    }
    internal long ReserveValue(SqlValue value)
    {
        if (!Enabled) return 0;
        var bytes = MemorySize.ValueBytes(value); Reserve(bytes); return bytes;
    }
}
