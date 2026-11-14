package org.jed;

import java.util.List;

/** A FROM relation before the enclosing projection charges row production. */
interface RelationPlan extends QueryPlan {
    record FunctionalKey(List<Integer> determinant, List<Integer> dependent) {}
    default List<FunctionalKey> functionalKeys() { return List.of(); }
    List<ScalarPlan.Binding> bindings();
    @Override default List<CollationDerivation> collations() { return bindings().stream().map(ScalarPlan.Binding::collation).toList(); }
    void touch(boolean[] columns);
    default boolean baseIsland() { return false; }
    default boolean projectedResult() { return false; }
    default boolean orderedBy(List<SqlParser.Order> order) { return false; }
    default void prepare(CostMeter meter) {}
    default boolean pushScan(SqlParser.Expr predicate) { return push(predicate); }
    default boolean pushInJoin(SqlParser.Expr predicate) { return push(predicate); }
    default boolean push(SqlParser.Expr predicate) { return false; }
}
