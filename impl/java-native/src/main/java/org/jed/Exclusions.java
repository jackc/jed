package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import org.jed.storage.PersistentTree;

/** Exclusion schema and final-state probes over their ordinary or GiST backing indexes. */
final class Exclusions {
    private Exclusions() {}
    static Engine.Table define(Engine.Table table, SqlParser.Constraint syntax, Engine.Snapshot snapshot, QueryEnvironment environment, Database database) {
        String method = syntax.method();
        if (!List.of("btree", "gist").contains(method)) throw new SqlException(method.equals("gin") ? "0A000" : "42704", "access method does not support exclusions: " + method);
        List<Integer> columns = new ArrayList<>(); List<String> operators = new ArrayList<>(); List<Engine.IndexKey> keys = new ArrayList<>();
        for (SqlParser.ExcludeElement element : syntax.excludeElements()) {
            var expression = DefaultExpression.unwrap(element.expression());
            if (!(expression instanceof SqlParser.Name ref)) throw new SqlException("0A000", "exclusion expressions are not supported");
            if (ref.parts().size() != 1) throw new SqlException("42601", "qualified exclusion column");
            int ordinal = table.column(ref.parts().getLast()); SqlType type = table.columns().get(ordinal).type(); String operator = element.operator();
            if (method.equals("gist")) {
                if (columns.contains(ordinal)) throw new SqlException("42701", "duplicate GiST exclusion column");
                if (!List.of("&&", "=").contains(operator)) throw new SqlException("0A000", "unsupported GiST exclusion strategy");
                if (operator.equals("&&") && type.kind() != SqlType.Kind.RANGE || operator.equals("=") && type.kind() == SqlType.Kind.RANGE) throw new SqlException("42704", "exclusion operator has no operator class");
                GistIndex.validateType(type);
            } else {
                if (!operator.equals("=")) throw new SqlException("42809", "B-tree exclusions require equality");
                if (!Indexes.keyable(type)) throw new SqlException("42704", "exclusion type has no operator class");
            }
            columns.add(ordinal); operators.add(operator); keys.add(new Engine.IndexKey(ordinal, null, type, false, false, table.columns().get(ordinal).collation()));
        }
        var predicate = (SqlParser.StoredExpression)syntax.expression();
        if (predicate != null) {
            var resolver = environment.resolver(table.bindings(null), new ScalarPlan.Frame()).persisted(); Indexes.validateExpression(predicate, resolver);
            if (method.equals("gist") && !HostDependencies.collect(predicate, resolver, true).isEmpty()) throw new SqlException("42P17", "index method cannot persist host dependencies");
            if (resolver.resolve(predicate, SqlType.BOOLEAN).type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("exclusion predicate requires boolean");
            if (method.equals("gist") && IndexDependencies.analyze(predicate, resolver).present()) throw new SqlException("42P17", "GiST predicate cannot persist timezone dependencies");
        }
        String name = syntax.name();
        if (name == null) name = Sequences.autoName(snapshot.put(table), table.name(), String.join("_", columns.stream().map(i -> table.columns().get(i).name()).toList()), "excl");
        else {
            if (ForeignKeys.nameTaken(table, name)) throw new SqlException("42710", "constraint already exists: " + name);
            String candidate = name;
            if (snapshot.tables().containsKey(name) || snapshot.views().containsKey(name) || snapshot.sequences().containsKey(name) || table.indexes().stream().anyMatch(index -> index.name().equals(candidate)) || snapshot.tables().values().stream().flatMap(other -> other.indexes().stream()).anyMatch(index -> index.name().equals(candidate))) throw new SqlException("42P07", "relation already exists: " + name);
        }
        if (syntax.name() != null && name.startsWith("jed_")) throw new SqlException("42939", "reserved index name");
        var index = new Engine.Index(name, keys, false, false, predicate, method.equals("gist") ? 2 : 0, PersistentTree.empty(database.pageSize(), TableLeafCodec.index(), database.pageCodec()));
        if (index.kind() == 0) index = new Engine.Index(name, keys, false, false, predicate, 0, index.entries(), IndexDependencies.forIndex(index, table, environment), null, HostDependencies.forIndex(index, table, environment));
        var indexes = new ArrayList<>(table.indexes()); indexes.add(index); indexes.sort((a,b) -> SqlText.compare(a.name(),b.name()));
        var exclusions = new ArrayList<>(table.exclusions()); exclusions.add(new Engine.Exclusion(name, name, columns, operators)); exclusions.sort((a,b) -> SqlText.compare(a.name(),b.name()));
        return table.withIndexes(indexes).withExclusions(exclusions);
    }
    static void validateAlter(Engine.Table table, QueryEnvironment environment) throws IOException {
        for (Engine.Exclusion exclusion : table.exclusions()) {
            Engine.Index index = table.indexes().stream().filter(value -> value.name().equals(exclusion.index())).findFirst().orElseThrow();
            Indexes.Evaluator evaluator = new Indexes.Evaluator(index, table, environment);
            try (var left = table.rows().cursor()) {
                PersistentTree.Entry a;
                while ((a = left.next()) != null) {
                    List<SqlValue> rowA = Encoding.row(a, table.types());
                    if (!admitted(evaluator, rowA, a.key(), environment.meter())) continue;
                    try (var right = table.rows().cursor(a.key(), false, null, false)) {
                        PersistentTree.Entry b;
                        while ((b = right.next()) != null) {
                            List<SqlValue> rowB = Encoding.row(b, table.types());
                            if (!admitted(evaluator, rowB, b.key(), environment.meter())) continue;
                            environment.meter().charge(Costs.CONSTRAINT_CHECK); environment.meter().guard();
                            if (conflict(exclusion, rowA, rowB)) throw violation(exclusion);
                        }
                    }
                }
            }
        }
    }
    private static boolean admitted(Indexes.Evaluator evaluator, List<SqlValue> row, byte[] key, CostMeter meter) {
        // NULL values are admitted by the predicate but cannot make a strict WITH comparison true.
        if (evaluator.predicate == null) return true;
        evaluator.frame.values = row;
        return meter.unmetered(() -> { SqlValue result = evaluator.predicate.run(meter); return !result.isNull() && result.asBoolean(); });
    }
    static boolean conflict(Engine.Exclusion exclusion, List<SqlValue> left, List<SqlValue> right) {
        for (int i = 0; i < exclusion.columns().size(); i++) { int ordinal = exclusion.columns().get(i); SqlValue a = left.get(ordinal), b = right.get(ordinal);
            if (a.isNull() || b.isNull()) return false;
            if (exclusion.operators().get(i).equals("&&") ? !a.asRange().overlaps(b.asRange()) : ScalarPlan.compare(a, b, null) != 0) return false;
        }
        return true;
    }
    static Engine.Exclusion forIndex(Engine.Table table, String name) { return table.exclusions().stream().filter(exclusion -> exclusion.index().equals(name)).findFirst().orElse(null); }
    static void check(Engine.Table table, Engine.Index index, byte[] entry, byte[] storageKey) throws IOException {
        Engine.Exclusion exclusion = forIndex(table, index.name()); if (exclusion == null) return;
        if (index.kind() == 2) {
            if (index.gist().conflicts(entry, exclusion.operators())) throw violation(exclusion);
        } else {
            byte[] prefix = Arrays.copyOf(entry, entry.length - storageKey.length);
            // Every component uses the ordinary nullable key slot; NULL exempts the row.
            // Caller omits exempt keys before staging probes through participates().
            try (var cursor = index.entries().cursor(prefix, true, Indexes.after(prefix), false)) {
                PersistentTree.Entry candidate;
                while ((candidate = cursor.next()) != null) if (!Arrays.equals(candidate.key(), entry)) throw violation(exclusion);
            }
        }
    }
    static boolean hasConflict(Engine.Index index, byte[] entry, byte[] storageKey) throws IOException {
        if (index.kind() == 2) {
            GistTree tree = index.gist();
            if (tree == null) tree = GistTree.build(index.keys().stream().map(Engine.IndexKey::type).toList(), index.entries(), null);
            return tree.conflicts(entry, index.keys().stream().map(key -> key.type().kind() == SqlType.Kind.RANGE ? "&&" : "=").toList(), false);
        }
        byte[] prefix = Arrays.copyOf(entry, entry.length - storageKey.length);
        try (var cursor = index.entries().cursor(prefix, true, Indexes.after(prefix), false)) { return cursor.next() != null; }
    }
    static boolean participates(Engine.Exclusion exclusion, List<SqlValue> row) {
        for (int i = 0; i < exclusion.columns().size(); i++) { SqlValue value = row.get(exclusion.columns().get(i));
            if (value.isNull() || exclusion.operators().get(i).equals("&&") && value.asRange().isEmpty()) return false; }
        return true;
    }
    static SqlException violation(Engine.Exclusion exclusion) { return new SqlException("23P01", "conflicting key value violates exclusion constraint: " + exclusion.name()); }
}
