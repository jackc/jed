package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import org.jed.storage.PersistentTree;

/** Snapshot matching followed by end-state validation and ordered mutation. */
final class MergePlan {
    record Result(Engine.Snapshot snapshot, QueryPlan rows) {}
    private record Clause(SqlParser.MergeWhen syntax, ScalarPlan.Frame frame, ScalarPlan.Resolver resolver,
                          ScalarPlan.Expression condition, List<Integer> columns, List<ScalarPlan.Expression> values, boolean[] assigned) {}
    private final SqlParser.Merge command;
    private final Engine.Table table, writable;
    private final Engine.Snapshot pending;
    private final QueryEnvironment environment;
    private final Database database;
    private final long workMem;
    private final SqlValue[] args;
    private final RelationPlan source;
    private final ScalarPlan.Frame pair = new ScalarPlan.Frame(), sourceFrame = new ScalarPlan.Frame();
    private final ScalarPlan.Resolver pairResolver, sourceResolver;
    private final ScalarPlan.Expression on, targetFilter;
    private final ScalarPlan.Frame targetFrame = new ScalarPlan.Frame();
    private final List<Clause> clauses = new ArrayList<>();
    private final List<SqlValue> nullTarget, nullSource;
    private final Engine.CheckEvaluator checks;
    private final Output output;
    private final boolean[] touched;
    private final MergeMatching matching;
    private PersistentTree affected, matched;
    private long rowId;
    private long actionCost, sourceCost, probeCost, loopCost, unmatchedCost, executionStart;
    private PlanNode sourceActual;


    MergePlan(SqlParser.Merge command, Engine.Table table, Engine.Table writable, Engine.Snapshot pending,
              RelationPlan source, QueryEnvironment environment, Database database, long workMem, SqlValue[] args) {
        this.command = command; this.table = table; this.writable = writable; this.pending = pending;
        this.source = source; this.environment = environment; this.database = database; this.workMem = workMem; this.args = args;
        rowId = writable.nextRowId();
        String target = command.alias() == null ? table.name() : command.alias();
        String sourceLabel = label(command.source());
        if (target.equals(sourceLabel)) throw new SqlException("42712", "duplicate MERGE relation label");
        if (source.bindings().stream().anyMatch(binding -> binding.qualifier().equals(List.of(target)))) {
            ambiguous(command.on(), target); ambiguous(command.returning(), target);
            for (SqlParser.MergeWhen clause : command.clauses()) if (clause.kind().equals("matched")) ambiguous(clause, target);
        }
        List<ScalarPlan.Binding> bindings = new ArrayList<>(table.bindings(command.alias())); bindings.addAll(source.bindings());
        pairResolver = environment.resolver(bindings, pair); sourceResolver = environment.resolver(source.bindings(), sourceFrame);
        on = predicate(pairResolver, command.on());
        targetFilter = predicate(environment.resolver(table.bindings(command.alias()), targetFrame), command.targetWhere());
        nullTarget = table.types().stream().map(SqlValue::nullValue).toList();
        nullSource = source.types().stream().map(SqlValue::nullValue).toList();
        checks = new Engine.CheckEvaluator(table, environment);
        for (SqlParser.MergeWhen clause : command.clauses()) {
            ScalarPlan.Frame frame = clause.kind().equals("matched") ? pair : clause.kind().equals("target") ? sourceFrame : new ScalarPlan.Frame();
            ScalarPlan.Resolver resolver = clause.kind().equals("matched") ? pairResolver : clause.kind().equals("target") ? sourceResolver : environment.resolver(table.bindings(command.alias()), frame);
            ScalarPlan.Expression condition = clause.condition() == null ? null : resolver.writtenPredicate(clause.condition());
            if (condition != null && condition.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("MERGE WHEN condition must be boolean");
            List<Integer> columns = new ArrayList<>(); List<ScalarPlan.Expression> values = new ArrayList<>(); boolean[] assigned = new boolean[table.columns().size()];
            if (clause.action().equals("update")) for (SqlParser.Assignment assignment : clause.assignments()) {
                int column = table.column(assignment.column()); if (assigned[column]) throw new SqlException("42701", "duplicate MERGE assignment");
                assigned[column] = true; columns.add(column); Engine.Column definition = table.columns().get(column);
                boolean generated = assignment.expression() instanceof SqlParser.Default;
                if (definition.identity() == 2 && !generated) throw new SqlException("428C9", "GENERATED ALWAYS requires DEFAULT");
                values.add(Engine.assignment(resolver, generated ? Engine.defaultOf(definition) : assignment.expression(), definition.type()));
            }
            if (clause.action().equals("insert")) {
                List<Integer> targets = new ArrayList<>();
                if (clause.columns().isEmpty()) for (int i = 0; i < table.columns().size(); i++) targets.add(i);
                else for (String name : clause.columns()) { int column = table.column(name); if (targets.contains(column)) throw new SqlException("42701", "duplicate INSERT column"); targets.add(column); }
                if (clause.values().size() > targets.size()) throw new SqlException("42601", "too many MERGE INSERT values");
                List<SqlParser.Expr> expressions = new ArrayList<>(java.util.Collections.nCopies(table.columns().size(), null));
                for (int i = 0; i < clause.values().size(); i++) expressions.set(targets.get(i), clause.values().get(i));
                for (int i = 0; i < table.columns().size(); i++) {
                    Engine.Column definition = table.columns().get(i); SqlParser.Expr expression = expressions.get(i);
                    boolean generated = expression == null || expression instanceof SqlParser.Default || definition.identity() != 0 && "user".equals(clause.overriding());
                    if (definition.identity() == 2 && !generated && !"system".equals(clause.overriding())) throw new SqlException("428C9", "GENERATED ALWAYS requires DEFAULT");
                    columns.add(i); assigned[i] = true; values.add(Engine.assignment(resolver, generated ? Engine.defaultOf(definition) : expression, definition.type()));
                }
            }
            clauses.add(new Clause(clause, frame, resolver, condition, columns, values, assigned));
        }
        output = new Output(table, command, source.bindings(), environment);
        touched = Arrays.copyOf(pairResolver.touched(), table.columns().size());
        for (Clause clause : clauses) if (!clause.syntax.kind().equals("target")) {
            boolean[] read = clause.resolver.touched(); for (int i = 0; i < touched.length; i++) touched[i] |= read[i];
        }
        boolean[] returned = output.touched(); for (int i = 0; i < touched.length; i++) touched[i] |= returned[i];
        boolean[] sourceTouched = new boolean[source.bindings().size()]; Arrays.fill(sourceTouched, true); source.touch(sourceTouched);
        for (int i = 0; i < touched.length; i++) if (touched[i]) pairResolver.resolve(new SqlParser.ColumnSlot(i), null);
        matching = new MergeMatching(command, table, source, pairResolver, environment.meter());
        affected = PersistentTree.empty(database.pageSize(), TableLeafCodec.index(), database.pageCodec()); matched = PersistentTree.empty(database.pageSize(), TableLeafCodec.index(), database.pageCodec());
    }
    Result execute(ForeignKeys.Deferred deferred, ViewWrites.Checks viewChecks) throws IOException {
        CostMeter meter = environment.meter(); executionStart = meter.cost();
        if (meter.writeMetrics != null) source.requireAnalyze();
        List<SqlType> actionTypes = new ArrayList<>(List.of(SqlType.I32, SqlType.BYTEA, SqlType.BYTEA)); actionTypes.addAll(source.types()); actionTypes.addAll(table.types());
        try (source; RowSpool sources = new RowSpool(source.types(), 0, null, meter.memory());
             RowSpool actions = new RowSpool(actionTypes, workMem, database.scratchDirectory(), meter.memory());
             HashRows hashRows = matching.hash() ? new HashRows(database.scratchDirectory(), workMem, meter.memory(), table.types().size() + 1) : null;
             IndexMutation indexes = new IndexMutation(table, environment, workMem, database.scratchDirectory());
             ForeignKeys.Batch references = new ForeignKeys.Batch(table, database, environment, workMem, pending)) {
            environment.initialize(meter);
            Row row; while ((row = source.next(meter)) != null) { meter.charge(Costs.ROW_PRODUCED); meter.guard(); sources.append(row.values()); }
            sourceCost = meter.cost() - executionStart;
            if (meter.writeMetrics != null) { sourceActual = source.actualPlan(false); sourceActual = sourceActual.finish(sourceActual.detail(), sources.size() * Costs.ROW_PRODUCED, sourceActual.lane()); }
            if (hashRows != null) {
                long beforeScan = meter.cost();
                try (var scan = new TableScan(table, command.alias(), table.columns().stream().map(Engine.Column::name).toList(), null, args, meter, touched, environment)) {
                    probeCost += meter.cost() - beforeScan;
                    PersistentTree.Entry entry; while ((entry = readTarget(scan, meter)) != null) {
                        List<SqlValue> old = Encoding.row(entry, table.types());
                        byte[] key = matching.key(old, false, meter); if (key == null) continue;
                        List<SqlValue> retained = new ArrayList<>(); for (int c = 0; c < touched.length; c++) retained.add(touched[c] ? old.get(c) : nullTarget.get(c));
                        retained.add(SqlValue.ofBytes(entry.key())); hashRows.append(key, retained);
                    }
                }
                hashRows.freeze();
            }
            QueryEnvironment probeEnvironment = environment.child(sourceResolver);
            try (var rows = sources.cursor()) { List<SqlValue> values; while ((values = rows.next()) != null) {
                meter.guard(); sourceFrame.values = values; boolean found = false;
                if (hashRows != null) {
                    byte[] key = matching.key(values, true, meter);
                    if (key != null) try (var candidates = hashRows.probe(key)) {
                        HashRows.Entry candidate; while ((candidate = candidates.next()) != null) {
                            meter.charge(Math.max(1, Math.min(key.length, candidate.key().length)) * Costs.HASH_PROBE); meter.guard();
                            if (!Arrays.equals(key, candidate.key())) continue;
                            byte[] storageKey = candidate.row().getLast().asBytes(); PersistentTree.Entry entry = table.rows().getEntry(storageKey);
                            List<SqlValue> old = Encoding.row(entry, table.types()); pair.values = concat(old, values);
                            if (!matches(on, meter)) continue;
                            found = true; matched = matched.put(entry.key(), new byte[0]); choose("matched", entry, old, values, actions);
                        }
                    }
                } else { long beforeScan = meter.cost(); try (var scan = new TableScan(table, command.alias(), table.columns().stream().map(Engine.Column::name).toList(), command.on(), args, meter, touched, probeEnvironment)) {
                    probeCost += meter.cost() - beforeScan;
                    PersistentTree.Entry entry; while ((entry = readTarget(scan, meter)) != null) {
                        List<SqlValue> old = Encoding.row(entry, table.types()); pair.values = concat(old, values);
                        if (!matches(on, meter)) continue;
                        found = true; matched = matched.put(entry.key(), new byte[0]); choose("matched", entry, old, values, actions);
                    }
                }
                }
                if (!found) choose("target", null, nullTarget, values, actions);
            } }
            loopCost = meter.cost() - executionStart - actionCost;
            long beforeUnmatched = meter.cost(), earlierActions = actionCost;
            if (command.clauses().stream().anyMatch(c -> c.kind().equals("source"))) {
                sourceFrame.values = nullSource;
                try (var scan = new TableScan(table, command.alias(), table.columns().stream().map(Engine.Column::name).toList(), null, args, meter, touched, environment)) {
                    PersistentTree.Entry entry; while ((entry = scan.next()) != null) {
                        meter.charge(Costs.STORAGE_ROW_READ); meter.guard(); if (matched.getEntry(entry.key()) == null) { targetFrame.values = Encoding.row(entry, table.types()); if (matches(targetFilter, meter)) choose("source", entry, targetFrame.values, nullSource, actions); }
                    }
                }
            }
            unmatchedCost = meter.cost() - beforeUnmatched - (actionCost - earlierActions);
            if (actions.size() == 0) return finish(pending, output.finish(), 0, meter);
            PersistentTree tree = writable.rows(), physical = writable.rows();
            try (var rows = actions.cursor()) { List<SqlValue> action; while ((action = rows.next()) != null) if (!action.get(1).isNull()) {
                byte[] oldKey = action.get(1).asBytes(); tree = tree.remove(oldKey);
                if (action.get(2).isNull() || !Arrays.equals(oldKey, action.get(2).asBytes())) physical = physical.remove(oldKey);
            } }
            try (var rows = actions.cursor()) { List<SqlValue> action; while ((action = rows.next()) != null) {
                Clause clause = clauses.get((int)action.getFirst().asInteger()); byte[] oldKey = bytes(action.get(1)), newKey = bytes(action.get(2));
                PersistentTree.Entry oldEntry = oldKey == null ? null : table.rows().getEntry(oldKey);
                List<SqlValue> old = oldEntry == null ? null : Encoding.row(oldEntry, table.types());
                List<SqlValue> values = newKey == null ? null : action.subList(3 + source.types().size(), action.size());
                if (newKey != null) {
                    if (tree.getEntry(newKey) != null) throw new SqlException("23505", "duplicate primary key");
                    PersistentTree.Entry placed = Encoding.entry(newKey, values, oldEntry, clause.assigned, database.pageLayout(), meter, table.primaryKey());
                    tree = tree.put(placed); physical = physical.put(placed);
                }
                indexes.add(old, oldKey, values, newKey);
            } }
            Engine.Table changed = indexes.apply(writable.changed(physical, rowId));
            try (var rows = actions.cursor()) { List<SqlValue> action; while ((action = rows.next()) != null) if (!action.get(2).isNull()) viewChecks.accept(action.subList(3 + source.types().size(), action.size())); }
            changed = indexes.checkExclusions(changed);
            // Referential action order is DELETE first, then UPDATE/INSERT in action order.
            for (boolean deletes : new boolean[] {true, false}) try (var rows = actions.cursor()) { List<SqlValue> action; while ((action = rows.next()) != null) {
                boolean delete = action.get(2).isNull(); if (delete != deletes) continue;
                byte[] oldKey = bytes(action.get(1)); List<SqlValue> old = oldKey == null ? null : Encoding.row(table.rows().getEntry(oldKey), table.types());
                references.record(old, delete ? null : action.subList(3 + source.types().size(), action.size()));
            } }
            Engine.Snapshot result = references.apply(pending.put(changed), deferred);
            try (var rows = actions.cursor()) { List<SqlValue> action; while ((action = rows.next()) != null) {
                byte[] oldKey = bytes(action.get(1)); List<SqlValue> old = oldKey == null ? null : Encoding.row(table.rows().getEntry(oldKey), table.types());
                output.add(clauses.get((int)action.getFirst().asInteger()).syntax.action(), old, action.get(2).isNull() ? null : action.subList(3 + source.types().size(), action.size()), action.subList(3, 3 + source.types().size()));
            } }
            meter.guard(); return finish(result, output.finish(), actions.size(), meter);
        } catch (IOException | RuntimeException | Error failure) { output.close(); throw failure; }
    }
    private PersistentTree.Entry readTarget(TableScan scan, CostMeter meter) throws IOException {
        long before = meter.cost();
        try { PersistentTree.Entry entry = scan.next(); if (entry != null) { meter.charge(Costs.STORAGE_ROW_READ); meter.guard(); } return entry; }
        finally { probeCost += meter.cost() - before; }
    }
    private Result finish(Engine.Snapshot snapshot, QueryPlan result, long affected, CostMeter meter) {
        result = QueryPlan.counted(result, affected);
        if (meter.writeMetrics != null) result = new CapturedPlan(result, matching.explain(false), matching.actual(meter.cost() - executionStart, loopCost, probeCost, unmatchedCost, sourceActual));
        return new Result(snapshot, result);
    }
    QueryPlan metadata() {
        return new QueryPlan() {
            public List<String> names() { return List.copyOf(output.names); }
            public List<SqlType> types() { return output.expressions.stream().map(ScalarPlan.Expression::type).toList(); }
            public PlanNode explain(boolean verbose) { return matching.explain(verbose); }
            public void requireAnalyze() { source.requireAnalyze(); }
            public Row next(CostMeter meter) { throw new IllegalStateException("metadata-only MERGE cannot execute"); }
            public void close() { output.close(); source.close(); }
        };
    }
    private void choose(String kind, PersistentTree.Entry entry, List<SqlValue> old, List<SqlValue> sourceValues, RowSpool actions) throws IOException {
        long before = environment.meter().cost();
        try { chooseAction(kind, entry, old, sourceValues, actions); }
        finally { actionCost += environment.meter().cost() - before; }
    }
    private void chooseAction(String kind, PersistentTree.Entry entry, List<SqlValue> old, List<SqlValue> sourceValues, RowSpool actions) throws IOException {
        CostMeter meter = environment.meter(); pair.values = concat(old, sourceValues); sourceFrame.values = sourceValues;
        for (int i = 0; i < clauses.size(); i++) {
            Clause clause = clauses.get(i); if (!clause.syntax.kind().equals(kind)) continue;
            if (kind.equals("source")) clause.frame.values = old;
            if (!matches(clause.condition, meter)) continue;
            if (clause.syntax.action().equals("nothing")) return;
            if (entry != null) { if (affected.getEntry(entry.key()) != null) throw new SqlException("21000", "MERGE cannot affect row a second time"); affected = affected.put(entry.key(), new byte[0]); }
            List<SqlValue> values = new ArrayList<>(old); byte[] newKey = null;
            if (!clause.syntax.action().equals("delete")) {
                for (int c = 0; c < clause.columns.size(); c++) values.set(clause.columns.get(c), clause.values.get(c).run(meter));
                for (int c = 0; c < values.size(); c++) if (table.columns().get(c).notNull() && values.get(c).isNull()) throw new SqlException("23502", "NULL in MERGE column");
                checks.evaluate(values, meter);
                newKey = table.primaryKey().length == 0 && entry != null ? entry.key() : Engine.key(table, values, rowId, database.environment());
                if (table.primaryKey().length == 0 && entry == null) { if (rowId == Long.MAX_VALUE) throw SqlException.numericOverflow(); rowId++; }
            } else values = nullTarget;
            List<SqlValue> action = new ArrayList<>(); action.add(SqlValue.ofInteger(SqlType.I32, i)); action.add(binary(entry == null ? null : entry.key())); action.add(binary(newKey)); action.addAll(sourceValues); action.addAll(values); actions.append(action); return;
        }
    }
    private static void ambiguous(Object node, String label) {
        if (node == null) return;
        if (node instanceof List<?> list) { for (Object item : list) ambiguous(item, label); return; }
        if (node instanceof SqlParser.Name name && name.parts().size() > 1 && name.parts().getFirst().equals(label)
                || node instanceof SqlParser.Star star && star.qualifier().equals(List.of(label))) throw new SqlException("42P09", "ambiguous MERGE relation alias");
        for (Object child : SqlParser.depthChildren(node)) ambiguous(child, label);
    }
    private static byte[] bytes(SqlValue value) { return value.isNull() ? null : value.asBytes(); }
    private static SqlValue binary(byte[] value) { return value == null ? SqlValue.nullValue(SqlType.BYTEA) : SqlValue.ofBytes(value); }
    private static List<SqlValue> concat(List<SqlValue> first, List<SqlValue> second) { return new java.util.AbstractList<>() { public int size() { return first.size() + second.size(); } public SqlValue get(int i) { return i < first.size() ? first.get(i) : second.get(i - first.size()); } }; }
    private static ScalarPlan.Expression predicate(ScalarPlan.Resolver resolver, SqlParser.Expr expression) { if (expression == null) return null; ScalarPlan.Expression result = resolver.predicate(expression); if (result.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("MERGE condition must be boolean"); return result; }
    private static boolean matches(ScalarPlan.Expression expression, CostMeter meter) { if (expression == null) return true; SqlValue value = expression.run(meter); return !value.isNull() && value.asBoolean(); }
    private static String label(SqlParser.From source) {
        if (source instanceof SqlParser.Table t) return t.alias() == null ? Engine.name(t.name()) : t.alias();
        if (source instanceof SqlParser.Derived d) return d.alias();
        if (source instanceof SqlParser.TableFunction f) return f.alias() == null ? f.call().name() : f.alias();
        return null;
    }
    /** The result collector is charged to Q1 and is evaluated before publication. */
    private static final class Output implements AutoCloseable {
        final ScalarPlan.Frame frame = new ScalarPlan.Frame();
        final List<ScalarPlan.Expression> expressions = new ArrayList<>();
        final List<String> names = new ArrayList<>();
        final ScalarPlan.Resolver resolver;
        final List<SqlValue> nulls;
        final CostMeter meter;
        final int sourceWidth, targetWidth;
        final boolean oldVisible, newVisible;
        final RowSpool rows;
        String action;
        Output(Engine.Table table, SqlParser.Merge command, List<ScalarPlan.Binding> sourceBindings, QueryEnvironment environment) {
            sourceWidth = sourceBindings.size(); targetWidth = table.columns().size(); meter = environment.meter();
            nulls = table.types().stream().map(SqlValue::nullValue).toList();
            String target = command.alias() == null ? table.name() : command.alias();
            java.util.Set<String> labels = new java.util.HashSet<>(); labels.add(target);
            for (ScalarPlan.Binding binding : sourceBindings) if (!binding.qualifier().isEmpty()) labels.add(String.join(".", binding.qualifier()));
            boolean oldExplicit = !command.oldAlias().equals("old"), newExplicit = !command.newAlias().equals("new");
            if (oldExplicit && labels.contains(command.oldAlias()) || newExplicit && labels.contains(command.newAlias()) || oldExplicit && newExplicit && command.oldAlias().equals(command.newAlias())) throw new SqlException("42712", "duplicate RETURNING alias");
            oldVisible = !labels.contains(command.oldAlias()) && !(newExplicit && command.newAlias().equals(command.oldAlias()));
            newVisible = !labels.contains(command.newAlias()) && !(oldExplicit && command.oldAlias().equals(command.newAlias()));
            List<ScalarPlan.Binding> bindings = new ArrayList<>(sourceBindings); bindings.addAll(table.bindings(command.alias()));
            for (String alias : new String[] {oldVisible ? command.oldAlias() : null, newVisible ? command.newAlias() : null}) if (alias != null)
                for (ScalarPlan.Binding binding : table.bindings(alias)) bindings.add(new ScalarPlan.Binding(binding.qualifier(), binding.name(), binding.type(), true, false, binding.collation()));
            QueryEnvironment scope = environment.fork(); scope.mergeAction = new ScalarPlan.Expression(SqlType.TEXT, ignored -> SqlValue.ofText(action));
            resolver = scope.resolver(bindings, frame);
            for (SqlParser.Item item : ProjectionSyntax.expand(command.returning(), resolver)) {
                if (item.expression() instanceof SqlParser.Star star) {
                    int count = 0;
                    for (int i = 0; i < bindings.size(); i++) {
                        ScalarPlan.Binding binding = bindings.get(i);
                        if (star.qualifier().isEmpty() ? !binding.qualifiedOnly() : star.qualifier().equals(binding.qualifier())) {
                            names.add(binding.name()); expressions.add(resolver.resolve(new SqlParser.ColumnSlot(i), null)); count++;
                        }
                    }
                    if (count == 0 && !star.qualifier().isEmpty()) throw new SqlException("42P01", "unknown RETURNING qualifier");
                } else { names.add(item.alias() == null ? ScalarPlan.outputName(item.expression()) : item.alias()); expressions.add(resolver.resolve(item.expression(), null)); }
            }
            rows = new RowSpool(expressions.stream().map(ScalarPlan.Expression::type).toList(), 0, null, meter.memory());
        }
        boolean[] touched() {
            boolean[] read = resolver.touched(), touched = new boolean[targetWidth];
            for (int i = sourceWidth; i < read.length; i++) touched[(i - sourceWidth) % targetWidth] |= read[i];
            return touched;
        }
        void add(String action, List<SqlValue> old, List<SqlValue> fresh, List<SqlValue> source) {
            if (expressions.isEmpty()) return;
            this.action = action.toUpperCase(java.util.Locale.ROOT);
            List<SqlValue> values = new ArrayList<>(source); values.addAll(fresh == null ? old : fresh);
            if (oldVisible) values.addAll(old == null ? nulls : old); if (newVisible) values.addAll(fresh == null ? nulls : fresh);
            frame.values = values; meter.charge(Costs.ROW_PRODUCED); meter.guard();
            List<SqlValue> result = new ArrayList<>(); for (ScalarPlan.Expression expression : expressions) result.add(expression.run(meter)); rows.append(result);
        }
        QueryPlan finish() {
            frame.values = List.of(); RowSpool.Cursor cursor = rows.cursor();
            return new QueryPlan() {
                public List<String> names() { return List.copyOf(names); }
                public List<SqlType> types() { return expressions.stream().map(ScalarPlan.Expression::type).toList(); }
                public List<CollationDerivation> collations() { return expressions.stream().map(ScalarPlan.Expression::collation).toList(); }
                public Row next(CostMeter ignored) { List<SqlValue> row = cursor.next(); return row == null ? null : new Row(names, row); }
                public void close() { cursor.close(); Output.this.close(); }
            };
        }
        public void close() { frame.values = List.of(); rows.close(); }
    }
}
