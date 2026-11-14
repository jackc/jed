package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import org.jed.storage.PersistentTree;

/** A lazy base relation for multi-relation plans; predicate evaluation retains scan order. */
final class TableRelation implements RelationPlan {
    private final Engine.Table table;
    private final boolean hostAttachment;
    private final SqlParser.Table syntax;
    private SqlParser.Expr predicate;
    private SqlParser.Expr accessPredicate;
    private SqlParser.Expr accessWhere;
    record Probe(String kind, Engine.Index index, List<String> keys, SqlParser.Expr access, ScalarPlan.Resolver estimates) {
        Probe(String kind, Engine.Index index, List<String> keys, SqlParser.Expr access) { this(kind, index, keys, access, null); }
    }
    private Probe probe;
    private List<AccessCandidate> accessOptions;
    private AccessCandidate selectedAccess;
    List<AccessCandidate> accessOptions() {
        if (accessOptions == null) accessOptions = AccessCandidate.inventory(table, names(), syntax.alias() == null ? table.name() : syntax.alias(), accessFilter(), resolver, environment.meter(), hostAttachment);
        return accessOptions;
    }
    void selectAccess(AccessCandidate candidate) { selectedAccess = candidate; }
    long logicalRows() { return statistics().rows(TableExplanation.orderedPredicate(predicate, environment), table.rows().size(), table.primaryKey().length == 1 ? List.of(names().get(table.primaryKey()[0])) : List.of()); }
    PlanNode accessEstimate(AccessCandidate candidate, boolean verbose) {
        ExplainExpression printer = new ExplainExpression(bindings, resolver, environment.meter());
        String detail = switch (candidate.kind()) {
            case AccessCandidate.PK -> {
                List<String> parts = new ArrayList<>();
                for (SqlParser.Expr term : FilterRules.terms(candidate.predicate())) {
                    var bound = (SqlParser.Binary)term;
                    parts.add(((SqlParser.Name)bound.left()).parts().getLast() + " " + bound.op() + " " + printer.print(bound.right()));
                }
                yield "PK bound: " + String.join(" and ", parts);
            }
            case AccessCandidate.BTREE, AccessCandidate.INDEX_SET -> TableExplanation.indexDetail(new IndexAccess.Selection(candidate.index(), candidate.predicate(), candidate.rows(), candidate.pages(), candidate.scanCost()), names());
            case AccessCandidate.GIST -> "GiST bound: using " + candidate.name();
            case AccessCandidate.GIN -> "GIN bound: using " + candidate.name();
            case AccessCandidate.PK_SET -> {
                try {
                    var set = KeySetEstimate.primary(table, names(), syntax.alias() == null ? table.name() : syntax.alias(), candidate.predicate(), resolver, environment.meter());
                    yield "PK interval set: " + String.join(", ", set.columns()) + "; intervals=" + set.intervals() + (set.arrays() == 0 ? "" : "; arrays=" + set.arrays());
                } catch (IOException failure) { throw Engine.storageError(failure); }
            }
            default -> "Full scan";
        };
        int count = 0; for (boolean column : touched) if (column) count++;
        if (count != 0) detail += "; touched=" + count;
        long rows = candidate.rows(), cost = candidate.scanCost();
        if (predicate != null) {
            cost = PlanEstimate.add(cost, PlanEstimate.add(PlanEstimate.mul(rows, PlanEstimate.operations(predicate, resolver, environment.meter())), environment.hiddenEstimate(predicate, rows)));
            rows = Math.min(rows, logicalRows());
            detail += "; " + (verbose ? "filter=" + printer.print(TableExplanation.orderedPredicate(predicate, environment)) : "filter:conjuncts=" + FilterRules.terms(predicate).size());
        }
        if (environment.contradiction(predicate)) { rows = 0; cost = 0; }
        return new PlanNode("Scan " + table.name(), detail, rows, cost, List.of(), "buffered");
    }
    private final SqlValue[] args;
    private final QueryEnvironment environment;
    private final List<ScalarPlan.Binding> bindings = new ArrayList<>();
    private final ScalarPlan.Frame frame;
    private final ScalarPlan.Resolver resolver;
    private ScalarPlan.Expression where;
    private boolean[] touched;
    private TableScan scan;
    private final PlanMetrics metrics = new PlanMetrics();
    public void requireAnalyze() {}
    public PlanNode actualPlan(boolean verbose) { return metrics.render(explain(verbose)); }
    TableRelation(Engine.Table table, SqlParser.Table syntax, SqlParser.Expr predicate, SqlValue[] args, QueryEnvironment environment) {
        this.frame = new ScalarPlan.Frame();
        this.table = table; this.syntax = syntax; hostAttachment = DomainRouting.hostAttachment(syntax.name()); this.predicate = predicate; this.args = args; this.environment = environment;
        if (syntax.columns().size() > table.columns().size()) throw new SqlException("42P10", "too many table column aliases");
        String alias = syntax.alias() == null ? table.name() : syntax.alias();
        for (int c = 0; c < table.columns().size(); c++) bindings.add(new ScalarPlan.Binding(List.of(alias), c < syntax.columns().size() ? syntax.columns().get(c) : table.columns().get(c).name(), table.columns().get(c).type(), false, false, CollationDerivation.collatable(table.columns().get(c).type()) ? CollationDerivation.implicit(table.columns().get(c).collation()) : CollationDerivation.NONE).catalog(table.columns().get(c).name(), table.columns().get(c).notNull()));
        resolver = environment.resolver(bindings, frame);
        where = predicate == null ? null : predicate instanceof SqlParser.Binary b && b.op().equals("and") ? resolver.binary(b, true) : resolver.resolve(predicate, SqlType.BOOLEAN);
        if (where != null && where.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("relation filter requires boolean");
        touched = new boolean[bindings.size()]; Arrays.fill(touched, true);
    }
    boolean reusableProbe() { return probe != null; }
    /** Only sequential probes may share the resolver's frame; scans and metrics remain fresh. */
    TableRelation probeExecution() {
        if (probe == null) throw new IllegalStateException("relation is not a probe template");
        return new TableRelation(this);
    }
    private TableRelation(TableRelation template) {
        table = template.table; hostAttachment = template.hostAttachment; syntax = template.syntax;
        predicate = template.predicate; accessPredicate = template.accessPredicate; accessWhere = template.accessWhere;
        probe = template.probe; selectedAccess = template.selectedAccess; args = template.args; environment = template.environment;
        bindings.addAll(template.bindings); frame = template.frame; resolver = template.resolver; where = template.where;
        touched = template.touched.clone();
    }
    StatisticsEstimate statistics() { return new StatisticsEstimate(table, names(), syntax.alias() == null ? table.name() : syntax.alias(), resolver, environment.meter()); }
    public boolean baseIsland() { return true; }
    public List<ScalarPlan.Binding> bindings() { return List.copyOf(bindings); }
    public List<FunctionalKey> functionalKeys() {
        return table.primaryKey().length == 0 ? List.of() : List.of(new FunctionalKey(Arrays.stream(table.primaryKey()).boxed().toList(), java.util.stream.IntStream.range(0, bindings.size()).boxed().toList()));
    }
    public List<String> names() { return bindings.stream().map(ScalarPlan.Binding::name).toList(); }
    public List<SqlType> types() { return table.types(); }
    public void touch(boolean[] columns) {
        if (scan != null) throw new IllegalStateException("relation already opened");
        touched = columns.clone(); boolean[] predicateColumns = resolver.touched();
        for (int c = 0; c < touched.length; c++) touched[c] |= predicateColumns[c];
        environment.viewTable(table, touched);
    }
    void accessWhere(SqlParser.Expr filter) { accessOptions = null; accessWhere = FilterRules.and(accessWhere, filter); }
    private SqlParser.Expr accessFilter() { return FilterRules.and(predicate, accessWhere); }
    public boolean push(SqlParser.Expr filter) {
        if (!FilterRules.safe(filter, resolver) || !FilterRules.belongs(filter, bindings)) return false;
        if (scan != null) throw new IllegalStateException("relation already opened");
        accessOptions = null; predicate = FilterRules.and(predicate, filter);
        where = predicate instanceof SqlParser.Binary b && b.op().equals("and") ? resolver.binary(b, true) : resolver.resolve(predicate, SqlType.BOOLEAN);
        return true;
    }
    public boolean orderedBy(List<SqlParser.Order> order) { return orderedBy(order, false); }
    boolean orderedBy(List<SqlParser.Order> order, boolean full) {
        AccessCandidate access = selectedAccess;
        if (access == null && probe == null) {
            for (AccessCandidate candidate : accessOptions()) if (access == null || accessEstimate(candidate, false).cost() < accessEstimate(access, false).cost()) access = candidate;
        }
        return orderedBy(order, full, access);
    }
    boolean orderedBy(List<SqlParser.Order> order, boolean full, AccessCandidate access) {
        if (access != null && !access.storageOrder() || probe != null && probe.kind().equals("Index")) return false;
        if (environment != null && !CollationCompatibility.table(table, environment.meter())) return false;
        int[] key = table.primaryKey();
        if (order.isEmpty() || order.size() > key.length || full && order.size() != key.length) return false;
        for (int i = 0; i < order.size(); i++) {
            SqlParser.Order term = order.get(i);
            if (term.descending() || !(term.expression() instanceof SqlParser.Name name)) return false;
            ScalarPlan.Binding binding = bindings.get(key[i]);
            if (!name.parts().getLast().equals(binding.name()) || name.parts().size() > 1 && !name.parts().subList(0, name.parts().size() - 1).equals(binding.qualifier())) return false;
        }
        return true;
    }
    TableRelation probe(QueryEnvironment scope, SqlParser.Expr condition, Probe candidate) {
        TableRelation result = new TableRelation(table, syntax, predicate, args, scope); result.accessWhere = accessWhere; result.accessPredicate = candidate.kind().equals("GIN") || candidate.kind().equals("GiST") ? candidate.access() : condition; result.probe = candidate;
        result.resolver.resolve(condition, SqlType.BOOLEAN);
        result.selectedAccess = new AccessCandidate(probeKind(candidate), candidate.index(), candidate.kind().equals("PK") && !candidate.keys().isEmpty() ? FilterRules.and(accessFilter(), condition) : candidate.access(), 0, 0, 0);
        return result;
    }
    boolean constantGinBound() {
        try { return GinEstimate.select(table, names(), syntax.alias() == null ? table.name() : syntax.alias(), accessFilter(), resolver, environment.meter(), Long.MAX_VALUE) != null; }
        catch (IOException failure) { throw Engine.storageError(failure); }
    }
    Probe probeCandidate(SqlParser.Expr condition, List<ScalarPlan.Binding> outer) {
        List<Probe> candidates = probeOptions(condition, outer); return candidates.isEmpty() ? null : candidates.getFirst();
    }
    static int probeKind(Probe probe) { return switch (probe.kind()) {
        case "PK" -> AccessCandidate.PK;
        case "Index" -> IndexAccess.interval(probe.access()) ? AccessCandidate.INDEX_SET : AccessCandidate.BTREE;
        case "GiST" -> AccessCandidate.GIST;
        case "GIN" -> AccessCandidate.GIN;
        default -> AccessCandidate.PK_SET;
    }; }
    List<Probe> probeOptions(SqlParser.Expr condition, List<ScalarPlan.Binding> outer) {
        List<Probe> candidates = new ArrayList<>();
        if (hostAttachment) return List.of();
        List<String> keys = new ArrayList<>();
        for (int key : table.primaryKey()) {
            ScalarPlan.Binding binding = bindings.get(key); boolean found = false;
            for (SqlParser.Expr term : FilterRules.terms(condition)) if (term instanceof SqlParser.Binary binary && binary.op().equals("=")) {
                SqlParser.Expr other = namesBinding(binary.left(), binding) ? binary.right() : namesBinding(binary.right(), binding) ? binary.left() : null;
                if (outer.stream().anyMatch(candidate -> namesBinding(other, candidate) && (candidate.type().equals(binding.type()) || candidate.type().isInteger() && (binding.type().isInteger() || binding.type().kind() == SqlType.Kind.DECIMAL) || candidate.type().kind() == SqlType.Kind.DECIMAL && binding.type().isInteger()))) found = true;
            }
            if (!found) { keys.clear(); break; } keys.add(binding.name());
        }
        if (!keys.isEmpty()) candidates.add(new Probe("PK", null, List.copyOf(keys), condition));
        try {
            ScalarPlan.Resolver estimates = environment.resolver(outer, new ScalarPlan.Frame());
            SqlParser.Expr combined = FilterRules.and(accessFilter(), condition);
            KeySetEstimate.Selection set = KeySetEstimate.primary(table, names(), syntax.alias() == null ? table.name() : syntax.alias(), combined, estimates, environment.meter());
            if (set != null && hasOuter(combined, outer)) candidates.add(new Probe("PK interval", null, set.columns(), combined, estimates));
            TableScan.PrimarySelection primary = TableScan.primary(table, names(), syntax.alias() == null ? table.name() : syntax.alias(), combined, estimates, environment.meter());
            if (keys.isEmpty() && primary != null && hasOuter(primary.predicate(), outer)) candidates.add(new Probe("PK", null, List.of(), primary.predicate(), estimates));
            for (IndexAccess.Selection selected : IndexAccess.inventory(table, names(), syntax.alias() == null ? table.name() : syntax.alias(), combined, environment.meter(), estimates, false))
                if (hasOuter(selected.predicate(), outer) && compatibleOuterKeys(selected.predicate(), outer)) candidates.add(new Probe("Index", selected.index(), List.of(), selected.predicate(), estimates));
        } catch (IOException failure) { throw Engine.storageError(failure); }
        for (Engine.Index index : table.indexes().stream().filter(i -> i.kind() == 2 && i.keys().size() == 1 && i.predicate() == null).sorted((a,b) -> SqlText.compare(a.name(),b.name())).toList()) {
            Engine.IndexKey key = index.keys().getFirst(); ScalarPlan.Binding binding = bindings.get(key.column()); boolean range = key.type().kind() == SqlType.Kind.RANGE;
            for (SqlParser.Expr term : FilterRules.terms(condition)) if (term instanceof SqlParser.Binary binary) {
                SqlParser.Expr other = namesBinding(binary.left(), binding) && (range ? List.of("&&", "@>").contains(binary.op()) : binary.op().equals("=")) ? binary.right()
                    : namesBinding(binary.right(), binding) && (range ? List.of("&&", "<@").contains(binary.op()) : binary.op().equals("=")) ? binary.left() : null;
                if (outer.stream().anyMatch(candidate -> namesBinding(other, candidate) && candidate.type().equals(binding.type()))) candidates.add(new Probe("GiST", index, List.of(binding.name()), term));
            }
        }
        for (Engine.Index index : table.indexes()) if (index.kind() == 1) for (Engine.IndexKey key : index.keys()) {
            var binding = bindings.get(key.column());
            for (SqlParser.Expr term : FilterRules.terms(condition)) {
                SqlParser.Expr other = null;
                if (term instanceof SqlParser.Binary binary && namesBinding(binary.left(), binding) && (key.type().kind() == SqlType.Kind.JSONB ? List.of("@>", "?", "?|", "?&").contains(binary.op()) : List.of("@>", "&&", "=").contains(binary.op()))) other = binary.right();
                else if (term instanceof SqlParser.Quantified q && q.operator().equals("=") && q.quantifier().equals("any") && namesBinding(q.array(), binding)) other = q.subject();
                if (other != null && hasOuter(other, outer)) candidates.add(new Probe("GIN", index, List.of(binding.name()), term));
            }
        }
        candidates.sort(java.util.Comparator.<Probe>comparingInt(TableRelation::probeKind).thenComparing(probe -> probe.index() == null ? "" : probe.index().name(), SqlText::compare));
        List<Probe> unique = new ArrayList<>();
        for (Probe probe : candidates) if (unique.stream().noneMatch(prior -> probeKind(prior) == probeKind(probe) && java.util.Objects.equals(prior.index(), probe.index()))) unique.add(probe);
        return List.copyOf(unique);
    }
    private boolean compatibleOuterKeys(SqlParser.Expr expression, List<ScalarPlan.Binding> outer) {
        for (SqlParser.Expr term : FilterRules.terms(expression)) if (term instanceof SqlParser.Binary binary) {
            for (ScalarPlan.Binding local : bindings) {
                SqlParser.Expr other = namesBinding(binary.left(), local) ? binary.right() : namesBinding(binary.right(), local) ? binary.left() : null;
                for (ScalarPlan.Binding earlier : outer) if (namesBinding(other, earlier) && local.type().isFloat() && earlier.type().isFloat() && local.type() != earlier.type()) return false;
            }
        }
        return true;
    }
    private static boolean hasOuter(SqlParser.Expr expression, List<ScalarPlan.Binding> outer) {
        if (outer.stream().anyMatch(binding -> namesBinding(expression, binding))) return true;
        for (SqlParser.Expr child : DefaultExpression.children(expression)) if (child != null && hasOuter(child, outer)) return true;
        return false;
    }
    private static boolean namesBinding(SqlParser.Expr expression, ScalarPlan.Binding binding) {
        return expression instanceof SqlParser.Name name && name.parts().getLast().equals(binding.name()) && (name.parts().size() == 1 || name.parts().subList(0, name.parts().size() - 1).equals(binding.qualifier()));
    }
    PlanNode probeEstimate(Probe candidate) {
        try {
            var facts = table.rows().structuralFacts(); long rows = Math.min(table.rows().size(), 1);
            long cost = PlanEstimate.mul(rows, PlanEstimate.add(facts.height(), 1));
            int count = 0; for (boolean value : touched) if (value) count++;
            String detail = "Index-nested-loop " + candidate.kind() + " bound: ";
            if (candidate.kind().equals("PK interval")) {
                KeySetEstimate.Selection selected = KeySetEstimate.primary(table, names(), syntax.alias() == null ? table.name() : syntax.alias(), candidate.access(), candidate.estimates(), environment.meter());
                rows = selected.rows(); cost = PlanEstimate.add(selected.pages(), rows);
                detail = "Index-nested-loop PK interval set: " + String.join(", ", selected.columns()) + "; intervals=" + selected.intervals() + (selected.arrays() == 0 ? "" : "; arrays=" + selected.arrays());
            } else if (candidate.index() == null && candidate.keys().isEmpty()) {
                TableScan.PrimarySelection selected = TableScan.primary(table, names(), syntax.alias() == null ? table.name() : syntax.alias(), candidate.access(), candidate.estimates(), environment.meter());
                rows = selected.rows(); cost = PlanEstimate.add(selected.pages(), rows);
                List<String> parts = new ArrayList<>();
                ExplainExpression printer = new ExplainExpression(bindings, resolver, environment.meter());
                for (SqlParser.Expr term : FilterRules.terms(selected.predicate())) {
                    SqlParser.Binary bound = (SqlParser.Binary)term;
                    boolean sibling = bound.right() instanceof SqlParser.Name name && !FilterRules.belongs(name, bindings);
                    parts.add(((SqlParser.Name)bound.left()).parts().getLast() + " " + bound.op() + " " + (sibling ? "join" : printer.print(bound.right())));
                }
                detail += String.join(" and ", parts);
            } else if (candidate.index() == null) detail += candidate.keys().stream().map(key -> key + " = join").collect(java.util.stream.Collectors.joining(" and "));
            else {
                detail += "using " + candidate.index().name();
                if (candidate.kind().equals("Index")) {
                    IndexAccess.Selection selected = IndexAccess.inventory(table, names(), syntax.alias() == null ? table.name() : syntax.alias(), FilterRules.and(accessFilter(), candidate.access()), environment.meter(), candidate.estimates(), false).stream()
                        .filter(value -> value.index().equals(candidate.index()) && IndexAccess.interval(value.predicate()) == IndexAccess.interval(candidate.access())).findFirst().orElse(null);
                    if (selected != null) { rows = selected.rows(); cost = PlanEstimate.add(selected.pages(), rows); detail = "Index-nested-loop " + TableExplanation.indexDetail(selected, names()); }
                } else if (candidate.kind().equals("GIN")) {
                    var tree = candidate.index().entries().structuralFacts();
                    rows = PlanEstimate.scale(table.rows().size(), EstimatorFacts.MATCHING);
                    long pages = rows == 0 ? 0 : Math.min(tree.nodeCount(), Math.max(tree.height(), PlanEstimate.scale(tree.nodeCount(), EstimatorFacts.MATCHING)));
                    cost = PlanEstimate.add(pages, PlanEstimate.mul(rows, 2 + facts.height()));
                } else {
                    var tree = table.rows().structuralFacts();
                    EstimatorFacts.Fraction fraction = candidate.index().keys().getFirst().type().kind() == SqlType.Kind.RANGE ? EstimatorFacts.MATCHING : EstimatorFacts.EQUALITY;
                    rows = PlanEstimate.scale(table.rows().size(), fraction);
                    long pages = rows == 0 ? 0 : Math.min(tree.nodeCount(), Math.max(tree.height(), PlanEstimate.scale(tree.nodeCount(), fraction)));
                    long descents = pages;
                    cost = PlanEstimate.add(PlanEstimate.add(pages, descents), PlanEstimate.mul(rows, 1 + facts.height()));
                }
            }
            if (count != 0) detail += "; touched=" + count;
            if (predicate != null) { detail += "; filter:conjuncts=" + FilterRules.terms(predicate).size(); cost = PlanEstimate.add(cost, PlanEstimate.mul(rows, PlanEstimate.operations(predicate))); rows = PlanEstimate.rows(predicate, rows, List.of()); }
            return new PlanNode("Scan " + table.name(), detail, rows, cost, List.of(), "buffered");
        } catch (IOException failure) { throw Engine.storageError(failure); }
    }
    public PlanNode explain(boolean verbose) { return explain(verbose, List.of(), null, null); }
    PlanNode explainPrefix(boolean verbose, List<SqlParser.Order> order, SqlParser.Expr limit, SqlParser.Expr offset) {
        PlanNode result = explain(verbose, order, limit, offset);
        return result.label().equals("Limit") ? result.children().getFirst() : result;
    }
    private PlanNode explain(boolean verbose, List<SqlParser.Order> order, SqlParser.Expr limit, SqlParser.Expr offset) {
        if (probe != null) return probeEstimate(probe);
        if (selectedAccess != null) return accessEstimate(selectedAccess, verbose);
        ScalarPlan.Resolver explainResolver = environment.resolver(bindings, frame);
        if (predicate != null) explainResolver.resolve(predicate, SqlType.BOOLEAN);
        for (int i = 0; i < touched.length; i++) if (touched[i]) explainResolver.resolve(new SqlParser.ColumnSlot(i), null);
        SqlParser.Select query = new SqlParser.Select(List.of(), accessFilter(), order, limit, offset, false);
        PlanNode node = TableExplanation.describe(query, table, names(), syntax.alias() == null ? table.name() : syntax.alias(), List.of(), bindings, explainResolver, environment.meter(), false, false, false, verbose, false, false, null, null, hostAttachment);
        if (accessWhere != null && node.label().equals("Filter")) {
            node = node.children().getFirst();
            if (predicate != null) node = node.wrap("Filter", "", PlanEstimate.rows(predicate, node.rows(), List.of()), PlanEstimate.mul(node.rows(), PlanEstimate.operations(predicate)));
        }
        if (predicate != null && node.label().equals("Filter")) {
            PlanNode scan = node.children().getFirst(); String filter = verbose ? "filter=" + new ExplainExpression(bindings, explainResolver, environment.meter()).print(TableExplanation.orderedPredicate(predicate, environment)) : "filter:conjuncts=" + FilterRules.terms(predicate).size();
            return new PlanNode(scan.label(), TableExplanation.append(scan.detail(), filter), node.rows(), node.cost(), scan.children(), "buffered");
        }
        return node;
    }
    public Row next(CostMeter meter) {
        long before = meter.cost();
        try { return nextMeasured(meter); } finally { metrics.total(before, meter); }
    }
    private Row nextMeasured(CostMeter meter) {
        try {
            if (scan == null) scan = new TableScan(table, syntax.alias(), names(), hostAttachment ? null : probe != null && (probe.kind().equals("GIN") || probe.kind().equals("GiST")) ? accessPredicate : FilterRules.and(accessFilter(), accessPredicate), args, meter, touched, environment, false, null, false, false, selectedAccess);
            PersistentTree.Entry entry;
            while ((entry = scan.next()) != null) {
                meter.charge(Costs.STORAGE_ROW_READ); meter.guard();
                frame.values = Encoding.row(entry, table.types());
                if (where != null) { SqlValue accepted = where.run(meter); if (accepted.isNull() || !accepted.asBoolean()) continue; }
                List<SqlValue> row = new ArrayList<>();
                for (int c = 0; c < bindings.size(); c++) row.add(touched[c] ? frame.values.get(c) : SqlValue.nullValue(bindings.get(c).type()));
                return new Row(names(), row);
            }
            return null;
        } catch (IOException error) { throw Engine.storageError(error); }
    }
    public void close() { if (scan != null) scan.close(); }
}
