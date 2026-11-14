// Generated data from 3e5962f9affc06c768df1688765162a65a893848.
// Regenerate only with mise run java-native:codegen.
package org.jed;

public final class SetReturningCatalog {
    private SetReturningCatalog() {}
    public record Fact(String name, int arity, java.util.List<String> families, String result, String column, String columnSource) {}
    public static final java.util.List<Fact> FACTS = java.util.List.of(
        new Fact("generate_series", 2, java.util.List.of("integer", "integer"), "set_of_promoted", "generate_series", "function"),
        new Fact("generate_series", 3, java.util.List.of("integer", "integer", "integer"), "set_of_promoted", "generate_series", "function"),
        new Fact("unnest", 1, java.util.List.of("anyarray"), "set_of_element", "unnest", "function"),
        new Fact("jsonb_array_elements", 1, java.util.List.of("jsonb"), "jsonb", "value", "out_param"),
        new Fact("jsonb_array_elements_text", 1, java.util.List.of("jsonb"), "text", "value", "out_param"),
        new Fact("json_array_elements", 1, java.util.List.of("json"), "json", "value", "out_param"),
        new Fact("json_array_elements_text", 1, java.util.List.of("json"), "text", "value", "out_param"),
        new Fact("jsonb_object_keys", 1, java.util.List.of("jsonb"), "text", "jsonb_object_keys", "function"),
        new Fact("json_object_keys", 1, java.util.List.of("json"), "text", "json_object_keys", "function"),
        new Fact("jsonb_each", 1, java.util.List.of("jsonb"), "jsonb", "value", "out_param"),
        new Fact("jsonb_each_text", 1, java.util.List.of("jsonb"), "text", "value", "out_param"),
        new Fact("json_each", 1, java.util.List.of("json"), "json", "value", "out_param"),
        new Fact("json_each_text", 1, java.util.List.of("json"), "text", "value", "out_param"),
        new Fact("jsonb_path_query", 2, java.util.List.of("jsonb", "jsonpath"), "jsonb", "jsonb_path_query", "function"),
        new Fact("regexp_matches", 2, java.util.List.of("text", "text"), "text[]", "regexp_matches", "function"),
        new Fact("regexp_matches", 3, java.util.List.of("text", "text", "text"), "text[]", "regexp_matches", "function"),
        new Fact("regexp_split_to_table", 2, java.util.List.of("text", "text"), "text", "regexp_split_to_table", "function"),
        new Fact("regexp_split_to_table", 3, java.util.List.of("text", "text", "text"), "text", "regexp_split_to_table", "function")
    );
}
