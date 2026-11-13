// Generated data from b1a6affca7725e0b07a4f734eddb1c569ef76bcf.
// Regenerate only with mise run csharp-native:codegen.
namespace Jed;

internal static class ScalarCatalog
{
    internal static readonly string[] KnownNames = ["i16", "smallint", "int2", "i32", "int", "integer", "int4", "i64", "bigint", "int8", "boolean", "bool", "text", "varchar", "character varying", "string", "decimal", "numeric", "dec", "bytea", "uuid", "timestamp", "timestamp without time zone", "timestamptz", "timestamp with time zone", "interval", "f32", "real", "float4", "f64", "double precision", "float", "float8", "date", "json", "jsonb", "jsonpath"];
    internal static readonly ScalarFact[] Facts =
    [
        new(SqlType.I16, "i16", "integer", -32768L, 32767L, 1, ["i16", "smallint", "int2"]),
        new(SqlType.I32, "i32", "integer", -2147483648L, 2147483647L, 2, ["i32", "int", "integer", "int4"]),
        new(SqlType.I64, "i64", "integer", -9223372036854775808L, 9223372036854775807L, 3, ["i64", "bigint", "int8"]),
        new(SqlType.Boolean, "boolean", "boolean", 0L, 0L, 0, ["boolean", "bool"]),
        new(SqlType.Text, "text", "text", 0L, 0L, 0, ["text", "varchar", "character varying", "string"]),
    ];
}
