using System.Buffers;
using System.Buffers.Binary;
using System.Text;

namespace Jed;

internal sealed class WireWriter
{
    private readonly ArrayBufferWriter<byte> buffer = new();
    internal int Length => buffer.WrittenCount;
    internal void Bytes(ReadOnlySpan<byte> bytes) { bytes.CopyTo(buffer.GetSpan(bytes.Length)); buffer.Advance(bytes.Length); }
    internal void Byte(byte value) { buffer.GetSpan(1)[0] = value; buffer.Advance(1); }
    internal void U16(ushort value) { BinaryPrimitives.WriteUInt16BigEndian(buffer.GetSpan(2), value); buffer.Advance(2); }
    internal void U32(uint value) { BinaryPrimitives.WriteUInt32BigEndian(buffer.GetSpan(4), value); buffer.Advance(4); }
    internal void U64(ulong value) { BinaryPrimitives.WriteUInt64BigEndian(buffer.GetSpan(8), value); buffer.Advance(8); }
    internal void String(string value)
    {
        var bytes = TextEncoding.Utf8.GetBytes(value);
        if (bytes.Length > ushort.MaxValue) throw Errors.Unsupported("oversize catalog string");
        U16((ushort)bytes.Length); Bytes(bytes);
    }
    internal byte[] ToArray() => buffer.WrittenSpan.ToArray();
}

internal ref struct WireReader(ReadOnlySpan<byte> input)
{
    private ReadOnlySpan<byte> rest = input;
    internal int Remaining => rest.Length;
    internal ReadOnlySpan<byte> Bytes(int count)
    {
        if (count < 0 || count > rest.Length) throw StorageFormat.Corrupt("truncated encoded value");
        var bytes = rest[..count]; rest = rest[count..]; return bytes;
    }
    internal byte Byte() => Bytes(1)[0];
    internal ushort U16() => BinaryPrimitives.ReadUInt16BigEndian(Bytes(2));
    internal uint U32() => BinaryPrimitives.ReadUInt32BigEndian(Bytes(4));
    internal ulong U64() => BinaryPrimitives.ReadUInt64BigEndian(Bytes(8));
    internal string String() => Utf8(Bytes(U16()));
    internal static string Utf8(ReadOnlySpan<byte> bytes)
    {
        try { return TextEncoding.Utf8.GetString(bytes); }
        catch (DecoderFallbackException) { throw StorageFormat.Corrupt("invalid stored UTF-8"); }
    }
    internal void Finish() { if (!rest.IsEmpty) throw StorageFormat.Corrupt("trailing encoded value bytes"); }
}

internal static class StoredValues
{
    internal static byte TypeCode(SqlType type) => type.Kind switch
    {
        SqlKind.I16 => 1, SqlKind.I32 => 2, SqlKind.I64 => 3, SqlKind.Text => 4, SqlKind.Boolean => 5,
        SqlKind.Decimal => 6, SqlKind.Bytea => 7, SqlKind.Uuid => 8, SqlKind.Timestamp => 9, SqlKind.Timestamptz => 10,
        SqlKind.Interval => 11, SqlKind.F64 => 12, SqlKind.F32 => 13, SqlKind.Composite => 14, SqlKind.Array => 15,
        SqlKind.Date => 16, SqlKind.Range => 17, SqlKind.Json => 18, SqlKind.Jsonb => 19, SqlKind.Jsonpath => 20,
        _ => throw Errors.Type("type cannot be stored")
    };
    internal static SqlType FromCode(byte code) => code switch
    {
        1 => SqlType.I16, 2 => SqlType.I32, 3 => SqlType.I64, 4 => SqlType.Text, 5 => SqlType.Boolean,
        6 => SqlType.Decimal, 7 => SqlType.Bytea, 8 => SqlType.Uuid, 9 => SqlType.Timestamp, 10 => SqlType.Timestamptz,
        11 => SqlType.Interval, 12 => SqlType.F64, 13 => SqlType.F32, 16 => SqlType.Date,
        18 => SqlType.Json, 19 => SqlType.Jsonb, 20 => SqlType.Jsonpath,
        _ => throw StorageFormat.Corrupt("invalid scalar type code")
    };
    internal static int Width(TypeSpec spec) => spec.Type.Kind switch
    {
        SqlKind.I16 => 2, SqlKind.I32 or SqlKind.Date or SqlKind.F32 => 4,
        SqlKind.I64 or SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.F64 => 8,
        SqlKind.Boolean => 1, SqlKind.Uuid or SqlKind.Interval => 16,
        SqlKind.Decimal when spec.Precision is > 0 and <= 18 => spec.Precision <= 4 ? 2 : spec.Precision <= 9 ? 4 : 8,
        _ => 0
    };
    internal static byte[] Encode(SqlValue value)
    {
        if (value.IsNull) return [1];
        var writer = new WireWriter(); writer.Byte(0); WriteBody(writer, value); return writer.ToArray();
    }
    internal static void WriteBody(WireWriter writer, SqlValue value, int depth = 0)
    {
        if (value.Type.IsInteger()) { writer.Bytes(KeyEncoding.Integer(value.AsInteger(), Width(new(value.Type)))); return; }
        switch (value.Type.Kind)
        {
            case SqlKind.Date: writer.Bytes(KeyEncoding.Integer(value.AsDate(), 4)); return;
            case SqlKind.Timestamp or SqlKind.Timestamptz: writer.Bytes(KeyEncoding.Integer(value.AsTimestamp(), 8)); return;
            case SqlKind.Interval:
                var interval = value.AsInterval();
                writer.U32(unchecked((uint)interval.Months)); writer.U32(unchecked((uint)interval.Days)); writer.U64(unchecked((ulong)interval.Micros)); return;
            case SqlKind.F32:
                writer.U32(double.IsNaN(value.AsFloat()) ? 0x7fc00000u : BitConverter.SingleToUInt32Bits((float)value.AsFloat())); return;
            case SqlKind.F64:
                writer.U64(double.IsNaN(value.AsFloat()) ? 0x7ff8000000000000UL : BitConverter.DoubleToUInt64Bits(value.AsFloat())); return;
            case SqlKind.Boolean: writer.Byte(value.AsBoolean() ? (byte)1 : (byte)0); return;
            case SqlKind.Text: writer.String(value.AsText()); return;
            case SqlKind.Json: writer.String(value.AsJsonText()); return;
            case SqlKind.Jsonpath: writer.String(value.AsJsonpath().Text); return;
            case SqlKind.Jsonb: JsonCodec.Write(writer, value.AsJsonb()); return;
            case SqlKind.Bytea:
                if (value.ByteSpan.Length > ushort.MaxValue) throw Errors.Unsupported("oversize inline binary value");
                writer.U16((ushort)value.ByteSpan.Length); writer.Bytes(value.ByteSpan); return;
            case SqlKind.Uuid: writer.Bytes(value.ByteSpan); return;
            case SqlKind.Decimal:
            {
                var number = value.AsDecimal();
                var groups = number.ExportBase10000();
                writer.Byte(number.Negative ? (byte)1 : (byte)0); writer.U16((ushort)number.Scale); writer.U16((ushort)groups.Length);
                foreach (var group in groups) writer.U16(group);
                return;
            }
            case SqlKind.Array or SqlKind.Composite or SqlKind.Range: OpenCodec.Write(writer, value, depth); return;
            default: throw Errors.Unsupported($"stored {value.Type.Name()} value");
        }
    }
    internal static SqlValue Decode(ref WireReader reader, SqlType type)
    {
        var tag = reader.Byte();
        if (tag == 1) return SqlValue.Null(type);
        if (tag != 0) throw StorageFormat.Corrupt("invalid inline value tag");
        return ReadBody(ref reader, type);
    }
    internal static SqlValue ReadBody(ref WireReader reader, SqlType type, int depth = 0)
    {
        if (type.IsInteger()) return SqlValue.Integer(KeyEncoding.ReadInteger(reader.Bytes(Width(new(type)))), type);
        switch (type.Kind)
        {
            case SqlKind.Date: return SqlValue.Date((int)KeyEncoding.ReadInteger(reader.Bytes(4)));
            case SqlKind.Timestamp or SqlKind.Timestamptz: return SqlValue.Timestamp(KeyEncoding.ReadInteger(reader.Bytes(8)), type == SqlType.Timestamptz);
            case SqlKind.Interval:
                return SqlValue.Interval(new(unchecked((int)reader.U32()), unchecked((int)reader.U32()), unchecked((long)reader.U64())));
            case SqlKind.F32: return SqlValue.Float(BitConverter.UInt32BitsToSingle(reader.U32()), type);
            case SqlKind.F64: return SqlValue.Float(BitConverter.UInt64BitsToDouble(reader.U64()), type);
            case SqlKind.Boolean:
                return reader.Byte() switch { 0 => SqlValue.Boolean(false), 1 => SqlValue.Boolean(true), _ => throw StorageFormat.Corrupt("invalid boolean") };
            case SqlKind.Text: return SqlValue.Text(reader.String());
            case SqlKind.Json: return SqlValue.StoredJson(reader.String());
            case SqlKind.Jsonpath: return SqlValue.Jsonpath(SqlJsonPath.Parse(reader.String()));
            case SqlKind.Jsonb: return SqlValue.Jsonb(JsonCodec.Read(ref reader));
            case SqlKind.Bytea: return SqlValue.OwnedBytes(reader.Bytes(reader.U16()).ToArray());
            case SqlKind.Uuid: return SqlValue.OwnedBytes(reader.Bytes(16).ToArray(), SqlType.Uuid);
            case SqlKind.Decimal:
            {
                var flags = reader.Byte(); var scale = reader.U16(); var count = reader.U16();
                if (flags > 1) throw StorageFormat.Corrupt("invalid decimal header flags");
                if (count > reader.Remaining / 2) throw StorageFormat.Corrupt("truncated decimal groups");
                var groups = new ushort[count];
                for (var i = 0; i < count; i++) groups[i] = reader.U16();
                return SqlValue.StoredDecimal(ExactDecimal.FromBase10000(flags == 1, scale, groups));
            }
            case SqlKind.Array or SqlKind.Composite or SqlKind.Range: return OpenCodec.Read(ref reader, type, depth);
            default: throw Errors.Unsupported($"reading stored {type.Name()} value");
        }
    }
    internal static byte[]? Leaf(SqlValue value, TypeSpec spec)
    {
        if (value.IsNull) return null;
        if (spec.Type == SqlType.Decimal && Width(spec) > 0)
        {
            var number = value.AsDecimal().Coerce(spec.Precision!.Value, spec.Scale!.Value);
            return KeyEncoding.Integer(number.ToUnscaledInt64(spec.Scale.Value), Width(spec));
        }
        if (spec.Type.Kind is SqlKind.Text or SqlKind.Bytea or SqlKind.Json)
        {
            var content = spec.Type == SqlType.Bytea ? value.AsBytes() : TextEncoding.Utf8.GetBytes(spec.Type == SqlType.Json ? value.AsJsonText() : value.AsText());
            return [5, .. content];
        }
        var encoded = Encode(value);
        return Width(spec) > 0 ? encoded[1..] : encoded;
    }
    internal static SqlValue FromLeaf(ReadOnlyMemory<byte>? cell, TypeSpec spec)
    {
        if (cell is null) return SqlValue.Null(spec.Type);
        var bytes = cell.Value.Span;
        if (spec.Type == SqlType.Decimal && Width(spec) > 0)
            return SqlValue.Decimal(ExactDecimal.FromUnscaled(KeyEncoding.ReadInteger(bytes), spec.Scale!.Value));
        if (spec.Type.Kind is SqlKind.Text or SqlKind.Bytea or SqlKind.Json && bytes.Length > 0 && bytes[0] == 5)
            return spec.Type == SqlType.Bytea ? SqlValue.OwnedBytes(bytes[1..].ToArray()) : spec.Type == SqlType.Json ? SqlValue.StoredJson(WireReader.Utf8(bytes[1..])) : SqlValue.Text(WireReader.Utf8(bytes[1..]));
        var reader = new WireReader(bytes);
        var value = Width(spec) > 0 ? ReadBody(ref reader, spec.Type) : Decode(ref reader, spec.Type);
        reader.Finish(); return value;
    }
}

internal static class KeyEncoding
{
    internal static int KeyLength(ReadOnlySpan<byte> key, SqlType type, bool nullable = false, bool descending = false)
    {
        var reader = new KeyReader(key, descending);
        reader.Skip(type, nullable);
        return reader.Position;
    }
    internal static byte[] Integer(long value, int width)
    {
        var bytes = new byte[width];
        switch (width)
        {
            case 2: BinaryPrimitives.WriteUInt16BigEndian(bytes, unchecked((ushort)((short)value ^ short.MinValue))); break;
            case 4: BinaryPrimitives.WriteUInt32BigEndian(bytes, unchecked((uint)((int)value ^ int.MinValue))); break;
            case 8: BinaryPrimitives.WriteUInt64BigEndian(bytes, unchecked((ulong)(value ^ long.MinValue))); break;
            default: throw new ArgumentOutOfRangeException(nameof(width));
        }
        return bytes;
    }
    internal static long ReadInteger(ReadOnlySpan<byte> bytes) => bytes.Length switch
    {
        2 => unchecked((short)(BinaryPrimitives.ReadUInt16BigEndian(bytes) ^ 0x8000)),
        4 => unchecked((int)(BinaryPrimitives.ReadUInt32BigEndian(bytes) ^ 0x80000000u)),
        8 => unchecked((long)(BinaryPrimitives.ReadUInt64BigEndian(bytes) ^ 0x8000000000000000UL)),
        _ => throw StorageFormat.Corrupt("invalid fixed integer width")
    };
    internal static byte[] Encode(SqlValue value, bool nullable = false, bool descending = false)
    {
        var writer = new WireWriter();
        if (value.IsNull)
        {
            if (!nullable) throw new SqlException(SqlStates.NotNullViolation, "NULL key");
            writer.Byte(1);
        }
        else
        {
            if (nullable) writer.Byte(0);
            if (value.Type.IsInteger()) writer.Bytes(Integer(value.AsInteger(), StoredValues.Width(new(value.Type))));
            else switch (value.Type.Kind)
            {
                case SqlKind.Date: writer.Bytes(Integer(value.AsDate(), 4)); break;
                case SqlKind.Timestamp or SqlKind.Timestamptz: writer.Bytes(Integer(value.AsTimestamp(), 8)); break;
                case SqlKind.Interval:
                    var spanBytes = new byte[16];
                    BinaryPrimitives.WriteInt128BigEndian(spanBytes, value.AsInterval().Span);
                    spanBytes[0] ^= 128; writer.Bytes(spanBytes); break;
                case SqlKind.F32:
                    var single = value.AsFloat();
                    var bits32 = double.IsNaN(single) ? 0x7fc00000u : BitConverter.SingleToUInt32Bits(single == 0 ? 0f : (float)single);
                    writer.U32((bits32 & 0x80000000u) != 0 ? ~bits32 : bits32 ^ 0x80000000u); break;
                case SqlKind.F64:
                    var number = value.AsFloat();
                    var bits64 = double.IsNaN(number) ? 0x7ff8000000000000UL : BitConverter.DoubleToUInt64Bits(number == 0 ? 0d : number);
                    writer.U64((bits64 & 0x8000000000000000UL) != 0 ? ~bits64 : bits64 ^ 0x8000000000000000UL); break;
                case SqlKind.Boolean: writer.Byte(value.AsBoolean() ? (byte)1 : (byte)0); break;
                case SqlKind.Uuid: writer.Bytes(value.ByteSpan); break;
                case SqlKind.Text: Escape(writer, TextEncoding.Utf8.GetBytes(value.AsText())); break;
                case SqlKind.Bytea: Escape(writer, value.ByteSpan); break;
                case SqlKind.Decimal: writer.Bytes(Decimal(value.AsDecimal())); break;
                case SqlKind.Array or SqlKind.Composite or SqlKind.Range: OpenCodec.Key(writer, value); break;
                default: throw Errors.Unsupported($"key of type {value.Type.Name()}");
            }
        }
        var bytes = writer.ToArray();
        if (descending) for (var i = 0; i < bytes.Length; i++) bytes[i] = (byte)(bytes[i] ^ 255);
        return bytes;
    }
    private static void Escape(WireWriter writer, ReadOnlySpan<byte> bytes)
    {
        foreach (var b in bytes) { writer.Byte(b); if (b == 0) writer.Byte(255); }
        writer.Byte(0); writer.Byte(1);
    }
    private static byte[] Decimal(ExactDecimal value)
    {
        if (value.IsZero) return [4];
        var digits = value.CoefficientDigits();
        var point = digits.Length - value.Scale;
        digits = digits.TrimEnd('0');
        if ((point & 1) != 0) digits = "0" + digits;
        if ((digits.Length & 1) != 0) digits += "0";
        // Floor division matters for sub-unit values with negative decimal point.
        var twiceExponent = point + 1;
        var exponent = twiceExponent >= 0 ? twiceExponent / 2 : (twiceExponent - 1) / 2;
        var result = new byte[6 + digits.Length / 2];
        result[0] = value.Negative ? (byte)3 : (byte)5;
        Integer(exponent, 4).CopyTo(result, 1);
        for (var i = 0; i < digits.Length / 2; i++) result[5 + i] = (byte)((digits[i * 2] - '0') * 10 + digits[i * 2 + 1] - '0' + 1);
        if (value.Negative) for (var i = 1; i < result.Length; i++) result[i] ^= 255;
        return result;
    }
}
