package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Iterative recursive CTE, with a fresh working table and one continuous statement meter. */
final class RecursivePlan implements QueryPlan {
    private final List<String> names;
    private final List<SqlType> types;
    private final List<CollationDerivation> collations;
    private final QueryEnvironment scope;
    private final SqlParser.Statement step;
    private final boolean all;
    private final int baseWidth;
    private final SearchCycle searchCycle;
    private QueryPlan anchor, preparedStep;
    private RowSpool working, result;
    private RowSpool.Cursor output;
    private DistinctState seen;
    private boolean started;

    RecursivePlan(SqlParser.Cte definition, QueryEnvironment scope) {
        SearchCycle.Marks marks = SearchCycle.marks(definition, scope);
        SqlParser.SetOperation union = validate(definition);
        this.scope = scope; all = union.all();
        anchor = scope.plan(union.left()); baseWidth = anchor.types().size();
        if (definition.columns().size() > baseWidth) { anchor.close(); throw new SqlException("42P10", "too many CTE aliases"); }
        List<String> names = new ArrayList<>(anchor.names());
        for (int i = 0; i < definition.columns().size(); i++) names.set(i, definition.columns().get(i));
        try { searchCycle = SearchCycle.present(definition) ? new SearchCycle(definition, names, anchor.types(), scope, marks) : null; }
        catch (RuntimeException | Error failure) { anchor.close(); throw failure; }
        this.names = searchCycle == null ? List.copyOf(names) : searchCycle.names();
        types = searchCycle == null ? anchor.types() : searchCycle.types();
        var collations = new ArrayList<>(anchor.collations());
        while (collations.size() < types.size()) collations.add(CollationDerivation.NONE);
        this.collations = List.copyOf(collations);
        try {
            if (!all) for (SqlType type : types) ScalarPlan.requireComparable(type);
            scope.cte(definition.name(), new CtePlan(this::workingReference, this.names, types, this.collations));
            step = searchCycle == null ? union.right() : searchCycle.recursive((SqlParser.Select)union.right(), definition.name(), scope);
            preparedStep = scope.plan(step);
            if (preparedStep.types().size() != types.size()) throw new SqlException("42601", "recursive terms have different column counts");
            for (int i = 0; i < types.size(); i++) {
                if (!collations.get(i).asColumn().equals(preparedStep.collations().get(i).asColumn())) throw new SqlException("42P21", "recursive term changes collation");
                try { if (!ScalarPlan.common(types.get(i), preparedStep.types().get(i)).equals(types.get(i))) throw SqlException.typeMismatch("recursive term widens anchor type"); }
                catch (SqlException failure) { throw SqlException.typeMismatch("recursive term does not preserve anchor type"); }
            }
        } catch (RuntimeException | Error failure) { close(); throw failure; }
    }
    private static SqlParser.SetOperation validate(SqlParser.Cte definition) {
        if (SearchCycle.present(definition) && definition.body() instanceof SqlParser.SetOperation shape) {
            if (!(shape.left() instanceof SqlParser.Select)) throw new SqlException("0A000", "SEARCH/CYCLE anchor must be one SELECT");
            if (!(shape.right() instanceof SqlParser.Select)) throw new SqlException("42601", "SEARCH/CYCLE recursive term must be one SELECT");
        }
        if (definition.body() instanceof SqlParser.QueryTail) throw new SqlException("0A000", "recursive query ORDER BY/LIMIT/OFFSET");
        if (!(definition.body() instanceof SqlParser.SetOperation union) || !union.kind().equals("union")) throw invalid("recursive query requires an anchor UNION recursive term");
        if (CteReferences.count(union.left(), definition.name()) != 0) throw invalid("self reference in recursive anchor");
        if (CteReferences.count(union.right(), definition.name()) != 1) throw invalid("recursive term must reference itself exactly once");
        if (!(union.right() instanceof SqlParser.Select select)) throw new SqlException("0A000", "recursive term query shape");
        if (AggregatePlan.needed(select)) throw invalid("aggregate in recursive term");
        int direct = 0;
        for (SqlParser.From from : select.from()) direct += direct(from, definition.name(), false);
        if (direct != 1) throw invalid("recursive reference inside expression subquery");
        return union;
    }
    private static int direct(SqlParser.From from, String name, boolean nullable) {
        if (from instanceof SqlParser.Table table && table.name().parts().equals(List.of(name))) {
            if (nullable) throw invalid("recursive reference on nullable join side"); return 1;
        }
        if (from instanceof SqlParser.Grouped group) return direct(group.source(), name, nullable);
        if (from instanceof SqlParser.JsonTable && CteReferences.from(from, name) != 0) throw invalid("recursive reference inside JSON_TABLE expression subquery");
        if (from instanceof SqlParser.Derived derived && CteReferences.count(derived.query(), name) != 0) throw new SqlException("0A000", "recursive reference in derived relation");
        if (from instanceof SqlParser.Join join) return direct(join.left(), name, nullable || join.kind().equals("right") || join.kind().equals("full")) + direct(join.right(), name, nullable || join.kind().equals("left") || join.kind().equals("full"));
        return 0;
    }
    private static SqlException invalid(String message) { return new SqlException("42P19", message); }
    private QueryPlan workingReference() {
        return new QueryPlan() {
            RowSpool.Cursor cursor;
            public List<String> names() { return names; }
            public List<SqlType> types() { return types; }
            public List<CollationDerivation> collations() { return collations; }
            public Row next(CostMeter meter) {
                if (cursor == null) cursor = working.cursor();
                List<SqlValue> values = cursor.next();
                if (values == null) return null;
                meter.charge(Costs.CTE_SCAN_ROW); meter.guard(); return new Row(names, values);
            }
            public void close() { if (cursor != null) cursor.close(); }
        };
    }
    private RowSpool evaluate(QueryPlan plan, CostMeter meter, boolean seed) {
        RowSpool next = new RowSpool(types, 0, null, meter.memory());
        try (plan) {
            Row row;
            while ((row = plan.next(meter)) != null) {
                List<SqlValue> values = new ArrayList<>();
                meter.guard();
                for (int i = 0; i < baseWidth; i++) values.add(SqlCasts.cast(row.get(i), types.get(i), meter));
                if (searchCycle != null) {
                    if (!seed) values.addAll(row.values().subList(baseWidth, row.values().size()));
                    List<SqlValue> generated = searchCycle.apply(values, seed, meter);
                    if (generated == null) continue;
                    values = generated;
                }
                if (seen != null) {
                    for (SqlValue value : values) meter.charge(MemoryAccount.payload(value) * Costs.HASH_KEY);
                    meter.guard(); if (!seen.add(values)) continue;
                }
                next.append(values); result.append(values);
            }
            return next;
        } catch (RuntimeException | Error failure) { next.close(); throw failure; }
    }
    @Override public List<String> names() { return names; }
    @Override public List<SqlType> types() { return types; }
    @Override public List<CollationDerivation> collations() { return collations; }
    @Override public Row next(CostMeter meter) {
        if (!started) {
            started = true; result = new RowSpool(types, 0, null, meter.memory());
            if (!all) seen = new DistinctState(null, 0, meter.memory());
            QueryPlan seed = anchor; anchor = null; working = evaluate(seed, meter, true);
            while (working.size() != 0) {
                meter.guard(); QueryPlan iteration = preparedStep == null ? scope.plan(step) : preparedStep; preparedStep = null;
                RowSpool next = evaluate(iteration, meter, false); working.close(); working = next;
            }
            working.close(); working = null; if (seen != null) { seen.close(); seen = null; }
            output = result.cursor();
        }
        List<SqlValue> values = output.next(); return values == null ? null : new Row(names, values);
    }
    @Override public void close() {
        if (anchor != null) anchor.close(); if (preparedStep != null) preparedStep.close();
        if (working != null) working.close(); if (result != null) result.close();
        if (output != null) output.close(); if (seen != null) seen.close();
    }
}
