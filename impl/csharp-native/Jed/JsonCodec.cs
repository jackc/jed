namespace Jed;

internal static class JsonCodec
{
    internal static void Write(WireWriter writer, SqlJsonNode value)
    {
        writer.Byte((byte)value.Kind);
        switch (value.Kind)
        {
            case SqlJsonKind.Number: StoredValues.WriteBody(writer, SqlValue.StoredDecimal(value.Number!)); break;
            case SqlJsonKind.String: String(writer, value.Text!); break;
            case SqlJsonKind.Array:
                Varint(writer, (uint)value.Elements.Count); foreach (var element in value.Elements) Write(writer, element); break;
            case SqlJsonKind.Object:
                Varint(writer, (uint)value.Members.Count);
                foreach (var member in value.Members) { writer.Byte((byte)SqlJsonKind.String); String(writer, member.Key); Write(writer, member.Value); } break;
        }
    }
    private static void String(WireWriter writer, string value) { var bytes = TextEncoding.Utf8.GetBytes(value); Varint(writer, (uint)bytes.Length); writer.Bytes(bytes); }
    private static void Varint(WireWriter writer, uint value)
    {
        do { var next = (byte)(value & 127); value >>= 7; writer.Byte(value == 0 ? next : (byte)(next | 128)); } while (value != 0);
    }
    private static int Varint(ref WireReader reader)
    {
        uint result = 0;
        for (var shift = 0; shift < 35; shift += 7)
        {
            var next = reader.Byte(); if (shift == 28 && next > 7) throw StorageFormat.Corrupt("jsonb length exceeds supported range");
            result |= (uint)(next & 127) << shift;
            if ((next & 128) == 0) { if (shift != 0 && next == 0) throw StorageFormat.Corrupt("noncanonical jsonb varint"); return (int)result; }
        }
        throw StorageFormat.Corrupt("invalid jsonb varint");
    }
    internal static SqlJsonNode Read(ref WireReader reader, int depth = 0)
    {
        var tag = reader.Byte();
        if (tag is 6 or 7 && depth == SqlJsonNode.MaximumDepth) throw StorageFormat.Corrupt("jsonb nesting depth exceeds the maximum of 256");
        switch (tag)
        {
            case 0: return SqlJsonNode.Null;
            case 1: case 2: return SqlJsonNode.Boolean(tag == 2);
            case 3: return new(SqlJsonKind.Number, number: StoredValues.ReadBody(ref reader, SqlType.Decimal).AsDecimal());
            case 4:
                var text = WireReader.Utf8(reader.Bytes(Varint(ref reader))); if (text.Contains('\0')) throw StorageFormat.Corrupt("NUL in jsonb string"); return new(SqlJsonKind.String, text: text);
            case 6:
                var count = Varint(ref reader); if (count > reader.Remaining) throw StorageFormat.Corrupt("truncated jsonb array");
                var elements = new SqlJsonNode[count]; for (var i = 0; i < count; i++) elements[i] = Read(ref reader, depth + 1); return new(SqlJsonKind.Array, elements: elements);
            case 7:
                var membersCount = Varint(ref reader); if (membersCount > reader.Remaining / 3) throw StorageFormat.Corrupt("truncated jsonb object");
                var members = new SqlJsonMember[membersCount];
                for (var i = 0; i < membersCount; i++)
                {
                    var key = Read(ref reader, depth + 1); if (key.Kind != SqlJsonKind.String) throw StorageFormat.Corrupt("jsonb object key is not a string");
                    if (i != 0 && JsonKeyComparer.Instance.Compare(members[i - 1].Key, key.Text) >= 0) throw StorageFormat.Corrupt("unordered jsonb object keys");
                    members[i] = new(key.Text!, Read(ref reader, depth + 1));
                }
                return new(SqlJsonKind.Object, members: members);
            default: throw StorageFormat.Corrupt("invalid or reserved jsonb node tag");
        }
    }
}
