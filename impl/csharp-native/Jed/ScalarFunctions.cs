using System.Globalization;
using System.Text;

namespace Jed;

internal static class ScalarFunctions
{
    internal static SqlValue Call(string name, SqlValue[] args, SqlType result, Meter meter)
    {
        if (name == "current_setting")
        {
            if (args.Any(argument => argument.IsNull)) return SqlValue.Null(SqlType.Text);
            var key = Session.VariableName(args[0].AsText());
            if (meter.Variables.TryGetValue(key, out var value)) return SqlValue.Text(value);
            if (args.Length == 2 && args[1].AsBoolean()) return SqlValue.Null(SqlType.Text);
            throw new SqlException("42704", "unrecognized configuration parameter: " + key);
        }
        if (name is "uuidv4" or "gen_random_uuid" or "uuidv7" or "uuid_extract_version" or "uuid_extract_timestamp") return UuidFunctions.Call(name, args, meter);
        if (name == "format") return TextFormat.Format(args, meter);
        if (name == "similar_to_escape") return SqlValue.Text(SimilarPattern.Translate(args[0].AsText(), args.Length == 2 ? args[1].AsText() : "\\").Text);
        if (name is "nextval" or "currval" or "setval" or "lastval") return meter.Sequences.Call(name, args);
        if (name == "to_char" && args[0].Type.Kind is SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.Interval)
            return DateTimeFormat.Render(args[0], args[1].AsText(), meter);
        if (name == "to_char") return NumberFormat.Render(args[0], args[1].AsText(), meter);
        if (name == "to_number") return NumberFormat.Parse(args[0].AsText(), args[1].AsText(), meter);
        if (name == "to_date" || name == "to_timestamp" && args.Length == 2)
            return DateTimeFormatInput.Parse(args[0].AsText(), args[1].AsText(), name == "to_date", meter);
        if (JsonFunctions.TryCall(name, args, meter, out var json)) return json;
        if (OpenFunctions.TryCall(name, args, result, meter, out var open)) return open;
        if (TemporalFunctions.TryCall(name, args, meter, out var temporal)) return temporal;
        if (name == "pi") return SqlValue.Float(FloatValue.Pi);
        if (args.Length > 0 && args[0].Type.IsFloat() && (result.IsFloat() || name == "width_bucket"))
        {
            var floating = args[0].AsFloat();
            if (name == "width_bucket") return SqlValue.Integer(FloatValue.WidthBucket(floating, args[1].AsFloat(), args[2].AsFloat(), args[3].AsInteger()), SqlType.I32);
            return SqlValue.Float(name switch
            {
                "round" => FloatValue.Round(floating, args.Length > 1 ? args[1].AsInteger() : 0),
                "pow" or "power" => FloatValue.Power(floating, args[1].AsFloat()),
                "atan2" => FloatValue.Atan2(floating, args[1].AsFloat()),
                "mod" => FloatValue.Binary("%", floating, args[1].AsFloat(), result == SqlType.F32),
                _ => FloatValue.Unary(name, floating)
            }, result);
        }
        if (DecimalFunctions.TryCall(name, args, result, meter, out var numeric)) return numeric;
        if (name is "num_nulls" or "num_nonnulls")
            return SqlValue.Integer(args.Count(v => v.IsNull == (name == "num_nulls")), SqlType.I32);
        if (name == "quote_nullable" && args[0].IsNull) return SqlValue.Text("NULL");
        if (name is "concat" or "concat_ws")
        {
            if (name == "concat_ws" && args[0].IsNull) return SqlValue.Null(SqlType.Text);
            var separator = name == "concat_ws" ? args[0].AsText() : "";
            var selected = args.Skip(name == "concat_ws" ? 1 : 0).Where(v => !v.IsNull).ToArray();
            var rendered = selected.Select(v => v.Type == SqlType.Boolean ? v.AsBoolean() ? "t" : "f" : TemporalRender.CastText(v, meter)).ToArray();
            if (name == "concat_ws") meter.ScalarOutput(rendered.Sum(text => (long)TextEncoding.ByteCount(text)) + Math.Max(0, rendered.Length - 1) * (long)TextEncoding.ByteCount(separator));
            return SqlValue.Text(string.Join(separator, rendered));
        }
        if (name == "abs")
        {
            if (args[0].Type == SqlType.Decimal) return SqlValue.Decimal(args[0].AsDecimal().Abs());
            if (args[0].AsInteger() == long.MinValue) throw Errors.Overflow();
            return SqlValue.Integer(Math.Abs(args[0].AsInteger()), result);
        }
        if (args.Length > 0 && args[0].Type == SqlType.Decimal)
        {
            var value = args[0].AsDecimal(); var places = args.Length > 1 && name is "round" or "trunc" ? args[1].AsInteger() : 0;
            void Work(long units, long bytes) { meter.Charge(units); meter.Guard(); meter.ScalarReserve(bytes); }
            switch (name)
            {
                case "round": return SqlValue.Decimal(value.Round(places));
                case "trunc": return SqlValue.Decimal(value.Truncate(places));
                case "ceil": case "ceiling": return SqlValue.Decimal(value.Ceiling());
                case "floor": return SqlValue.Decimal(value.Floor());
                case "scale": return SqlValue.Integer(value.Scale, SqlType.I32);
                case "min_scale": return SqlValue.Integer(value.TrimScale().Scale, SqlType.I32);
                case "trim_scale": return SqlValue.Decimal(value.TrimScale());
                case "sign": return SqlValue.Decimal(ExactDecimal.FromInt64(value.IsZero ? 0 : value.Negative ? -1 : 1));
                case "sqrt": return SqlValue.Decimal(value.Sqrt(Work));
                case "exp": return SqlValue.Decimal(value.Exp(Work));
                case "ln": return SqlValue.Decimal(value.Ln(Work));
                case "log10": return SqlValue.Decimal(value.Log10(Work));
                case "log": return SqlValue.Decimal(args.Length == 1 ? value.Log10(Work) : args[1].AsDecimal().Log(value, Work));
                case "power": case "pow": return SqlValue.Decimal(value.Power(args[1].AsDecimal(), Work));
                case "div": return SqlValue.Decimal(value.DivideAtScale(args[1].AsDecimal(), 0, round: false));
            }
        }
        if (name is "md5" or "sha224" or "sha256" or "sha384" or "sha512")
        {
            var bytes = args[0].Type == SqlType.Text ? TextEncoding.Utf8.GetBytes(args[0].AsText()) : args[0].AsBytes();
            var digest = CryptoProvider.Digest(name, bytes);
            return name == "md5" ? SqlValue.Text(Convert.ToHexStringLower(digest)) : SqlValue.OwnedBytes(digest);
        }
        if (name == "encode") return SqlValue.Text(Bytea.Encode(args[0].ByteSpan, args[1].AsText()));
        if (name == "decode") return SqlValue.OwnedBytes(Bytea.Decode(args[0].AsText(), args[1].AsText()));
        if (args.Length > 0 && args[0].Type == SqlType.Bytea)
        {
            var bytes = args[0].ByteSpan;
            return name switch
            {
                "length" or "octet_length" => SqlValue.Integer(bytes.Length, SqlType.I32),
                "bit_length" => SqlValue.Integer(checked(bytes.Length * 8L), SqlType.I32),
                "substr" or "substring" => SqlValue.OwnedBytes(Bytea.Substring(bytes, args[1].AsInteger(), args.Length == 3 ? args[2].AsInteger() : null)),
                "get_byte" => SqlValue.Integer(Bytea.GetByte(bytes, args[1].AsInteger()), SqlType.I32),
                "get_bit" => SqlValue.Integer(Bytea.GetBit(bytes, args[1].AsInteger()), SqlType.I32),
                "set_byte" => SqlValue.OwnedBytes(Bytea.SetByte(bytes, args[1].AsInteger(), args[2].AsInteger())),
                "set_bit" => SqlValue.OwnedBytes(Bytea.SetBit(bytes, args[1].AsInteger(), args[2].AsInteger())),
                "position" => SqlValue.Integer(Bytea.Position(bytes, args[1].ByteSpan), SqlType.I32),
                "btrim" or "ltrim" or "rtrim" => SqlValue.OwnedBytes(Bytea.Trim(bytes, args[1].ByteSpan, name != "rtrim", name != "ltrim")),
                "overlay" => SqlValue.OwnedBytes(Bytea.Overlay(bytes, args[1].ByteSpan, args[2].AsInteger(), args.Length == 4 ? args[3].AsInteger() : null)),
                _ => throw Errors.Unsupported($"bytea function {name}")
            };
        }
        if (name == "chr")
        {
            var cp = args[0].AsInteger();
            if (cp < 0) throw new SqlException(SqlStates.InvalidParameterValue, "negative character number");
            if (cp is 0 or > 0x10ffff or >= 0xd800 and <= 0xdfff) throw new SqlException(SqlStates.ProgramLimitExceeded, "invalid character number");
            return SqlValue.Text(new Rune((int)cp).ToString());
        }
        if (name == "to_hex") return SqlValue.Text(unchecked((ulong)args[0].AsInteger()).ToString("x", CultureInfo.InvariantCulture));
        if (args.Length > 0 && args[0].Type == SqlType.Text) return TextFunctions.Call(name, args, meter);
        throw Errors.Unsupported($"function {name}");
    }
}
