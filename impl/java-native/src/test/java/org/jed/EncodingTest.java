package org.jed;

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.List;

/** Byte-exact canonical vectors generated from the experiment's frozen spec. */
public final class EncodingTest {
    private EncodingTest() {}
    public static void main(String[] args) throws Exception {
        List<String> vectors;
        if (args.length > 0) vectors = Files.readAllLines(Path.of(args[0]), StandardCharsets.UTF_8);
        else try (var stream = EncodingTest.class.getResourceAsStream("/encoding.tsv")) {
            if (stream == null) throw new AssertionError("Missing frozen encoding vectors");
            vectors = new BufferedReader(new InputStreamReader(stream, StandardCharsets.UTF_8)).lines().toList();
        }
        int count = 0;
        String previousGroup = null;
        byte[] previousKey = null;
        for (String line : vectors) {
            if (line.startsWith("#") || line.isEmpty()) continue;
            String[] fields = line.split("\t", -1);
            check(fields.length == 8, "vector field count");
            SqlType type = type(fields[4]);
            String value = new String(HexFormat.of().parseHex(fields[6]), StandardCharsets.UTF_8);
            SqlValue input = fields[5].equals("null") ? SqlValue.nullValue(type) : switch (type.kind()) {
                case I16, I32, I64 -> SqlValue.ofInteger(type, Long.parseLong(value));
                case BOOLEAN -> SqlValue.ofBoolean(Boolean.parseBoolean(value));
                case TEXT -> value.indexOf(0) >= 0 ? SqlValue.ofBytes(HexFormat.of().parseHex(fields[6])) : SqlValue.ofText(value);
                case BYTEA -> SqlValue.ofBytes(HexFormat.of().parseHex(fields[6]));
                case F32, F64 -> SqlValue.ofFloat(type, Double.parseDouble(value));
                case DECIMAL -> SqlValue.ofDecimal(Decimal.parse(value));
                case UUID -> SqlValue.ofUuid(SqlUuid.parse(value));
                case RANGE, COMPOSITE -> SqlCasts.literal(value, type);
                case ARRAY, DATE, TIMESTAMP, TIMESTAMPTZ -> SqlCasts.cast(SqlValue.ofText(value), type);
                default -> throw new AssertionError("unhandled generated vector type " + type);
            };
            boolean descending = fields[2].equals("descending");
            boolean nullable = fields[2].equals("nullable") || descending && fields[3].equals("nullable");
            byte[] actual = Encoding.key(input, nullable, descending);
            if (type.kind() == SqlType.Kind.TEXT && value.indexOf(0) >= 0) {
                // The canonical raw byte-escape vectors cover NUL even though SQL text forbids it.
                check(Arrays.equals(Encoding.key(input), Encoding.terminatedKey(input.asBytes())), "raw terminated-key seam");
            }
            byte[] expected = HexFormat.of().parseHex(fields[7]);
            check(Arrays.equals(expected, actual), "key vector " + count + " " + type + " " + value + ": expected " + fields[7] + ", got " + HexFormat.of().formatHex(actual));
            String group = String.join("/", Arrays.copyOf(fields, 5));
            if (group.equals(previousGroup)) check(Arrays.compareUnsigned(previousKey, actual) < 0, "strict key order " + count);
            previousGroup = group; previousKey = actual;
            List<SqlValue> decoded = Encoding.row(Encoding.value(input), List.of(input.type()));
            check(decoded.size() == 1 && decoded.getFirst().equals(input), "single-value roundtrip " + count);
            if (!input.isNull() && type.isFloat()) check(Double.doubleToRawLongBits(input.asFloat()) == Double.doubleToRawLongBits(decoded.getFirst().asFloat()), "stored float bit identity");
            count++;
        }
        malformed("02", SqlType.I32);
        malformed("0080", SqlType.I32);
        malformed("0002", SqlType.BOOLEAN);
        malformed("000001ff", SqlType.TEXT);
        malformed("000100000000", SqlType.DECIMAL); // Negative zero.
        malformed("0000000000010000", SqlType.DECIMAL); // Leading zero coefficient group.
        malformed("0000000000012710", SqlType.DECIMAL); // Group exceeds base 10^4.
        malformed("0100", SqlType.TEXT); // Trailing bytes after NULL.
        System.out.println("EncodingTest passed (" + count + " shared byte/order vectors)");
    }
    private static SqlType type(String name) {
        if (name.endsWith("[]")) return SqlType.array(type(name.substring(0, name.length() - 2)));
        return switch (name) {
            case "i16" -> SqlType.I16; case "i32" -> SqlType.I32; case "i64" -> SqlType.I64;
            case "boolean" -> SqlType.BOOLEAN; case "text" -> SqlType.TEXT; case "bytea" -> SqlType.BYTEA;
            case "decimal" -> SqlType.DECIMAL; case "f32" -> SqlType.F32; case "f64" -> SqlType.F64;
            case "uuid" -> SqlType.UUID; case "date" -> SqlType.DATE;
            case "timestamp" -> SqlType.TIMESTAMP; case "timestamptz" -> SqlType.TIMESTAMPTZ;
            case "i32range", "numrange" -> SqlType.named(name);
            case "addr" -> SqlType.composite("addr", List.of(new SqlType.Field("street", SqlType.TEXT), new SqlType.Field("zip", SqlType.I32)));
            default -> throw new AssertionError("unhandled vector type " + name);
        };
    }
    private static void malformed(String hex, SqlType type) {
        try { Encoding.row(HexFormat.of().parseHex(hex), List.of(type)); throw new AssertionError("malformed value accepted: " + hex); }
        catch (SqlException expected) { check(expected.sqlState().equals("XX001"), "malformed value SQLSTATE: " + expected.sqlState()); }
    }
    private static void check(boolean value, String message) { if (!value) throw new AssertionError(message); }
}
