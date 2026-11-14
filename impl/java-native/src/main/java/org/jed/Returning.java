package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Resolve before mutation; evaluate the complete projection before publishing a new root. */
final class Returning implements AutoCloseable {
    private record Versions(List<SqlValue> oldRow, List<SqlValue> newRow) {}
    private final ScalarPlan.Frame frame = new ScalarPlan.Frame();
    private final List<String> names = new ArrayList<>();
    private final List<ScalarPlan.Expression> expressions = new ArrayList<>();
    private final List<Versions> pending = new ArrayList<>();
    private final List<SqlValue> nulls;
    private final boolean baseOld, oldVisible, newVisible;
    private final ScalarPlan.Resolver resolver;
    private RowSpool results;

    Returning(Engine.Table table, String alias, List<SqlParser.Item> items, String oldAlias, String newAlias, boolean baseOld, SqlValue[] args, QueryEnvironment environment) {
        this.baseOld = baseOld;
        String target = alias == null ? table.name() : alias;
        boolean oldRenamed = !oldAlias.equals("old"), newRenamed = !newAlias.equals("new");
        if (oldRenamed && oldAlias.equals(target) || newRenamed && newAlias.equals(target) || oldRenamed && newRenamed && oldAlias.equals(newAlias)) throw new SqlException("42712", "duplicate RETURNING relation alias");
        oldVisible = !target.equals(oldAlias) && !(newRenamed && newAlias.equals(oldAlias));
        newVisible = !target.equals(newAlias) && !(oldRenamed && oldAlias.equals(newAlias));
        nulls = table.types().stream().map(SqlValue::nullValue).toList();
        List<ScalarPlan.Binding> bindings = new ArrayList<>(table.bindings(alias));
        if (oldVisible) for (Engine.Column column : table.columns()) bindings.add(new ScalarPlan.Binding(List.of(oldAlias), column.name(), column.type(), true, false, CollationDerivation.collatable(column.type()) ? CollationDerivation.implicit(column.collation()) : CollationDerivation.NONE));
        if (newVisible) for (Engine.Column column : table.columns()) bindings.add(new ScalarPlan.Binding(List.of(newAlias), column.name(), column.type(), true, false, CollationDerivation.collatable(column.type()) ? CollationDerivation.implicit(column.collation()) : CollationDerivation.NONE));
        resolver = environment.deferred().resolver(bindings, frame);
        for (SqlParser.Item item : ProjectionSyntax.expand(items, resolver)) {
            if (item.expression() instanceof SqlParser.Star star) {
                String qualifier = star.qualifier().isEmpty() ? target : String.join(".", star.qualifier());
                if (!qualifier.equals(target) && !(oldVisible && qualifier.equals(oldAlias)) && !(newVisible && qualifier.equals(newAlias))) throw new SqlException("42P01", "unknown RETURNING relation");
                for (Engine.Column column : table.columns()) {
                    names.add(column.name());
                    expressions.add(resolver.resolve(new SqlParser.Name(List.of(qualifier, column.name())), null));
                }
            } else {
                names.add(item.alias() == null ? ScalarPlan.outputName(item.expression()) : item.alias());
                expressions.add(resolver.resolve(item.expression(), null));
            }
        }
    }
    boolean[] readTouched(boolean[] assigned) {
        boolean[] references = resolver.touched();
        boolean[] touched = new boolean[nulls.size()];
        int width = touched.length;
        for (int c = 0; c < width; c++) {
            touched[c] = references[c] && (baseOld || assigned == null || !assigned[c]);
            if (oldVisible) touched[c] |= references[width + c];
            if (newVisible && (assigned == null || !assigned[c])) touched[c] |= references[(oldVisible ? 2 : 1) * width + c] && !baseOld;
        }
        return touched;
    }
    void add(List<SqlValue> oldRow, List<SqlValue> newRow) {
        pending.add(new Versions(oldRow == null ? nulls : oldRow, newRow == null ? nulls : newRow));
    }
    QueryPlan finish(CostMeter meter) {
        List<SqlType> types = expressions.stream().map(ScalarPlan.Expression::type).toList();
        // RETURNING is a result collector under Q1; it cannot evade its memory limit by spilling.
        results = new RowSpool(types, 0, null, meter.memory());
        for (Versions versions : pending) {
            frame.values = new java.util.AbstractList<>() {
                public int size() { return nulls.size() * (1 + (oldVisible ? 1 : 0) + (newVisible ? 1 : 0)); }
                public SqlValue get(int index) {
                    int relation = index / nulls.size(), column = index % nulls.size();
                    return (relation == 0 ? baseOld ? versions.oldRow : versions.newRow : relation == 1 && oldVisible ? versions.oldRow : versions.newRow).get(column);
                }
            };
            meter.charge(Costs.ROW_PRODUCED); meter.guard();
            List<SqlValue> row = new ArrayList<>();
            for (ScalarPlan.Expression expression : expressions) row.add(expression.run(meter));
            results.append(row);
        }
        pending.clear(); frame.values = List.of();
        RowSpool.Cursor cursor = results.cursor();
        return new QueryPlan() {
            public List<String> names() { return List.copyOf(names); }
            public List<SqlType> types() { return types; }
            public List<CollationDerivation> collations() { return expressions.stream().map(ScalarPlan.Expression::collation).toList(); }
            public Row next(CostMeter ignored) { List<SqlValue> row = cursor.next(); return row == null ? null : new Row(names, row); }
            public void close() { Returning.this.close(); }
        };
    }
    @Override public void close() { pending.clear(); frame.values = List.of(); if (results != null) results.close(); }
}
