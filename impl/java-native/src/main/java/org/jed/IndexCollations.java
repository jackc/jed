package org.jed;

import java.util.List;

/** Persisted index derivation intentionally retains the format's older column/COLLATE/concat rule. */
final class IndexCollations {
    private IndexCollations() {}
    static String derive(SqlParser.Expr syntax, List<ScalarPlan.Binding> bindings) { return derivation(DefaultExpression.unwrap(syntax), bindings).require(); }
    private static CollationDerivation derivation(SqlParser.Expr syntax, List<ScalarPlan.Binding> bindings) {
        if (syntax instanceof SqlParser.Collate collate) return CollationDerivation.explicit(collate.name());
        if (syntax instanceof SqlParser.Binary binary && binary.op().equals("||")) return CollationDerivation.merge(derivation(binary.left(), bindings), derivation(binary.right(), bindings));
        if (syntax instanceof SqlParser.ColumnSlot slot) return bindings.get(slot.index()).collation();
        if (syntax instanceof SqlParser.Name name) {
            for (ScalarPlan.Binding binding : bindings) if (binding.name().equals(name.parts().getLast()) && (name.parts().size() == 1 || name.parts().subList(0, name.parts().size() - 1).equals(binding.qualifier()))) return binding.collation();
        }
        return CollationDerivation.NONE;
    }
    static byte[] key(SqlValue value, String name, Environment environment) {
        if (value.isNull()) throw new SqlException("23502", "NULL storage key");
        return name.equals("C") || !CollationDerivation.collatable(value.type()) ? Encoding.key(value) : CollationDerivation.key(value, environment.collation(name), null);
    }
}
