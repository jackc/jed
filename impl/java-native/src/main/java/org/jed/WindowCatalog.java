// Generated data from 3e5962f9affc06c768df1688765162a65a893848.
// Regenerate only with mise run java-native:codegen.
package org.jed;

public final class WindowCatalog {
    private WindowCatalog() {}
    public record Fact(String name, String arguments, String result, boolean frameSensitive) {}
    public static final java.util.List<Fact> FACTS = java.util.List.of(
        new Fact("row_number", "none", "i64", false),
        new Fact("rank", "none", "i64", false),
        new Fact("dense_rank", "none", "i64", false),
        new Fact("percent_rank", "none", "f64", false),
        new Fact("cume_dist", "none", "f64", false),
        new Fact("ntile", "one", "i64", false),
        new Fact("lag", "value_offset_default", "same_as_input", false),
        new Fact("lead", "value_offset_default", "same_as_input", false),
        new Fact("first_value", "one", "same_as_input", true),
        new Fact("last_value", "one", "same_as_input", true),
        new Fact("nth_value", "value_n", "same_as_input", true)
    );
}
