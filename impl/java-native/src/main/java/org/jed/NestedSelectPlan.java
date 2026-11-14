package org.jed;

import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.function.BiFunction;
import java.util.function.Function;

/** Build and initialize a chain of derived projections from the innermost relation outward. */
final class NestedSelectPlan implements QueryPlan {
    private record Layer(SqlParser.Select select, QueryEnvironment environment, List<String> aliases,
                         List<String> qualifier, Engine.View view) {}
    private final QueryPlan base, result;
    private final List<ProjectionPlan> stages;
    private final List<QueryEnvironment> scopes;
    private boolean initialized, closed;
    private NestedSelectPlan(QueryPlan base, QueryPlan result, List<ProjectionPlan> stages, List<QueryEnvironment> scopes) {
        this.base = base; this.result = result; this.stages = stages; this.scopes = scopes;
    }
    static QueryPlan resolve(SqlParser.Select select, QueryEnvironment environment,
                             Function<SqlParser.Name, Engine.View> views,
                             BiFunction<SqlParser.Statement, QueryEnvironment, QueryPlan> reader,
                             Path scratch, long workMem) {
        List<Layer> layers = new ArrayList<>();
        SqlParser.Statement current = select; QueryEnvironment scope = environment;
        while (current instanceof SqlParser.Select query && transparent(query)) {
            SqlParser.From from = query.from().getFirst();
            List<String> aliases, qualifier; Engine.View view = null;
            SqlParser.Statement body; QueryEnvironment child;
            if (from instanceof SqlParser.Derived derived && !derived.lateral()) {
                body = derived.query(); aliases = derived.columns(); qualifier = derived.alias() == null ? List.of() : List.of(derived.alias());
                child = scope.lexical().runtimeBody();
            } else if (from instanceof SqlParser.Table table && (table.name().parts().size() != 1 || scope.cte(table.name().parts().getFirst()) == null)
                    && !CatalogRelation.recognized(table.name()) && (view = views.apply(table.name())) != null) {
                scope.viewReference(view);
                if (table.columns().size() > view.columns().size()) throw new SqlException("42P10", "too many view column aliases");
                aliases = new ArrayList<>();
                for (int i = 0; i < view.columns().size(); i++) aliases.add(i < table.columns().size() ? table.columns().get(i) : view.columns().get(i).name());
                qualifier = List.of(table.alias() == null ? view.name() : table.alias()); body = view.query();
                child = scope.viewScope(view.name()).lexical().runtimeBody();
            } else break;
            layers.add(new Layer(query, scope, List.copyOf(aliases), qualifier, view));
            current = body; scope = child;
        }
        if (layers.size() < 8) return null;
        QueryPlan base = reader.apply(current, scope), result = base;
        List<ProjectionPlan> stages = new ArrayList<>(); List<QueryEnvironment> scopes = new ArrayList<>();
        try {
            for (int i = layers.size() - 1; i >= 0; i--) {
                Layer layer = layers.get(i); Alias input = new Alias(result, layer);
                ProjectionPlan projection = new ProjectionPlan(layer.select(), input, input.bindings(), layer.environment(), scratch, workMem);
                stages.add(projection); scopes.add(layer.environment()); result = projection;
            }
            return new NestedSelectPlan(base, result, List.copyOf(stages), List.copyOf(scopes));
        } catch (RuntimeException | Error failure) {
            for (ProjectionPlan stage : stages) stage.close(); base.close(); throw failure;
        }
    }
    private static boolean transparent(SqlParser.Select query) {
        return query.from().size() == 1 && query.where() == null && query.group().isEmpty() && query.having() == null
            && query.order().isEmpty() && query.limit() == null && query.offset() == null && !query.distinct()
            && query.windows().isEmpty() && query.items().stream().allMatch(item -> item.expression() instanceof SqlParser.Name || item.expression() instanceof SqlParser.Star || item.expression() instanceof SqlParser.ColumnSlot);
    }
    private static final class Alias implements RelationPlan {
        private final QueryPlan input;
        private final List<ScalarPlan.Binding> bindings;
        private final Layer layer;
        Alias(QueryPlan input, Layer layer) {
            this.input = input; this.layer = layer;
            if (layer.aliases().size() > input.types().size()) throw new SqlException("42P10", "too many derived column aliases");
            var columns = new ArrayList<ScalarPlan.Binding>();
            for (int i = 0; i < input.types().size(); i++) {
                String name = i < layer.aliases().size() ? layer.aliases().get(i) : input.names().get(i);
                ScalarPlan.Binding column = new ScalarPlan.Binding(layer.qualifier(), name, input.types().get(i), false, false, input.collations().get(i).asColumn());
                if (layer.view() != null) column = column.catalog(layer.view().columns().get(i).name(), false);
                columns.add(column);
            }
            bindings = List.copyOf(columns);
        }
        public List<ScalarPlan.Binding> bindings() { return bindings; }
        public List<String> names() { return bindings.stream().map(ScalarPlan.Binding::name).toList(); }
        public List<SqlType> types() { return input.types(); }
        public List<CollationDerivation> collations() { return bindings.stream().map(ScalarPlan.Binding::collation).toList(); }
        public void touch(boolean[] mask) { if (layer.view() != null) layer.environment().viewColumns(layer.view(), mask); }
        public boolean projectedResult() { return true; }
        public Row next(CostMeter meter) { return input.next(meter); }
        public void collect(RowSpool target, CostMeter meter) { input.collect(target, meter); }
        public PlanNode explain(boolean verbose) { PlanNode child = input.explain(verbose); return new PlanNode("Subquery" + (layer.qualifier().isEmpty() ? "" : " " + layer.qualifier().getFirst()), "", child.rows(), child.cost(), List.of(child), "buffered"); }
        public void requireAnalyze() { input.requireAnalyze(); }
        public PlanNode actualPlan(boolean verbose) { PlanNode child = input.actualPlan(verbose); return new PlanNode("Subquery" + (layer.qualifier().isEmpty() ? "" : " " + layer.qualifier().getFirst()), "", child.rows(), child.cost(), List.of(child), "buffered"); }
        public void close() { input.close(); }
    }
    public List<String> names() { return result.names(); }
    public List<SqlType> types() { return result.types(); }
    public List<CollationDerivation> collations() { return result.collations(); }
    public Row next(CostMeter meter) {
        if (!initialized) {
            initialized = true;
            for (int i = 0; i < stages.size(); i++) { scopes.get(i).initialize(meter); stages.get(i).prepareInput(meter); }
        }
        return result.next(meter);
    }
    public PlanNode explain(boolean verbose) { return result.explain(verbose); }
    public void requireAnalyze() { result.requireAnalyze(); }
    public PlanNode actualPlan(boolean verbose) { return result.actualPlan(verbose); }
    public void close() { if (closed) return; closed = true; for (ProjectionPlan stage : stages) stage.close(); base.close(); }
}
