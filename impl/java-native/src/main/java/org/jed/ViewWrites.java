package org.jed;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

/** Expand an automatically writable view into its ordinary base-table statement. */
final class ViewWrites {
    private final Engine.Snapshot snapshot;
    private final QueryEnvironment environment;
    private final List<Check> checks = new ArrayList<>();
    private boolean cascaded;
    record Check(String view, SqlParser.Expr expression) {}
    record Expanded(SqlParser.Statement statement, List<Check> checks) {
        Checks resolve(Engine.Snapshot snapshot, QueryEnvironment environment) {
            SqlParser.Name target = target(statement);
            Engine.Table table = target == null ? null : DomainRouting.readDomain(snapshot, target).tables().get(Engine.name(target));
            String alias = switch (statement) { case SqlParser.Insert x -> x.alias(); case SqlParser.Update x -> x.alias(); case SqlParser.Delete x -> x.alias(); case SqlParser.Merge x -> x.alias(); default -> null; };
            return new Checks(checks, table, alias, environment);
        }
    }
    static final class Checks {
        private final ScalarPlan.Frame frame = new ScalarPlan.Frame();
        private final List<String> names = new ArrayList<>();
        private final List<ScalarPlan.Expression> predicates = new ArrayList<>();
        private final CostMeter meter;
        Checks(List<Check> checks, Engine.Table table, String alias, QueryEnvironment environment) {
            meter = environment.meter();
            if (checks.isEmpty()) return;
            ScalarPlan.Resolver resolver = environment.resolver(table.bindings(alias), frame);
            for (Check check : checks) if (check.expression() != null) { names.add(check.view()); predicates.add(resolver.writtenPredicate(check.expression())); }
        }
        boolean present() { return !predicates.isEmpty(); }
        void accept(List<SqlValue> values) {
            frame.values = values;
            for (int i = 0; i < predicates.size(); i++) { SqlValue result = predicates.get(i).run(meter); if (result.isNull() || !result.asBoolean()) throw new SqlException("44000", "new row violates check option for view " + names.get(i)); }
        }
    }
    private ViewWrites(Engine.Snapshot snapshot, QueryEnvironment environment) { this.snapshot = snapshot; this.environment = environment; }
    static Expanded expand(SqlParser.Statement statement, Engine.Snapshot snapshot, QueryEnvironment environment) {
        ViewWrites rewrite = new ViewWrites(snapshot, environment);
        for (int depth = 0; depth <= 256; depth++) {
            SqlParser.Name target = target(statement); if (target == null) return new Expanded(statement, List.copyOf(rewrite.checks));
            Engine.View view = DomainRouting.readDomain(snapshot, target).views().get(Engine.name(target)); if (view == null) return new Expanded(statement, List.copyOf(rewrite.checks));
            statement = rewrite.level(statement, view);
        }
        throw new SqlException("54001", "view write nesting limit");
    }
    private static SqlParser.Name target(SqlParser.Statement statement) {
        return switch (statement) { case SqlParser.Insert x -> x.table(); case SqlParser.Update x -> x.table(); case SqlParser.Delete x -> x.table(); case SqlParser.Merge x -> x.table(); default -> null; };
    }
    static boolean shape(SqlParser.Statement query, boolean needsColumn) {
        if (!(query instanceof SqlParser.Select select) || select.distinct() || !select.distinctOn().isEmpty() || !select.group().isEmpty()
                || select.having() != null || select.limit() != null || select.offset() != null || select.from().size() != 1
                || !(select.from().getFirst() instanceof SqlParser.Table table) || Engine.name(table.name()).startsWith("jed_")
                || AggregatePlan.needed(select) || WindowSyntax.needed(select)) return false;
        return !needsColumn || select.items().stream().anyMatch(item -> DefaultExpression.unwrap(item.expression()) instanceof SqlParser.Name || item.expression() instanceof SqlParser.Star);
    }
    private SqlParser.Statement level(SqlParser.Statement statement, Engine.View view) {
        if (statement instanceof SqlParser.Insert insert && insert.conflict() != null && insert.conflict().predicate() != null) throw new SqlException("0A000", "partial conflict predicate on view is unsupported");
        SqlParser.Statement query = view.query();
        if (!shape(query, !(statement instanceof SqlParser.Delete))) throw new SqlException("55000", "view is not automatically updatable: " + view.name());
        try (QueryPlan checked = environment.viewScope(view.name()).plan(query)) { checked.types(); }
        SqlParser.Select select = (SqlParser.Select) query; SqlParser.Table from = (SqlParser.Table) select.from().getFirst();
        String base = Engine.name(from.name()), sourceLabel = from.alias() == null ? base : from.alias();
        String alias = switch (statement) { case SqlParser.Insert x -> x.alias(); case SqlParser.Update x -> x.alias(); case SqlParser.Delete x -> x.alias(); case SqlParser.Merge x -> x.alias(); default -> null; };
        String label = alias == null ? view.name() : alias;
        List<String> baseColumns = columns(from.name());
        Map<String, SqlParser.Expr> source = new LinkedHashMap<>();
        for (int i = 0; i < baseColumns.size(); i++) source.put(i < from.columns().size() ? from.columns().get(i) : baseColumns.get(i), new SqlParser.Name(List.of(label, baseColumns.get(i))));
        Rewriter body = new Rewriter(source, Map.of(sourceLabel, label), label, false);
        List<SqlParser.Expr> items = new ArrayList<>();
        for (SqlParser.Item item : select.items()) {
            if (item.expression() instanceof SqlParser.Star) items.addAll(source.values());
            else items.add(body.expression(item.expression()));
        }
        Map<String, SqlParser.Expr> mapping = new LinkedHashMap<>();
        for (int i = 0; i < view.columns().size(); i++) mapping.put(view.columns().get(i).name(), items.get(i));
        String oldAlias = switch (statement) { case SqlParser.Insert x -> x.oldAlias(); case SqlParser.Update x -> x.oldAlias(); case SqlParser.Delete x -> x.oldAlias(); case SqlParser.Merge x -> x.oldAlias(); default -> "old"; };
        String newAlias = switch (statement) { case SqlParser.Insert x -> x.newAlias(); case SqlParser.Update x -> x.newAlias(); case SqlParser.Delete x -> x.newAlias(); case SqlParser.Merge x -> x.newAlias(); default -> "new"; };
        Map<String, String> labels = new LinkedHashMap<>(); labels.put(label, label);
        Rewriter row = new Rewriter(mapping, labels, label, true);
        for (int i = 0; i < checks.size(); i++) { Check check = checks.get(i); checks.set(i, new Check(check.view(), row.expression(check.expression()))); }
        if (!(statement instanceof SqlParser.Delete) && (view.checkOption() != 0 || cascaded)) checks.addFirst(new Check(view.name(), body.expression(select.where())));
        cascaded |= view.checkOption() == 2;
        Map<String, String> returningLabels = new LinkedHashMap<>(labels); returningLabels.putIfAbsent(oldAlias, oldAlias); returningLabels.putIfAbsent(newAlias, newAlias);
        Rewriter returning = new Rewriter(mapping, returningLabels, label, true);
        if (statement instanceof SqlParser.Delete x) return new SqlParser.Delete(from.name(), label, x.using(), and(body.expression(select.where()), row.expression(x.where())), returning.items(x.returning()), oldAlias, newAlias);
        if (statement instanceof SqlParser.Update x) {
            validateTargets(x.assignments().stream().map(SqlParser.Assignment::column).toList(), mapping);
            List<SqlParser.Assignment> assignments = assignments(x.assignments(), view, mapping, row);
            return new SqlParser.Update(from.name(), label, assignments, x.from(), and(body.expression(select.where()), row.expression(x.where())), returning.items(x.returning()), oldAlias, newAlias);
        }
        if (statement instanceof SqlParser.Merge merge) {
            List<SqlParser.MergeWhen> clauses = new ArrayList<>();
            for (SqlParser.MergeWhen clause : merge.clauses()) {
                List<SqlParser.Assignment> assigned = clause.assignments(); List<String> targets = clause.columns(); List<SqlParser.Expr> values = clause.values();
                if (clause.action().equals("update")) {
                    validateTargets(assigned.stream().map(SqlParser.Assignment::column).toList(), mapping);
                    assigned = assignments(assigned, view, mapping, row);
                }
                if (clause.action().equals("insert")) {
                    targets = new ArrayList<>(clause.defaultValues() ? List.of() : targets.isEmpty() ? mapping.keySet() : targets);
                    validateTargets(targets, mapping);
                    if (values.size() > targets.size()) throw new SqlException("42601", "too many MERGE INSERT values");
                    List<SqlParser.Expr> next = new ArrayList<>();
                    int supplied = values.size();
                    for (int i = 0; i < targets.size(); i++) {
                        SqlParser.Expr value = i < supplied ? values.get(i) : new SqlParser.Default();
                        if (value instanceof SqlParser.Default) { SqlParser.Expr own = defaultOf(view, targets.get(i)); if (own != null) value = own; }
                        next.add(value);
                    }
                    for (Engine.ViewColumn column : view.columns()) if (column.defaultText() != null && !targets.contains(column.name())) { targets.add(column.name()); next.add(defaultOf(view, column.name())); }
                    targets = targets.stream().map(column -> baseColumn(mapping, column)).toList(); duplicate(targets); values = next;
                }
                clauses.add(new SqlParser.MergeWhen(clause.kind(), clause.kind().equals("target") ? clause.condition() : row.expression(clause.condition()), clause.action(), assigned, targets, values, clause.defaultValues() && targets.isEmpty(), clause.overriding()));
            }
            SqlParser.Expr filter = and(body.expression(select.where()), row.expression(merge.targetWhere()));
            return new SqlParser.Merge(from.name(), label, merge.source(), and(body.expression(select.where()), row.expression(merge.on())), clauses, returning.items(merge.returning()), oldAlias, newAlias, filter);
        }
        SqlParser.Insert x = (SqlParser.Insert) statement;
        List<String> targets = new ArrayList<>(x.defaultValues() ? List.of() : x.columns().isEmpty() ? mapping.keySet() : x.columns());
        validateNames(targets, mapping);
        if (x.conflict() != null) validateNames(x.conflict().assignments().stream().map(SqlParser.Assignment::column).toList(), mapping);
        List<String> allTargets = new ArrayList<>(targets);
        if (x.conflict() != null) allTargets.addAll(x.conflict().assignments().stream().map(SqlParser.Assignment::column).toList());
        for (Engine.ViewColumn column : view.columns()) if (column.defaultText() != null && !targets.contains(column.name())) { targets.add(column.name()); allTargets.add(column.name()); }
        for (String column : mapping.keySet()) if (allTargets.contains(column)) baseColumn(mapping, column);
        List<String> mapped = targets.stream().map(column -> baseColumn(mapping, column)).toList(); duplicate(mapped);
        List<List<SqlParser.Expr>> values = new ArrayList<>();
        List<List<SqlParser.Expr>> original = x.defaultValues() ? List.of(List.of()) : x.values();
        int supplied = x.defaultValues() ? 0 : x.columns().isEmpty() ? mapping.size() : x.columns().size();
        for (List<SqlParser.Expr> input : original) {
            if (input.size() != supplied) throw new SqlException("42601", "INSERT column count mismatch");
            List<SqlParser.Expr> next = new ArrayList<>();
            for (int i = 0; i < targets.size(); i++) {
                SqlParser.Expr value = i < input.size() ? input.get(i) : new SqlParser.Default();
                if (value instanceof SqlParser.Default) { SqlParser.Expr own = defaultOf(view, targets.get(i)); if (own != null) value = own; }
                next.add(value);
            }
            values.add(next);
        }
        SqlParser.Statement sourceQuery = x.query();
        if (sourceQuery != null && targets.size() > supplied) {
            List<String> names = new ArrayList<>(); List<SqlParser.Item> projection = new ArrayList<>();
            for (int i = 0; i < supplied; i++) { names.add("c" + i); projection.add(new SqlParser.Item(new SqlParser.Name(List.of("$view_input", "c" + i)), null)); }
            for (int i = supplied; i < targets.size(); i++) projection.add(new SqlParser.Item(defaultOf(view, targets.get(i)), null));
            sourceQuery = new SqlParser.Select(projection, null, List.of(), null, null, false, List.of(new SqlParser.Derived(sourceQuery, "$view_input", names)), List.of(), null, List.of());
        }
        SqlParser.Conflict conflict = x.conflict();
        if (conflict != null) {
            if (conflict.constraint() != null) throw new SqlException("42704", "view has no named constraints");
            for (SqlParser.ConflictElement column : conflict.columns()) if (!mapping.containsKey(column.column())) throw new SqlException("42703", "unknown view column: " + column.column());
            List<SqlParser.ConflictElement> keys = new ArrayList<>();
            for (SqlParser.ConflictElement column : conflict.columns()) {
                if (column.ordered()) throw new SqlException("42P10", "ordering is not allowed in conflict target");
                if (column.collation() != null) environment.meter().environment().collation(column.collation());
                if (column.opclass() != null) Upsert.requireOpclass(column.opclass());
                SqlParser.Expr item = mapping.get(column.column());
                if (!(item instanceof SqlParser.Name name)) throw new SqlException("42P10", "read-only view column has no conflict arbiter");
                keys.add(new SqlParser.ConflictElement(name.parts().getLast(), column.collation(), column.opclass(), column.ordered()));
            }
            Map<String, String> conflictLabels = new LinkedHashMap<>(labels); conflictLabels.put("excluded", "excluded");
            Rewriter existing = new Rewriter(mapping, conflictLabels, label, true);
            conflict = new SqlParser.Conflict(keys, null, row.expression(conflict.predicate()), conflict.nothing(), assignments(conflict.assignments(), view, mapping, existing), existing.expression(conflict.where()));
        }
        return new SqlParser.Insert(from.name(), mapped, values, sourceQuery, x.defaultValues() && targets.isEmpty(), returning.items(x.returning()), oldAlias, newAlias, x.overriding(), label, conflict);
    }
    private List<String> columns(String name) { return columns(new SqlParser.Name(List.of(name))); }
    private List<String> columns(SqlParser.Name syntax) {
        String name = Engine.name(syntax); Engine.Snapshot local = DomainRouting.readDomain(snapshot, syntax);
        Engine.Table table = local.tables().get(name); if (table != null) return table.columns().stream().map(Engine.Column::name).toList();
        Engine.View view = local.views().get(name); return view == null ? List.of() : view.columns().stream().map(Engine.ViewColumn::name).toList();
    }
    private static SqlParser.Expr defaultOf(Engine.View view, String name) {
        for (Engine.ViewColumn column : view.columns()) if (column.name().equals(name) && column.defaultText() != null) return new SqlParser("SELECT " + column.defaultText()).parse().items().getFirst().expression();
        return null;
    }
    private static List<SqlParser.Assignment> assignments(List<SqlParser.Assignment> assignments, Engine.View view, Map<String, SqlParser.Expr> mapping, Rewriter row) {
        validateTargets(assignments.stream().map(SqlParser.Assignment::column).toList(), mapping);
        List<SqlParser.Assignment> result = new ArrayList<>();
        for (SqlParser.Assignment assignment : assignments) {
            SqlParser.Expr value = assignment.expression();
            if (value instanceof SqlParser.Default) { value = defaultOf(view, assignment.column()); if (value == null) value = new SqlParser.Literal(Lexer.Kind.WORD, "null"); }
            else value = row.expression(value);
            result.add(new SqlParser.Assignment(baseColumn(mapping, assignment.column()), value));
        }
        duplicate(result.stream().map(SqlParser.Assignment::column).toList()); return result;
    }
    private static void duplicate(List<String> targets) { if (new HashSet<>(targets).size() != targets.size()) throw new SqlException("42701", "view column assigned more than once"); }
    private static void validateNames(List<String> targets, Map<String, SqlParser.Expr> mapping) {
        for (String target : targets) if (!mapping.containsKey(target)) throw new SqlException("42703", "unknown view column: " + target);
        duplicate(targets);
    }
    private static void validateTargets(List<String> targets, Map<String, SqlParser.Expr> mapping) {
        validateNames(targets, mapping); for (String column : mapping.keySet()) if (targets.contains(column)) baseColumn(mapping, column);
    }
    private static String baseColumn(Map<String, SqlParser.Expr> mapping, String column) {
        if (!(mapping.get(column) instanceof SqlParser.Name name)) throw new SqlException("0A000", "cannot assign read-only view column: " + column);
        return name.parts().getLast();
    }
    private static SqlParser.Expr and(SqlParser.Expr a, SqlParser.Expr b) { return a == null ? b : b == null ? a : new SqlParser.Binary("and", a, b); }

    private final class Rewriter {
        private final Map<String, SqlParser.Expr> mapping;
        private final Map<String, String> labels;
        private final String baseLabel;
        private final boolean strict;
        Rewriter(Map<String, SqlParser.Expr> mapping, Map<String, String> labels, String baseLabel, boolean strict) { this.mapping = mapping; this.labels = labels; this.baseLabel = baseLabel; this.strict = strict; }
        SqlParser.Expr expression(SqlParser.Expr expression) { return expression(expression, Set.of(), Set.of(), false); }
        private SqlParser.Expr expression(SqlParser.Expr expression, Set<String> locals, Set<String> localLabels, boolean nested) {
            return WindowSyntax.map(DefaultExpression.unwrap(expression), node -> {
                if (node instanceof SqlParser.Name name) {
                    String column = name.parts().getLast(), qualifier = name.parts().size() > 1 ? String.join(".", name.parts().subList(0, name.parts().size() - 1)) : null;
                    if (qualifier == null && locals.contains(column) || qualifier != null && localLabels.contains(qualifier)) return node;
                    if (qualifier == null || labels.containsKey(qualifier)) {
                        SqlParser.Expr replacement = mapping.get(column);
                        if (replacement == null) { if (strict && (!nested || qualifier != null)) throw new SqlException("42703", "unknown view column: " + column); return node; }
                        String use = qualifier == null ? baseLabel : labels.get(qualifier);
                        return qualify(replacement, baseLabel, use);
                    }
                    return node;
                }
                if (node instanceof SqlParser.Subquery x) return new SqlParser.Subquery(query(x.query(), locals, localLabels));
                if (node instanceof SqlParser.Exists x) return new SqlParser.Exists(query(x.query(), locals, localLabels));
                if (node instanceof SqlParser.In x && x.query() != null) return new SqlParser.In(expression(x.subject(), locals, localLabels, nested), x.values(), query(x.query(), locals, localLabels), x.negated());
                if (node instanceof SqlParser.ArrayConstructor x && x.query() != null) return new SqlParser.ArrayConstructor(x.values(), query(x.query(), locals, localLabels));
                if (node instanceof SqlParser.Quantified x && x.query() != null) return new SqlParser.Quantified(x.operator(), expression(x.subject(), locals, localLabels, nested), x.quantifier(), null, query(x.query(), locals, localLabels));
                return node;
            });
        }
        private SqlParser.Expr qualify(SqlParser.Expr expression, String old, String next) {
            if (old.equals(next)) return expression;
            return WindowSyntax.map(expression, node -> node instanceof SqlParser.Name name && name.parts().size() == 2 && name.parts().getFirst().equals(old) ? new SqlParser.Name(List.of(next, name.parts().getLast())) : node);
        }
        List<SqlParser.Item> items(List<SqlParser.Item> items) {
            List<SqlParser.Item> result = new ArrayList<>();
            for (SqlParser.Item item : items) {
                if (item.expression() instanceof SqlParser.Star star && (star.qualifier().isEmpty() || labels.containsKey(String.join(".", star.qualifier())))) {
                    String label = star.qualifier().isEmpty() ? baseLabel : labels.get(String.join(".", star.qualifier()));
                    for (var entry : mapping.entrySet()) result.add(new SqlParser.Item(qualify(entry.getValue(), baseLabel, label), entry.getKey()));
                } else result.add(new SqlParser.Item(expression(item.expression()), item.alias() == null ? ScalarPlan.outputName(item.expression()) : item.alias()));
            }
            return result;
        }
        private SqlParser.Statement query(SqlParser.Statement query, Set<String> enclosing, Set<String> enclosingLabels) {
            if (query instanceof SqlParser.Select select) {
                Set<String> locals = new HashSet<>(enclosing), localLabels = new HashSet<>(enclosingLabels);
                for (SqlParser.From from : select.from()) local(from, locals, localLabels);
                java.util.function.Function<SqlParser.Expr, SqlParser.Expr> map = x -> expression(x, locals, localLabels, true);
                return new SqlParser.Select(select.items().stream().map(x -> new SqlParser.Item(map.apply(x.expression()), x.alias())).toList(), map.apply(select.where()), select.order().stream().map(x -> new SqlParser.Order(map.apply(x.expression()), x.descending(), x.nullsFirst())).toList(), map.apply(select.limit()), map.apply(select.offset()), select.distinct(), select.from(), select.group().stream().map(map).toList(), map.apply(select.having()), select.distinctOn().stream().map(map).toList(), select.windows());
            }
            if (query instanceof SqlParser.SetOperation x) return new SqlParser.SetOperation(query(x.left(), enclosing, enclosingLabels), query(x.right(), enclosing, enclosingLabels), x.kind(), x.all());
            if (query instanceof SqlParser.Values x) return new SqlParser.Values(x.rows().stream().map(row -> row.stream().map(e -> expression(e, enclosing, enclosingLabels, true)).toList()).toList(), x.order(), x.limit(), x.offset());
            if (query instanceof SqlParser.With x) return new SqlParser.With(x.recursive(), x.ctes().stream().map(c -> new SqlParser.Cte(c.name(), c.columns(), c.materialization(), query(c.body(), enclosing, enclosingLabels), c.search(), c.cycle())).toList(), query(x.body(), enclosing, enclosingLabels));
            if (query instanceof SqlParser.QueryTail x) return new SqlParser.QueryTail(query(x.query(), enclosing, enclosingLabels), x.order(), x.limit(), x.offset());
            return query;
        }
        private void local(SqlParser.From from, Set<String> names, Set<String> labels) {
            if (from instanceof SqlParser.Table table) { labels.add(table.alias() == null ? Engine.name(table.name()) : table.alias()); List<String> original = columns(Engine.name(table.name())); for (int i = 0; i < original.size(); i++) names.add(i < table.columns().size() ? table.columns().get(i) : original.get(i)); }
            else if (from instanceof SqlParser.Join join) { local(join.left(), names, labels); local(join.right(), names, labels); }
            else if (from instanceof SqlParser.Derived derived) { if (derived.alias() != null) labels.add(derived.alias()); names.addAll(derived.columns()); if (derived.query() instanceof SqlParser.Select select) for (SqlParser.Item item : select.items()) names.add(item.alias() == null ? ScalarPlan.outputName(item.expression()) : item.alias()); }
            else if (from instanceof SqlParser.TableFunction function) { if (function.alias() != null) labels.add(function.alias()); names.addAll(function.columns()); }
            else if (from instanceof SqlParser.Grouped grouped) { local(grouped.source(), names, labels); if (grouped.alias() != null) labels.add(grouped.alias()); names.addAll(grouped.columns()); }
        }
    }
}
