using System.Text.Json;
using Jed;

internal static class EncodingTests
{
    internal static void Run(string root)
    {
        var package = Path.Combine(root, "impl/csharp-native");
        var path = Path.Combine(package, ".build/encoding-vectors.json");
        using var document = JsonDocument.Parse(File.ReadAllBytes(path));
        using var baseline = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(package, "baseline.json")));
        Corpus.Equal(baseline.RootElement.GetProperty("semantic_revision").GetString(), document.RootElement.GetProperty("semantic_revision").GetString(), "encoding fixture baseline revision");
        var files = document.RootElement.GetProperty("files");
        var count = 0;
        var fileCount = 0;
        var seen = new HashSet<string>(StringComparer.Ordinal);
        foreach (var file in files.EnumerateArray())
        {
            var source = file.GetProperty("source").GetString()!;
            if (!seen.Add(source)) throw new InvalidDataException("duplicate encoding fixture source");
            var data = file.GetProperty("data");
            Corpus.Equal(1, data.GetProperty("schema_version").GetInt32(), "encoding fixture schema");
            foreach (var mode in new[] { "bare", "nullable", "descending" })
            {
                foreach (var group in data.GetProperty(mode).EnumerateArray())
                {
                    var type = FixtureType(group.GetProperty("type").GetString()!);
                    var nullable = mode == "nullable" || mode == "descending" && (!group.TryGetProperty("base", out var keyBase) || keyBase.GetString() == "nullable");
                    byte[]? previous = null;
                    foreach (var item in group.GetProperty("cases").EnumerateArray())
                    {
                        var value = Value(type, item);
                        var actual = KeyEncoding.Encode(value, nullable, mode == "descending");
                        Corpus.Equal(actual.Length, KeyEncoding.KeyLength([.. actual, 0x19, 0x42], type, nullable, mode == "descending"), "typed key component length before row suffix");
                        var expected = item.GetProperty("bytes").GetString()!;
                        Corpus.Equal(expected, Convert.ToHexStringLower(actual), $"{source} {mode} {type} key bytes");
                        if (previous is not null)
                        {
                            if (previous.AsSpan().SequenceCompareTo(actual) >= 0) throw new InvalidDataException($"{source} {mode} keys not strictly ordered");
                            if (actual.AsSpan().StartsWith(previous)) throw new InvalidDataException($"{source} {mode} key is not self-delimiting");
                        }
                        previous = actual;
                        if (type.IsInteger() && !value.IsNull)
                        {
                            var ascending = mode == "descending" ? actual.Select(b => (byte)(b ^ 255)).ToArray() : actual;
                            Corpus.Equal(value.AsInteger(), KeyEncoding.ReadInteger(ascending.AsSpan(nullable ? 1 : 0)), "integer key inverse");
                        }
                        var encodedValue = StoredValues.Encode(value);
                        var reader = new WireReader(encodedValue);
                        var restored = StoredValues.Decode(ref reader, type);
                        reader.Finish();
                        Corpus.Equal(value.Type, restored.Type, "stored value type round trip");
                        Corpus.Equal(value.ToString(), restored.ToString(), "stored value display round trip");
                        count++;
                    }
                }
            }
            fileCount++;
        }
        Corpus.Equal(8, fileCount, "all selected fixture files consumed");
        foreach (var name in new[] { "integers", "text", "bytea", "decimal", "float", "interval", "range", "composite" })
            if (!seen.Contains($"encoding/{name}.toml")) throw new InvalidDataException($"missing {name} encoding fixture");
        foreach (var type in new[] { SqlType.F32, SqlType.F64 })
        {
            var positive = SqlValue.Float(0, type);
            var negative = SqlValue.Float(-0d, type);
            Corpus.Equal(Convert.ToHexString(KeyEncoding.Encode(positive)), Convert.ToHexString(KeyEncoding.Encode(negative)), "float key collapses signed zero");
            if (StoredValues.Encode(positive).AsSpan().SequenceEqual(StoredValues.Encode(negative))) throw new InvalidDataException("float value codec erased zero sign");
            var negativeNan = SqlValue.Float(BitConverter.UInt64BitsToDouble(0xfff8000000000042UL), type);
            var positiveNan = SqlValue.Float(double.NaN, type);
            Corpus.Equal(Convert.ToHexString(StoredValues.Encode(positiveNan)), Convert.ToHexString(StoredValues.Encode(negativeNan)), "float value canonicalizes NaN payloads");
        }
        Console.WriteLine($"PASS value encoding: {fileCount} complete shared fixture files, {count} bare/nullable/descending vectors, strict order, prefix freedom and value round trips");
    }

    private static readonly SqlType Address = SqlType.Composite("addr", [new("street", SqlType.Text), new("zip", SqlType.I32)]);
    private static SqlType FixtureType(string name) => name switch
    {
        "addr" => Address,
        "line" => SqlType.Composite("line", [new("a", Address), new("b", Address)]),
        _ => SqlTypes.Parse(name)
    };
    private static SqlValue RangeBound(SqlType type, JsonElement value) => type.Kind switch
    {
        SqlKind.Date => SqlValue.Date(value.GetInt32()),
        SqlKind.Decimal => SqlValue.Decimal(ExactDecimal.Parse(value.GetString()!)),
        _ => SqlValue.Integer(value.GetInt64(), type)
    };
    private static SqlValue CompositeValue(SqlType type, JsonElement elements)
    {
        var values = new List<SqlValue>(); var index = 0;
        foreach (var element in elements.EnumerateArray())
        {
            var fieldType = type.Fields[index++].Type;
            if (element.TryGetProperty("null", out var absent) && absent.GetBoolean()) values.Add(SqlValue.Null(fieldType));
            else if (element.TryGetProperty("composite", out var nested)) values.Add(CompositeValue(fieldType, nested));
            else if (fieldType == SqlType.Text) values.Add(SqlValue.Text(element.GetProperty("text").GetString()!));
            else values.Add(SqlValue.Integer(element.GetProperty(fieldType.Name()).GetInt64(), fieldType));
        }
        return SqlValue.Composite(new(type, values));
    }
    private static SqlValue Value(SqlType type, JsonElement item)
    {
        if (item.TryGetProperty("null", out var absent) && absent.GetBoolean()) return SqlValue.Null(type);
        if (type.IsInteger()) return SqlValue.Integer(item.GetProperty("value").GetInt64(), type);
        if (type.Kind == SqlKind.Range)
        {
            if (item.TryGetProperty("empty", out var empty) && empty.GetBoolean()) return SqlValue.Range(SqlRange.Empty(type));
            SqlValue? lo = item.TryGetProperty("lower", out var lower) ? RangeBound(type.Element!, lower) : (SqlValue?)null;
            SqlValue? hi = item.TryGetProperty("upper", out var upper) ? RangeBound(type.Element!, upper) : (SqlValue?)null;
            return SqlValue.Range(new(type, lo, hi, item.TryGetProperty("lower_inc", out var li) && li.GetBoolean(), item.TryGetProperty("upper_inc", out var ui) && ui.GetBoolean()));
        }
        if (type.Kind == SqlKind.Composite)
            return CompositeValue(type, item.GetProperty("fields"));
        return type.Kind switch
        {
            SqlKind.Boolean => SqlValue.Boolean(item.GetProperty("value").GetBoolean()),
            SqlKind.Text => SqlValue.Text(item.GetProperty("value").GetString()!),
            SqlKind.Bytea => SqlValue.Bytes(Convert.FromHexString(item.GetProperty("hex").GetString()!)),
            SqlKind.Uuid => SqlValue.OwnedBytes(Uuid.Parse(item.GetProperty("value").GetString()!), SqlType.Uuid),
            SqlKind.Decimal => SqlValue.Decimal(ExactDecimal.Parse(item.GetProperty("value").GetString()!)),
            SqlKind.F32 or SqlKind.F64 => SqlValue.Float(FloatValue.Parse(item.GetProperty("value").GetString()!, type == SqlType.F32), type),
            SqlKind.Interval => SqlValue.Interval(new(item.GetProperty("months").GetInt32(), item.GetProperty("days").GetInt32(), item.GetProperty("micros").GetInt64())),
            _ => throw new InvalidDataException($"unsupported encoding fixture type {type}")
        };
    }
}
