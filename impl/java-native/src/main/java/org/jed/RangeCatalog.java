// Generated data from 3e5962f9affc06c768df1688765162a65a893848.
// Regenerate only with mise run java-native:codegen.
package org.jed;

public final class RangeCatalog {
    private RangeCatalog() {}
    public record Fact(String id, String element, boolean discrete, java.util.List<String> aliases) {}
    public static final java.util.List<Fact> FACTS = java.util.List.of(
        new Fact("i32range", "i32", true, java.util.List.of("i32range", "int4range")),
        new Fact("i64range", "i64", true, java.util.List.of("i64range", "int8range")),
        new Fact("numrange", "decimal", false, java.util.List.of("numrange")),
        new Fact("tsrange", "timestamp", false, java.util.List.of("tsrange")),
        new Fact("tstzrange", "timestamptz", false, java.util.List.of("tstzrange")),
        new Fact("daterange", "date", true, java.util.List.of("daterange"))
    );
}
