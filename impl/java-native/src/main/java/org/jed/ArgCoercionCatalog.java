// Generated data from 3e5962f9affc06c768df1688765162a65a893848.
// Regenerate only with mise run java-native:codegen.
package org.jed;

public final class ArgCoercionCatalog {
    private ArgCoercionCatalog() {}
    public record Fact(String from, String to, int cost) {}
    public static final java.util.List<Fact> FACTS = java.util.List.of(
        new Fact("i16", "i32", 1),
        new Fact("i32", "i64", 1),
        new Fact("i16", "i64", 2),
        new Fact("f32", "f64", 1),
        new Fact("integer_literal", "i32", 1),
        new Fact("integer_literal", "i16", 2),
        new Fact("i16", "decimal", 3),
        new Fact("i32", "decimal", 3),
        new Fact("i64", "decimal", 3),
        new Fact("integer_literal", "f64", 4),
        new Fact("integer_literal", "f32", 5),
        new Fact("decimal_literal", "f64", 4),
        new Fact("decimal_literal", "f32", 5)
    );
}
