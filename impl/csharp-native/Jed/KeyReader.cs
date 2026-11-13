namespace Jed;

// Advances exactly one self-delimiting order key, leaving the index's row-key suffix untouched.
internal ref struct KeyReader(ReadOnlySpan<byte> key, bool descending)
{
    private readonly ReadOnlySpan<byte> input = key;
    private readonly byte mask = descending ? (byte)255 : (byte)0;
    internal int Position { get; private set; }
    private byte Byte()
    {
        if (Position >= input.Length) throw StorageFormat.Corrupt("truncated index key");
        return (byte)(input[Position++] ^ mask);
    }
    private void Bytes(int count)
    {
        if (count < 0 || count > input.Length - Position) throw StorageFormat.Corrupt("truncated index key");
        Position += count;
    }
    internal void Skip(SqlType type, bool nullable = false, int depth = 0, bool collated = false)
    {
        if (nullable)
        {
            var presence = Byte(); if (presence == 1) return;
            if (presence != 0) throw StorageFormat.Corrupt("invalid index presence tag");
        }
        switch (type.Kind)
        {
            case SqlKind.Boolean: Bytes(1); return;
            case SqlKind.I16: Bytes(2); return;
            case SqlKind.I32 or SqlKind.Date or SqlKind.F32: Bytes(4); return;
            case SqlKind.I64 or SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.F64: Bytes(8); return;
            case SqlKind.Uuid or SqlKind.Interval: Bytes(16); return;
            case SqlKind.Text or SqlKind.Bytea:
                if (collated && type.Kind == SqlKind.Text)
                    for (var level = 0; level < 3; level++) while ((Byte() << 8 | Byte()) != 0) { }
                while (true)
                {
                    if (Byte() != 0) continue;
                    var escape = Byte(); if (escape == 1) return;
                    if (escape != 255) throw StorageFormat.Corrupt("invalid text key escape");
                }
            case SqlKind.Decimal:
            {
                var sign = Byte(); if (sign == 4) return;
                if (sign is not (3 or 5)) throw StorageFormat.Corrupt("invalid decimal key sign");
                Bytes(4); var terminator = sign == 3 ? (byte)255 : (byte)0;
                while (Byte() != terminator) { }
                return;
            }
            case SqlKind.Composite:
                if (depth >= 32) throw StorageFormat.Corrupt("composite index nesting exceeds 32");
                foreach (var field in type.Fields) Skip(field.Type, true, depth + 1);
                return;
            case SqlKind.Array:
                while (true)
                {
                    var element = Byte(); if (element == 0) break;
                    if (element == 1) Skip(type.Element!, false, depth, collated);
                    else if (element != 2) throw StorageFormat.Corrupt("invalid array index element");
                }
                var rank = Byte(); Bytes(rank * 8); return;
            case SqlKind.Range:
            {
                var marker = Byte(); if (marker == 0) return;
                if (marker != 1) throw StorageFormat.Corrupt("invalid range index marker");
                var lower = Byte();
                if (lower == 1) { Skip(type.Element!, false, depth); if (Byte() > 1) throw StorageFormat.Corrupt("invalid range lower inclusivity"); }
                else if (lower != 0) throw StorageFormat.Corrupt("invalid range lower marker");
                var upper = Byte();
                if (upper == 1) { Skip(type.Element!, false, depth); if (Byte() > 1) throw StorageFormat.Corrupt("invalid range upper inclusivity"); }
                else if (upper != 2) throw StorageFormat.Corrupt("invalid range upper marker");
                return;
            }
            default: throw Errors.Unsupported("index key component type");
        }
    }
}
