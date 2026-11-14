package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import org.jed.storage.PersistentTree;

/** Ordered index definitions, key construction, and bounded canonical build order. */
final class Indexes {
    private Indexes() {}
    static Engine.Index definition(SqlParser.CreateIndex syntax, Engine.Table table, Engine.Snapshot snapshot, QueryEnvironment environment, Database database) {
        if (syntax.method().equals("gist")) return GistIndex.definition(syntax, table, snapshot, environment, database);
        if (syntax.method().equals("gin")) return GinIndex.definition(syntax, table, snapshot, environment, database);
        ScalarPlan.Resolver resolver = environment.resolver(table.bindings(null), new ScalarPlan.Frame()).persisted();
        List<Engine.IndexKey> keys = new ArrayList<>(); List<String> parts = new ArrayList<>();
        for (SqlParser.IndexKey key : syntax.keys()) {
            SqlParser.Expr expression = DefaultExpression.unwrap(key.expression());
            if (key.opclass() != null) throw new SqlException("0A000", "explicit index operator class is not supported");
            ScalarPlan.Expression resolved;
            int column = -1;
            if (expression instanceof SqlParser.Name name) {
                if (name.parts().size() > 1) throw new SqlException("42601", "qualified index column");
                column = table.column(name.parts().getLast()); resolved = resolver.resolve(expression, null);
            } else { validateExpression(expression, resolver); resolved = resolver.resolve(expression, null); }
            if (!keyable(resolved.type())) throw new SqlException("0A000", "type is not indexable: " + resolved.type());
            keys.add(new Engine.IndexKey(column, column >= 0 ? null : (SqlParser.StoredExpression)key.expression(), resolved.type(), key.descending(), key.nullsFirst() == null ? key.descending() : key.nullsFirst(), CollationDerivation.collatable(resolved.type()) ? IndexCollations.derive(expression, table.bindings(null)) : "C"));
            String part = column >= 0 ? table.columns().get(column).name() : expression instanceof SqlParser.Call call ? call.name() : "expr", candidate = part;
            for (int suffix = 1; parts.contains(candidate); suffix++) candidate = part + suffix;
            parts.add(candidate);
        }
        SqlParser.StoredExpression predicate = (SqlParser.StoredExpression)syntax.where();
        if (predicate != null) {
            validateExpression(predicate, resolver);
            if (resolver.resolve(predicate, SqlType.BOOLEAN).type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("index predicate must be boolean");
        }
        String name = syntax.name() == null ? Sequences.autoName(snapshot, table.name(), String.join("_", parts), "idx") : syntax.name().parts().getLast();
        if (syntax.name() != null && name.startsWith("jed_")) throw new SqlException("42939", "reserved index name");
        Engine.Index index = new Engine.Index(name, keys, syntax.unique(), false, predicate, 0, PersistentTree.empty(database.pageSize(), TableLeafCodec.index(), database.pageCodec()));
        return new Engine.Index(name, keys, syntax.unique(), false, predicate, 0, index.entries(), IndexDependencies.forIndex(index, table, environment), null, HostDependencies.forIndex(index, table, environment));
    }
    static boolean keyable(SqlType type) {
        return switch (type.kind()) {
            case JSON, JSONPATH, UNKNOWN, RECORD -> false;
            case ARRAY, RANGE -> keyable(type.element());
            case COMPOSITE -> type.fields().stream().allMatch(field -> keyable(field.type()));
            default -> true;
        };
    }
    static List<Engine.Index> rename(Engine.Table table, String oldName, String newName, boolean qualifier) {
        List<Engine.Index> result = new ArrayList<>();
        for (Engine.Index index : table.indexes()) {
            List<Engine.IndexKey> keys = new ArrayList<>();
            for (Engine.IndexKey key : index.keys()) keys.add(new Engine.IndexKey(key.column(), key.expression() == null ? null : SqlParser.renameColumn(key.expression(), table.name(), oldName, newName, qualifier), key.type(), key.descending(), key.nullsFirst(), key.collation()));
            result.add(new Engine.Index(index.name(), keys, index.unique(), index.constraint(), index.predicate() == null ? null : SqlParser.renameColumn(index.predicate(), table.name(), oldName, newName, qualifier), index.kind(), index.entries(), index.timezoneDependencies(), index.gist(), index.hostDependencies()));
        }
        return result;
    }
    static List<Engine.Index> dropColumn(Engine.Table table, int removed, QueryEnvironment environment) {
        List<Engine.Index> result = new ArrayList<>();
        for (Engine.Index index : table.indexes()) {
            if (new Evaluator(index, table, environment).touched[removed]) continue;
            List<Engine.IndexKey> keys = new ArrayList<>();
            for (Engine.IndexKey key : index.keys()) keys.add(new Engine.IndexKey(key.column() > removed ? key.column() - 1 : key.column(), key.expression(), key.type(), key.descending(), key.nullsFirst(), key.collation()));
            result.add(new Engine.Index(index.name(), keys, index.unique(), index.constraint(), index.predicate(), index.kind(), index.entries(), index.timezoneDependencies(), index.gist(), index.hostDependencies()));
        }
        return result;
    }
    static List<Engine.Index> retype(Engine.Table table, QueryEnvironment environment) {
        List<Engine.Index> result = new ArrayList<>();
        for (Engine.Index index : table.indexes()) {
            ScalarPlan.Resolver resolver = environment.resolver(table.bindings(null), new ScalarPlan.Frame()).persisted();
            List<Engine.IndexKey> keys = new ArrayList<>();
            for (Engine.IndexKey key : index.keys()) {
                SqlType type = key.column() >= 0 ? table.columns().get(key.column()).type() : resolver.resolve(key.expression(), null).type();
                if (index.kind() == 2) GistIndex.validateType(type);
                else if (index.kind() == 1) GinIndex.validateType(type);
                else if (!keyable(type)) throw new SqlException("0A000", "type is not indexable: " + type);
                keys.add(new Engine.IndexKey(key.column(), key.expression(), type, key.descending(), key.nullsFirst(), key.collation()));
            }
            if (index.predicate() != null && resolver.resolve(index.predicate(), SqlType.BOOLEAN).type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("index predicate must be boolean");
            result.add(new Engine.Index(index.name(), keys, index.unique(), index.constraint(), index.predicate(), index.kind(), index.entries(), index.timezoneDependencies(), index.gist(), index.hostDependencies()));
        }
        return result;
    }
    static void validateExpression(SqlParser.Expr syntax, ScalarPlan.Resolver resolver) {
        IndexDependencies.analyze(syntax, resolver);
    }

    static final class Evaluator {
        final Engine.Index index;
        final Environment environment;
        final ScalarPlan.Frame frame = new ScalarPlan.Frame();
        final List<ScalarPlan.Expression> keys = new ArrayList<>();
        final ScalarPlan.Expression predicate;
        final boolean[] touched;
        Evaluator(Engine.Index index, Engine.Table table, QueryEnvironment environment) {
            this.index = index; this.environment = environment.meter().environment();
            ScalarPlan.Resolver resolver = environment.resolver(table.bindings(null), frame).persisted();
            for (Engine.IndexKey key : index.keys()) keys.add(resolver.resolve(key.column() >= 0 ? new SqlParser.Name(List.of(table.columns().get(key.column()).name())) : key.expression(), key.type()));
            SqlParser.Expr syntax = index.predicate() == null ? null : index.predicate().expression();
            predicate = syntax == null ? null : resolver.writtenPredicate(syntax);
            touched = resolver.touched();
        }
        Key key(List<SqlValue> row, byte[] rowKey, CostMeter meter) {
            frame.values = row;
            return meter.unmetered(() -> {
                if (predicate != null) { SqlValue accepted = predicate.run(meter); if (accepted.isNull() || !accepted.asBoolean()) return null; }
                if (index.kind() == 2) {
                    List<SqlValue> values = keys.stream().map(expression -> expression.run(meter)).toList();
                    byte[] key = GistTree.leafKey(values, rowKey); return key == null ? null : new Key(key, null);
                }
                Encoding.Sink bytes = new Encoding.Sink(); boolean nullable = false;
                for (int i = 0; i < keys.size(); i++) {
                    SqlValue value = keys.get(i).run(meter); nullable |= value.isNull(); bytes.raw(slot(value, index.keys().get(i), environment));
                }
                byte[] prefix = index.unique() && !nullable ? bytes.bytes() : null; bytes.raw(rowKey);
                return new Key(bytes.bytes(), prefix);
            });
        }
    }
    record Key(byte[] entry, byte[] uniquePrefix) {}
    static byte[] slot(SqlValue value, Engine.IndexKey key) { return slot(value, key, Environment.empty()); }
    static byte[] slot(SqlValue value, Engine.IndexKey key, Environment environment) {
        byte[] body = value.isNull() ? new byte[0] : IndexCollations.key(value, key.collation(), environment);
        byte[] bytes = new byte[body.length + 1]; bytes[0] = (byte)(value.isNull() ? 1 : 0); System.arraycopy(body, 0, bytes, 1, body.length);
        if (key.descending()) for (int i = 0; i < bytes.length; i++) bytes[i] = (byte)~bytes[i];
        if (key.nullsFirst() != key.descending()) bytes[0] ^= 1;
        return bytes;
    }
    static byte[] after(byte[] prefix) {
        for (int i = prefix.length - 1; i >= 0; i--) if ((prefix[i] & 255) != 255) { byte[] result = Arrays.copyOf(prefix, i + 1); result[i]++; return result; }
        return null;
    }
    static PersistentTree insert(PersistentTree tree, Key key) throws IOException {
        if (key == null) return tree;
        if (key.uniquePrefix() != null) try (var cursor = tree.cursor(key.uniquePrefix(), true, after(key.uniquePrefix()), false)) {
            if (cursor.next() != null) throw new SqlException("23505", "duplicate unique index value");
        }
        return tree.put(new PersistentTree.Entry(key.entry(), new byte[0]));
    }
    static Engine.Index build(Engine.Index index, Engine.Table table, SqlValue[] args, QueryEnvironment environment, Database database, long workMem) throws IOException {
        if (index.kind() == 1) return GinIndex.build(index, table, args, environment, database, workMem);
        Evaluator evaluator = new Evaluator(index, table, environment); CostMeter meter = environment.meter();
        PersistentTree tree = PersistentTree.empty(database.pageSize(), TableLeafCodec.index(), database.pageCodec());
        try (ExternalSorter sorter = new ExternalSorter(List.of(SqlType.BYTEA, SqlType.BYTEA), workMem, database.scratchDirectory(), meter.memory(), (a, b) -> Arrays.compareUnsigned(a.getFirst().asBytes(), b.getFirst().asBytes()));
             TableScan scan = new TableScan(table, null, table.columns().stream().map(Engine.Column::name).toList(), null, args, meter, evaluator.touched, environment)) {
            PersistentTree.Entry entry;
            while ((entry = scan.next()) != null) {
                meter.charge(Costs.STORAGE_ROW_READ); meter.guard();
                Key key = evaluator.key(Encoding.row(entry, table.types()), entry.key(), meter);
                if (key != null) sorter.append(List.of(SqlValue.ofBytes(key.entry()), key.uniquePrefix() == null ? SqlValue.nullValue(SqlType.BYTEA) : SqlValue.ofBytes(key.uniquePrefix())));
            }
            try (var cursor = sorter.finish()) { List<SqlValue> row; while ((row = cursor.next()) != null) tree = insert(tree, new Key(row.getFirst().asBytes(), row.get(1).isNull() ? null : row.get(1).asBytes())); }
        }
        return index.changed(tree);
    }
}
