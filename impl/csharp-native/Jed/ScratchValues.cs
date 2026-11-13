namespace Jed;

// Private scratch framing has no inline-cell length limit. Types remain in the
// owning plan; recursive values retain their exact scale, bounds and NULLs.
internal static class ScratchValues
{
    internal static void Write(WireWriter writer, SqlValue value)
    {
        writer.Byte(value.IsNull ? (byte)0 : (byte)1);
        if (value.IsNull) return;
        switch (value.Type.Kind)
        {
            case SqlKind.Text: Blob(writer, TextEncoding.Utf8.GetBytes(value.AsText())); return;
            case SqlKind.Bytea: Blob(writer, value.ByteSpan); return;
            case SqlKind.Json: Blob(writer, TextEncoding.Utf8.GetBytes(value.AsJsonText())); return;
            case SqlKind.Jsonpath: Blob(writer, TextEncoding.Utf8.GetBytes(value.AsJsonpath().Text)); return;
            case SqlKind.Array:
                var array = value.AsArray(); writer.Byte((byte)array.Rank); writer.U32((uint)array.Count);
                for (var i = 0; i < array.Rank; i++) { writer.U32((uint)array.Dimensions[i]); writer.U32(unchecked((uint)array.LowerBounds[i])); }
                foreach (var item in array.Values) Write(writer, item);
                return;
            case SqlKind.Composite:
                foreach (var field in value.AsComposite().Fields) Write(writer, field);
                return;
            case SqlKind.Range:
                var range = value.AsRange();
                writer.Byte((byte)((range.IsEmpty ? 1 : 0) | (range.LowerInclusive ? 2 : 0) | (range.UpperInclusive ? 4 : 0)));
                if (!range.IsEmpty) { Write(writer, range.Lower ?? SqlValue.Null(value.Type.Element!)); Write(writer, range.Upper ?? SqlValue.Null(value.Type.Element!)); }
                return;
            default: StoredValues.WriteBody(writer, value); return;
        }
    }
    internal static SqlValue Read(ref WireReader reader, SqlType type)
    {
        var present = reader.Byte();
        if (present == 0) return SqlValue.Null(type);
        if (present != 1) throw StorageFormat.Corrupt("invalid scratch NULL tag");
        switch (type.Kind)
        {
            case SqlKind.Text: return SqlValue.Text(WireReader.Utf8(Blob(ref reader)));
            case SqlKind.Bytea: return SqlValue.Bytes(Blob(ref reader));
            case SqlKind.Json: return SqlValue.StoredJson(WireReader.Utf8(Blob(ref reader)));
            case SqlKind.Jsonpath: return SqlValue.Jsonpath(SqlJsonPath.Parse(WireReader.Utf8(Blob(ref reader))));
            case SqlKind.Array:
                var rank = reader.Byte(); var count = checked((int)reader.U32());
                var dimensions = new int[rank]; var bounds = new int[rank];
                for (var i = 0; i < rank; i++) { dimensions[i] = checked((int)reader.U32()); bounds[i] = unchecked((int)reader.U32()); }
                var values = new SqlValue[count]; for (var i = 0; i < count; i++) values[i] = Read(ref reader, type.Element!);
                return SqlValue.Array(new(type.Element!, values, dimensions, bounds, true));
            case SqlKind.Composite:
                var fields = new SqlValue[type.Fields.Count];
                for (var i = 0; i < fields.Length; i++) fields[i] = Read(ref reader, type.Fields[i].Type);
                return SqlValue.Composite(new(type, fields, true));
            case SqlKind.Range:
                var flags = reader.Byte();
                if ((flags & 1) != 0) return SqlValue.Range(SqlRange.Empty(type));
                var lower = Read(ref reader, type.Element!); var upper = Read(ref reader, type.Element!);
                return SqlValue.Range(SqlRange.Stored(type, lower.IsNull ? (SqlValue?)null : lower, upper.IsNull ? (SqlValue?)null : upper, (flags & 2) != 0, (flags & 4) != 0));
            default: return StoredValues.ReadBody(ref reader, type);
        }
    }
    internal static byte[] Encode(IEnumerable<SqlValue> values)
    { var writer = new WireWriter(); foreach (var value in values) Write(writer, value); return writer.ToArray(); }
    internal static SqlValue[] Decode(byte[] bytes, IReadOnlyList<SqlType> types)
    {
        var reader = new WireReader(bytes); var values = new SqlValue[types.Count];
        for (var i = 0; i < values.Length; i++) values[i] = Read(ref reader, types[i]);
        reader.Finish(); return values;
    }
    private static void Blob(WireWriter writer, ReadOnlySpan<byte> bytes) { writer.U32((uint)bytes.Length); writer.Bytes(bytes); }
    private static ReadOnlySpan<byte> Blob(ref WireReader reader) => reader.Bytes(checked((int)reader.U32()));
}
