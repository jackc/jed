package org.jed;

import java.util.List;

/** Resident column facts used by the join search; compound/fenced inputs carry no invented facts. */
final class JoinedStatistics {
    private final List<TableRelation> tables;
    JoinedStatistics(List<TableRelation> tables) { this.tables = tables; }
    private record Column(int relation, long population, StatisticsEstimate.Distribution facts) {}
    private Column column(SqlParser.Expr expression) {
        if (!(expression instanceof SqlParser.Name)) return null;
        for (int i = 0; i < tables.size(); i++) {
            TableRelation table = tables.get(i);
            if (table == null || !FilterRules.belongs(expression, table.bindings())) continue;
            StatisticsEstimate statistics = table.statistics(); var facts = statistics.distribution(expression);
            return facts == null ? null : new Column(i, statistics.population(), facts);
        }
        return null;
    }
    Long buckets(SqlParser.Expr expression) {
        Column column = column(expression);
        return column == null ? null : PlanEstimate.add(column.facts().distinct(), column.facts().nonnull() < column.population() ? 1 : 0);
    }
    long rows(SqlParser.Expr expression, long population) {
        if (expression == null || population == 0) return population;
        if (expression instanceof SqlParser.Binary binary && binary.op().equals("=")) {
            Column left = column(binary.left()), right = column(binary.right());
            if (left != null && right != null && left.relation() != right.relation()) {
                long total = PlanEstimate.mul(left.population(), right.population());
                if (total == 0) return 0;
                long matches = StatisticsEstimate.scale(left.facts().nonnull(), right.facts().nonnull(), Math.max(1, Math.max(left.facts().distinct(), right.facts().distinct())));
                return StatisticsEstimate.scale(population, Math.min(total, matches), total);
            }
        }
        // Let a single table retain its full contradiction/paired-range/statistics program.
        if (!containsSlot(expression)) for (TableRelation table : tables)
            if (table != null && FilterRules.belongs(expression, table.bindings())) return table.statistics().rows(expression, population, List.of());
        if (expression instanceof SqlParser.Binary binary) {
            if (binary.op().equals("and")) return rows(binary.right(), rows(binary.left(), population));
            if (binary.op().equals("or")) return Math.min(population, PlanEstimate.add(rows(binary.left(), population), rows(binary.right(), population)));
        }
        if (expression instanceof SqlParser.Unary unary && unary.op().equals("not")) return Math.max(0, population - rows(unary.value(), population));
        return PlanEstimate.rows(expression, population, List.of());
    }
    private static boolean containsSlot(SqlParser.Expr expression) {
        if (expression == null) return false;
        if (expression instanceof SqlParser.ColumnSlot) return true;
        for (SqlParser.Expr child : DefaultExpression.children(expression)) if (containsSlot(child)) return true;
        return false;
    }
}
