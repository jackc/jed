namespace Jed;

internal static class OpenCodec
{
    internal static void Write(WireWriter writer, SqlValue value, int depth)
    {
        switch (value.Type.Kind)
        {
            case SqlKind.Array:
            {
                var array = value.AsArray(); var nulls = array.Values.Any(v => v.IsNull);
                writer.Byte((byte)array.Rank); writer.Byte(nulls ? (byte)1 : (byte)0);
                for (var i = 0; i < array.Rank; i++) { writer.U32((uint)array.Dimensions[i]); writer.U32(unchecked((uint)array.LowerBounds[i])); }
                if (nulls) writer.Bytes(Bitmap(array.Values));
                foreach (var element in array.Values) if (!element.IsNull) StoredValues.WriteBody(writer, element, depth);
                return;
            }
            case SqlKind.Composite:
            {
                if (depth >= 32) throw new SqlException(SqlStates.StatementTooComplex, "composite nesting exceeds 32");
                var record = value.AsComposite(); writer.Bytes(Bitmap(record.Fields));
                foreach (var field in record.Fields) if (!field.IsNull) StoredValues.WriteBody(writer, field, depth + 1);
                return;
            }
            case SqlKind.Range:
            {
                var range = value.AsRange();
                if (range.IsEmpty) { writer.Byte(1); return; }
                writer.Byte((byte)((range.Lower.HasValue ? 0 : 2) | (range.Upper.HasValue ? 0 : 4) | (range.LowerInclusive ? 8 : 0) | (range.UpperInclusive ? 16 : 0)));
                if (range.Lower is { } lower) StoredValues.WriteBody(writer, lower, depth);
                if (range.Upper is { } upper) StoredValues.WriteBody(writer, upper, depth);
                return;
            }
        }
        throw Errors.Type("invalid open value");
    }
    private static byte[] Bitmap(IReadOnlyList<SqlValue> fields)
    {
        var bytes = new byte[(fields.Count + 7) / 8];
        for (var i = 0; i < fields.Count; i++) if (fields[i].IsNull) bytes[i / 8] |= (byte)(128 >> (i % 8));
        return bytes;
    }
    internal static SqlValue Read(ref WireReader reader, SqlType type, int depth)
    {
        if (type.Kind == SqlKind.Composite)
        {
            if (depth >= 32) throw StorageFormat.Corrupt("composite nesting exceeds 32");
            var bitmap = reader.Bytes((type.Fields.Count + 7) / 8).ToArray();
            var values = new SqlValue[type.Fields.Count];
            for (var i = 0; i < values.Length; i++)
                values[i] = (bitmap[i / 8] & (128 >> (i % 8))) != 0 ? SqlValue.Null(type.Fields[i].Type) : StoredValues.ReadBody(ref reader, type.Fields[i].Type, depth + 1);
            return SqlValue.Composite(new(type, values, true));
        }
        if (type.Kind == SqlKind.Range)
        {
            var flags = reader.Byte();
            if ((flags & 0xe0) != 0) throw StorageFormat.Corrupt("reserved range flags");
            if ((flags & 1) != 0) return SqlValue.Range(SqlRange.Empty(type));
            SqlValue? lower = (flags & 2) != 0 ? null : StoredValues.ReadBody(ref reader, type.Element!, depth);
            SqlValue? upper = (flags & 4) != 0 ? null : StoredValues.ReadBody(ref reader, type.Element!, depth);
            if (lower is { } lo && upper is { } hi)
            {
                var order = Evaluate.Compare(lo, hi);
                if (order > 0 || order == 0 && (flags & 24) != 24) throw StorageFormat.Corrupt("invalid range bounds");
            }
            return SqlValue.Range(SqlRange.Stored(type, lower, upper, (flags & 8) != 0, (flags & 16) != 0));
        }
        var rank = reader.Byte(); var arrayFlags = reader.Byte();
        if ((arrayFlags & 0xfe) != 0) throw StorageFormat.Corrupt("reserved array flags");
        if (rank == 0) return SqlValue.Array(new(type.Element!, []));
        if (rank > 6) throw StorageFormat.Corrupt("stored array dimensionality exceeds six");
        var dimensions = new int[rank]; var bounds = new int[rank]; long count = 1;
        for (var i = 0; i < rank; i++)
        {
            var length = reader.U32(); bounds[i] = unchecked((int)reader.U32());
            if (length == 0 || length > int.MaxValue || (long)bounds[i] + length - 1 > int.MaxValue) throw StorageFormat.Corrupt("invalid array dimension bounds");
            count *= length;
            if (count > 134217727) throw StorageFormat.Corrupt("array element count overflow");
            dimensions[i] = (int)length;
        }
        var nulls = (arrayFlags & 1) != 0 ? reader.Bytes(((int)count + 7) / 8).ToArray() : [];
        var elements = new List<SqlValue>((int)Math.Min(count, 1024));
        for (var i = 0; i < count; i++)
            elements.Add(nulls.Length != 0 && (nulls[i / 8] & (128 >> (i % 8))) != 0 ? SqlValue.Null(type.Element!) : StoredValues.ReadBody(ref reader, type.Element!, depth));
        return SqlValue.Array(new(type.Element!, elements, dimensions, bounds, true));
    }
    internal static void Key(WireWriter writer, SqlValue value, Collation? collation = null)
    {
        if (value.Type.Kind == SqlKind.Array)
        {
            var array = value.AsArray();
            if (array.ElementType.Kind == SqlKind.Composite) throw Errors.Unsupported("array of composite index key");
            foreach (var element in array.Values)
            {
                writer.Byte(element.IsNull ? (byte)2 : (byte)1);
                if (!element.IsNull) writer.Bytes(KeyEncoding.Encode(element, collation: collation));
            }
            writer.Byte(0); writer.Byte((byte)array.Rank);
            for (var i = 0; i < array.Rank; i++) { writer.U32((uint)array.Dimensions[i]); writer.Bytes(KeyEncoding.Integer(array.LowerBounds[i], 4)); }
            return;
        }
        if (value.Type.Kind == SqlKind.Composite)
        {
            foreach (var field in value.AsComposite().Fields) writer.Bytes(KeyEncoding.Encode(field, true));
            return;
        }
        var range = value.AsRange(); writer.Byte(range.IsEmpty ? (byte)0 : (byte)1);
        if (range.IsEmpty) return;
        if (range.Lower is { } lower)
        { writer.Byte(1); writer.Bytes(KeyEncoding.Encode(lower)); writer.Byte(range.LowerInclusive ? (byte)0 : (byte)1); }
        else writer.Byte(0);
        if (range.Upper is { } upper)
        { writer.Byte(1); writer.Bytes(KeyEncoding.Encode(upper)); writer.Byte(range.UpperInclusive ? (byte)1 : (byte)0); }
        else writer.Byte(2);
    }
}
