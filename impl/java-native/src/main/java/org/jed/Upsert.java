package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import org.jed.storage.PersistentTree;

/** Sequential conflict planning over committed rows, with end-state publication. */
final class Upsert implements AutoCloseable {
    private final Engine.Table table;
    private final SqlParser.Conflict syntax;
    private final QueryEnvironment environment;
    private final Database database;
    private final CostMeter meter;
    private final Returning returning;
    private final ViewWrites.Checks viewChecks;
    private final Engine.CheckEvaluator checks;
    private final ScalarPlan.Frame frame = new ScalarPlan.Frame();
    private final List<Integer> targets = new ArrayList<>();
    private final List<ScalarPlan.Expression> assignments = new ArrayList<>();
    private final ScalarPlan.Expression where;
    private final List<Arbiter> arbiters = new ArrayList<>();
    private final List<ExclusionArbiter> exclusions = new ArrayList<>();
    private final RowSpool inserts, updates;
    private final IndexMutation indexes;
    private final ForeignKeys.Batch references;
    private PersistentTree remaining, physical;
    private long rowId;
    private boolean changed, rekeyed;
    private final class ExclusionArbiter {
        private Engine.Index index;
        private final Engine.Exclusion exclusion;
        private final Indexes.Evaluator evaluator;
        private Indexes.Key proposed;
        ExclusionArbiter(Engine.Exclusion exclusion) {
            this.exclusion = exclusion; index = table.indexes().stream().filter(i -> i.name().equals(exclusion.index())).findFirst().orElseThrow();
            evaluator = new Indexes.Evaluator(index, table, environment);
        }
        boolean conflict(List<SqlValue> row, byte[] key) throws IOException {
            proposed = evaluator.key(row, key, meter);
            return proposed != null && Exclusions.participates(exclusion, row) && Exclusions.hasConflict(index, proposed.entry(), key);
        }
        void record() throws IOException { if (proposed != null) index = index.changed(index.entries().put(new PersistentTree.Entry(proposed.entry(), new byte[0]))); }
    }

    private final class Arbiter {
        final Engine.Index index;
        final Indexes.Evaluator evaluator;
        PersistentTree affected;
        Arbiter(Engine.Index index) {
            this.index = index; evaluator = index == null ? null : new Indexes.Evaluator(index, table, environment);
            affected = PersistentTree.empty(database.pageSize(), TableLeafCodec.index(), database.pageCodec());
        }
        byte[] key(List<SqlValue> row) {
            if (index == null) return Engine.key(table, row, 0, database.environment());
            Indexes.Key key = evaluator.key(row, new byte[0], meter);
            return key == null ? null : key.uniquePrefix();
        }
        PersistentTree.Entry existing(byte[] prefix) throws IOException {
            if (prefix == null) return null;
            if (index == null) return remaining.getEntry(prefix);
            try (var cursor = index.entries().cursor(prefix, true, Indexes.after(prefix), false)) {
                PersistentTree.Entry entry;
                while ((entry = cursor.next()) != null) {
                    byte[] key = Arrays.copyOfRange(entry.key(), prefix.length, entry.key().length);
                    PersistentTree.Entry row = remaining.getEntry(key);
                    if (row != null) return row;
                }
            }
            return null;
        }
        void record(List<SqlValue> row) throws IOException {
            byte[] key = key(row); if (key != null) affected = affected.put(new PersistentTree.Entry(key, new byte[0]));
        }
    }

    Upsert(Engine.Table table, SqlParser.Insert command, QueryEnvironment environment, Database database, long workMem, Returning returning, ForeignKeys.Batch references, ViewWrites.Checks viewChecks) {
        this.table = table; this.syntax = command.conflict(); this.environment = environment; this.database = database;
        this.meter = environment.meter(); this.returning = returning; this.viewChecks = viewChecks; this.references = references; this.checks = new Engine.CheckEvaluator(table, environment);
        remaining = table.rows(); physical = table.rows(); rowId = table.nextRowId();
        chooseArbiters(command);
        List<ScalarPlan.Binding> bindings = new ArrayList<>(table.bindings(command.alias()));
        for (Engine.Column column : table.columns()) bindings.add(new ScalarPlan.Binding(List.of("excluded"), column.name(), column.type(), true));
        ScalarPlan.Resolver resolver = environment.resolver(bindings, frame);
        String label = command.alias() == null ? table.name() : command.alias();
        for (SqlParser.Assignment assignment : syntax.assignments()) {
            int ordinal = table.column(assignment.column());
            if (targets.contains(ordinal)) throw new SqlException("42601", "multiple assignments to column");
            Engine.Column column = table.columns().get(ordinal);
            if (column.identity() == 2 && !(assignment.expression() instanceof SqlParser.Default)) throw new SqlException("428C9", "GENERATED ALWAYS column can only be updated to DEFAULT");
            SqlParser.Expr value = assignment.expression() instanceof SqlParser.Default ? Engine.defaultOf(column) : assignment.expression();
            if (label.equals("excluded")) ambiguous(value);
            targets.add(ordinal); assignments.add(Engine.assignment(resolver, value, column.type()));
        }
        if (label.equals("excluded")) ambiguous(syntax.where());
        where = syntax.where() == null ? null : resolver.writtenPredicate(syntax.where());
        if (where != null && where.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("WHERE requires boolean");
        List<SqlType> types = new ArrayList<>(table.types()); types.add(SqlType.BYTEA); types.add(SqlType.BYTEA);
        inserts = new RowSpool(types, workMem, database.scratchDirectory(), meter.memory());
        updates = new RowSpool(types, workMem, database.scratchDirectory(), meter.memory());
        indexes = new IndexMutation(table, environment, workMem, database.scratchDirectory()).upsert();
    }
    private static void ambiguous(SqlParser.Expr expression) {
        if (expression == null) return;
        if (expression instanceof SqlParser.Name name && name.parts().size() > 1 && name.parts().getFirst().equals("excluded")) throw new SqlException("42P09", "ambiguous excluded relation");
        for (SqlParser.Expr child : DefaultExpression.children(expression)) ambiguous(child);
    }
    private void chooseArbiters(SqlParser.Insert command) {
        if (syntax.columns().isEmpty() && syntax.constraint() == null) {
            if (!syntax.nothing()) throw new SqlException("42601", "DO UPDATE requires a conflict target");
            if (table.primaryKey().length > 0) arbiters.add(new Arbiter(null));
            for (Engine.Index index : table.indexes()) if (index.unique()) arbiters.add(new Arbiter(index));
            for (Engine.Exclusion exclusion : table.exclusions()) exclusions.add(new ExclusionArbiter(exclusion));
            return;
        }
        if (syntax.constraint() != null) {
            if (table.primaryKey().length > 0 && syntax.constraint().equals(Sequences.primaryKeyName(table.name()))) { arbiters.add(new Arbiter(null)); return; }
            for (Engine.Index index : table.indexes()) if (index.unique() && index.constraint() && index.name().equals(syntax.constraint())) { arbiters.add(new Arbiter(index)); return; }
            for (Engine.Exclusion exclusion : table.exclusions()) if (exclusion.name().equals(syntax.constraint())) {
                if (!syntax.nothing()) throw new SqlException("42809", "DO UPDATE cannot use an exclusion arbiter");
                exclusions.add(new ExclusionArbiter(exclusion)); return;
            }
            throw new SqlException("42704", "constraint does not exist: " + syntax.constraint());
        }
        List<Integer> columns = new ArrayList<>(); boolean decorationsMatch = true;
        for (SqlParser.ConflictElement element : syntax.columns()) {
            if (element.ordered()) throw new SqlException("42P10", "ordering is not allowed in conflict target");
            int ordinal = table.column(element.column()); SqlType type = table.columns().get(ordinal).type();
            if (!columns.contains(ordinal)) columns.add(ordinal);
            if (element.collation() != null) {
                meter.environment().collation(element.collation());
                decorationsMatch &= element.collation().equals(table.columns().get(ordinal).collation()) && (type.kind() == SqlType.Kind.TEXT || type.kind() == SqlType.Kind.ARRAY && type.element().kind() == SqlType.Kind.TEXT);
            }
            if (element.opclass() != null) {
                String opclass = requireOpclass(element.opclass());
                decorationsMatch &= opclass.equals(opclass(type));
            }
        }
        if (syntax.predicate() != null) {
            predicateStructure(syntax.predicate());
            environment.resolver(table.bindings(command.alias()), new ScalarPlan.Frame()).resolve(syntax.predicate(), null);
        }
        int[] requested = columns.stream().mapToInt(Integer::intValue).sorted().toArray();
        if (decorationsMatch && Arrays.equals(requested, Arrays.stream(table.primaryKey()).sorted().toArray())) { arbiters.add(new Arbiter(null)); return; }
        if (decorationsMatch) for (boolean partial : List.of(false, true)) for (Engine.Index index : table.indexes()) {
            if (!index.unique() || (index.predicate() != null) != partial || index.keys().stream().anyMatch(key -> key.column() < 0)) continue;
            if (!Arrays.equals(requested, index.keys().stream().mapToInt(Engine.IndexKey::column).sorted().toArray())) continue;
            if (partial && !PredicateImplication.implies(syntax.predicate(), index.predicate(), table, command.alias(), environment)) continue;
            arbiters.add(new Arbiter(index)); return;
        }
        throw new SqlException("42P10", "no unique constraint matches the conflict target");
    }
    static String requireOpclass(String name) {
        String opclass = name.equals("varchar_ops") ? "text_ops" : name;
        if (!List.of("array_ops", "range_ops", "record_ops").contains(opclass) && ScalarCatalog.FACTS.stream().noneMatch(fact -> fact.btreeOpclass().equals(opclass))) throw new SqlException("42704", "unknown operator class: " + name);
        return opclass;
    }
    private static String opclass(SqlType type) {
        return switch (type.kind()) {
            case ARRAY -> "array_ops"; case RANGE -> "range_ops"; case COMPOSITE -> "record_ops";
            default -> ScalarCatalog.FACTS.stream().filter(fact -> fact.id().equals(type.kind().name().toLowerCase(java.util.Locale.ROOT))).map(ScalarCatalog.Fact::btreeOpclass).findFirst().orElse("");
        };
    }
    private static void predicateStructure(SqlParser.Expr expression) {
        expression = DefaultExpression.unwrap(expression);
        if (expression instanceof SqlParser.Parameter) throw new SqlException("42P02", "parameter in conflict predicate");
        if (expression instanceof SqlParser.Window) throw new SqlException("42P20", "window in conflict predicate");
        if (expression instanceof SqlParser.Subquery || expression instanceof SqlParser.Exists || expression instanceof SqlParser.In in && in.query() != null) throw new SqlException("0A000", "subquery in conflict predicate");
        if (expression instanceof SqlParser.Call call && AggregatePlan.aggregateName(call.name())) throw new SqlException("42803", "aggregate in conflict predicate");
        for (SqlParser.Expr child : DefaultExpression.children(expression)) predicateStructure(child);
    }
    private long affectedRows;
    long affectedRows() { return affectedRows; }
    void accept(List<SqlValue> proposed) throws IOException {
        meter.guard(); Engine.validate(table, table.rows(), proposed, null, checks, meter);
        PersistentTree.Entry existing = null; boolean affected = false;
        for (Arbiter arbiter : arbiters) {
            byte[] key = arbiter.key(proposed);
            existing = arbiter.existing(key);
            if (existing != null) break;
            affected |= key != null && arbiter.affected.getEntry(key) != null;
        }
        if (syntax.nothing() && (existing != null || affected)) return;
        if (!exclusions.isEmpty()) {
            byte[] key = Engine.key(table, proposed, rowId, database.environment());
            for (ExclusionArbiter exclusion : exclusions) if (exclusion.conflict(proposed, key)) return;
        }
        if (existing == null && affected) throw new SqlException("21000", "conflict update cannot affect a row twice");
        List<SqlValue> values = proposed, old = null; byte[] oldKey = null;
        if (existing != null) {
            old = Encoding.row(existing, table.types()); List<SqlValue> scope = new ArrayList<>(old); scope.addAll(proposed); frame.values = scope;
            if (where != null) { SqlValue condition = where.run(meter); if (condition.isNull() || !condition.asBoolean()) return; }
            values = new ArrayList<>(old);
            for (int i = 0; i < targets.size(); i++) values.set(targets.get(i), assignments.get(i).run(meter));
            Engine.validate(table, table.rows(), values, existing.key(), checks, meter);
            oldKey = existing.key(); remaining = remaining.remove(oldKey);
        }
        viewChecks.accept(values);
        byte[] newKey = table.primaryKey().length == 0 && oldKey != null ? oldKey : Engine.key(table, values, rowId, database.environment());
        if (oldKey != null && !Arrays.equals(oldKey, newKey)) rekeyed = true;
        if (oldKey == null && table.primaryKey().length == 0) { if (rowId == Long.MAX_VALUE) throw SqlException.numericOverflow(); rowId++; }
        List<SqlValue> staged = new ArrayList<>(values); staged.add(oldKey == null ? SqlValue.nullValue(SqlType.BYTEA) : SqlValue.ofBytes(oldKey)); staged.add(SqlValue.ofBytes(newKey));
        (oldKey == null ? inserts : updates).append(staged);
        indexes.add(old, oldKey, values, newKey);
        references.record(old, values);
        for (Arbiter arbiter : arbiters) arbiter.record(values);
        for (ExclusionArbiter exclusion : exclusions) exclusion.record();
        if (returning != null) returning.add(old, values);
        affectedRows++; changed = true;
    }
    Engine.Table finish() throws IOException {
        if (!changed) return table;
        PersistentTree tree = remaining;
        if (rekeyed) try (var cursor = updates.cursor()) {
            List<SqlValue> staged;
            while ((staged = cursor.next()) != null) physical = physical.remove(staged.get(staged.size() - 2).asBytes());
        }
        for (RowSpool spool : List.of(inserts, updates)) try (var cursor = spool.cursor()) {
            List<SqlValue> staged;
            while ((staged = cursor.next()) != null) {
                byte[] key = staged.getLast().asBytes();
                if (tree.getEntry(key) != null) throw new SqlException("23505", "duplicate primary key");
                List<SqlValue> values = staged.subList(0, table.columns().size());
                PersistentTree.Entry entry = Encoding.entry(key, values, null, null, database.pageLayout(), meter, table.primaryKey());
                tree = tree.put(entry); physical = physical.put(entry);
            }
        }
        return indexes.checkExclusions(indexes.apply(table.changed(physical, rowId)));
    }
    @Override public void close() { inserts.close(); updates.close(); indexes.close(); }
}
