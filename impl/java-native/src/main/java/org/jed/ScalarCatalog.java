// Generated data from 3e5962f9affc06c768df1688765162a65a893848.
// Regenerate only with mise run java-native:codegen.
package org.jed;

public final class ScalarCatalog {
    private ScalarCatalog() {}
    public record Fact(String id, String family, long min, long max, int rank, java.util.List<String> aliases, String btreeOpclass) {}
    public static final java.util.List<Fact> FACTS = java.util.List.of(
        new Fact("i16", "integer", -32768L, 32767L, 1, java.util.List.of("i16", "smallint", "int2"), "int2_ops"),
        new Fact("i32", "integer", -2147483648L, 2147483647L, 2, java.util.List.of("i32", "int", "integer", "int4"), "int4_ops"),
        new Fact("i64", "integer", -9223372036854775808L, 9223372036854775807L, 3, java.util.List.of("i64", "bigint", "int8"), "int8_ops"),
        new Fact("boolean", "boolean", 0L, 0L, 0, java.util.List.of("boolean", "bool"), "bool_ops"),
        new Fact("text", "text", 0L, 0L, 0, java.util.List.of("text", "varchar", "character varying", "string"), "text_ops"),
        new Fact("decimal", "decimal", 0L, 0L, 0, java.util.List.of("decimal", "numeric", "dec"), "numeric_ops"),
        new Fact("bytea", "bytea", 0L, 0L, 0, java.util.List.of("bytea"), "bytea_ops"),
        new Fact("uuid", "uuid", 0L, 0L, 0, java.util.List.of("uuid"), "uuid_ops"),
        new Fact("timestamp", "timestamp", 0L, 0L, 0, java.util.List.of("timestamp", "timestamp without time zone"), "timestamp_ops"),
        new Fact("timestamptz", "timestamptz", 0L, 0L, 0, java.util.List.of("timestamptz", "timestamp with time zone"), "timestamptz_ops"),
        new Fact("interval", "interval", 0L, 0L, 0, java.util.List.of("interval"), "interval_ops"),
        new Fact("f32", "float", 0L, 0L, 1, java.util.List.of("f32", "real", "float4"), "float4_ops"),
        new Fact("f64", "float", 0L, 0L, 2, java.util.List.of("f64", "double precision", "float", "float8"), "float8_ops"),
        new Fact("date", "date", 0L, 0L, 0, java.util.List.of("date"), "date_ops"),
        new Fact("json", "json", 0L, 0L, 0, java.util.List.of("json"), ""),
        new Fact("jsonb", "jsonb", 0L, 0L, 0, java.util.List.of("jsonb"), "jsonb_ops"),
        new Fact("jsonpath", "jsonpath", 0L, 0L, 0, java.util.List.of("jsonpath"), "")
    );
}
