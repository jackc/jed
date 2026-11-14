// Generated data from 3e5962f9affc06c768df1688765162a65a893848.
// Regenerate only with mise run java-native:codegen.
package org.jed;

public final class OperatorCatalog {
    private OperatorCatalog() {}
    public record Fact(String name, String symbol, String kind, int arity, int precedence) {}
    public static final java.util.List<Fact> FACTS = java.util.List.of(
        new Fact("or", "", "logical", 2, 10),
        new Fact("and", "", "logical", 2, 20),
        new Fact("not", "", "logical", 1, 30),
        new Fact("eq", "=", "comparison", 2, 35),
        new Fact("ne", "<>", "comparison", 2, 35),
        new Fact("lt", "<", "comparison", 2, 35),
        new Fact("gt", ">", "comparison", 2, 35),
        new Fact("le", "<=", "comparison", 2, 35),
        new Fact("ge", ">=", "comparison", 2, 35),
        new Fact("is_distinct_from", "", "comparison", 2, 35),
        new Fact("is_not_distinct_from", "", "comparison", 2, 35),
        new Fact("like", "", "comparison", 2, 35),
        new Fact("ilike", "", "comparison", 2, 35),
        new Fact("regex_match", "~", "comparison", 2, 35),
        new Fact("regex_imatch", "~*", "comparison", 2, 35),
        new Fact("is_null", "", "null_test", 1, 35),
        new Fact("is_not_null", "", "null_test", 1, 35),
        new Fact("add", "+", "arithmetic", 2, 40),
        new Fact("sub", "-", "arithmetic", 2, 40),
        new Fact("mul", "*", "arithmetic", 2, 50),
        new Fact("div", "/", "arithmetic", 2, 50),
        new Fact("mod", "%", "arithmetic", 2, 50),
        new Fact("neg", "-", "arithmetic", 1, 60),
        new Fact("concat", "||", "concat", 2, 37),
        new Fact("contains", "@>", "containment", 2, 37),
        new Fact("contained_by", "<@", "containment", 2, 37),
        new Fact("overlaps", "&&", "containment", 2, 37),
        new Fact("strictly_left", "<<", "containment", 2, 37),
        new Fact("strictly_right", ">>", "containment", 2, 37),
        new Fact("not_extend_right", "&<", "containment", 2, 37),
        new Fact("not_extend_left", "&>", "containment", 2, 37),
        new Fact("adjacent", "-|-", "containment", 2, 37),
        new Fact("range_union", "+", "arithmetic", 2, 40),
        new Fact("range_difference", "-", "arithmetic", 2, 40),
        new Fact("range_intersect", "*", "arithmetic", 2, 50)
    );
}
