package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import org.jed.storage.PersistentTree;

/** Referential metadata and statement-local mutation closures. */
final class ForeignKeys {
    private ForeignKeys() {}
    static Engine.Table define(Engine.Table table, SqlParser.Constraint syntax, Engine.Snapshot snapshot) {
        List<Integer> local = ordinals(table, syntax.columns());
        if (syntax.reference().parts().size() > 2) throw new SqlException("42P01", "unknown referenced database");
        String parentName = syntax.reference().parts().getLast();
        Engine.Table parent = parentName.equals(table.name()) ? table : snapshot.tables().get(parentName);
        if (parent == null) throw new SqlException(snapshot.views().containsKey(parentName) || snapshot.sequences().containsKey(parentName) ? "42809" : "42P01", "referenced relation is not a table");
        List<Integer> referenced = syntax.referenceColumns().isEmpty() ? Arrays.stream(parent.primaryKey()).boxed().toList() : ordinals(parent, syntax.referenceColumns());
        if (syntax.referenceColumns().isEmpty() && referenced.isEmpty()) throw new SqlException("42704", "referenced table has no primary key");
        if (local.size() != referenced.size()) throw new SqlException("42830", "foreign key column counts differ");
        unique(parent, referenced);
        for (int i = 0; i < local.size(); i++) if (!comparable(table.columns().get(local.get(i)).type(), parent.columns().get(referenced.get(i)).type())) throw SqlException.typeMismatch("incompatible foreign key columns");
        String name = syntax.name();
        if (name == null) {
            String cols = String.join("_", local.stream().map(i -> table.columns().get(i).name()).toList());
            for (int suffix = 0; ; suffix++) {
                String label = "fkey" + (suffix == 0 ? "" : suffix), left = table.name(), right = cols;
                int available = 63 - SqlText.encode(label).length - 2;
                while (SqlText.encode(left).length + SqlText.encode(right).length > available) {
                    if (SqlText.encode(left).length > SqlText.encode(right).length) left = left.substring(0, left.offsetByCodePoints(left.length(), -1));
                    else right = right.substring(0, right.offsetByCodePoints(right.length(), -1));
                }
                String candidate = left + "_" + right + "_" + label;
                if (!nameTaken(table, candidate) && snapshot.tables().values().stream().noneMatch(other -> nameTaken(other, candidate))) { name = candidate; break; }
            }
        } else if (nameTaken(table, name)) throw new SqlException("42710", "constraint already exists: " + name);
        List<Engine.ForeignKey> keys = new ArrayList<>(table.foreignKeys());
        keys.add(new Engine.ForeignKey(name, local, parentName, referenced, action(syntax.onDelete()), action(syntax.onUpdate()), syntax.matchFull()));
        keys.sort((a, b) -> Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name())));
        return table.withForeignKeys(keys);
    }
    static boolean nameTaken(Engine.Table table, String name) {
        return table.checks().stream().anyMatch(c -> c.name().equals(name)) || table.indexes().stream().anyMatch(i -> i.constraint() && i.name().equals(name)) || table.foreignKeys().stream().anyMatch(f -> f.name().equals(name));
    }
    static Engine.Snapshot renameParent(Engine.Snapshot snapshot, String oldName, String newName) {
        for (Engine.Table table : snapshot.tables().values()) {
            List<Engine.ForeignKey> keys = new ArrayList<>(); boolean changed = false;
            for (Engine.ForeignKey key : table.foreignKeys()) {
                if (key.parent().equals(oldName)) { key = new Engine.ForeignKey(key.name(), key.local(), newName, key.referenced(), key.onDelete(), key.onUpdate(), key.matchFull()); changed = true; }
                keys.add(key);
            }
            if (changed) snapshot = snapshot.put(table.withForeignKeys(keys));
        }
        return snapshot;
    }
    static Engine.Snapshot dropColumn(Engine.Snapshot snapshot, String name, int removed, boolean cascade) {
        for (Engine.Table table : snapshot.tables().values()) {
            List<Engine.ForeignKey> keys = new ArrayList<>(); boolean changed = false;
            for (Engine.ForeignKey key : table.foreignKeys()) {
                if (table.name().equals(name) && key.local().contains(removed)) { changed = true; continue; }
                if (key.parent().equals(name) && key.referenced().contains(removed)) {
                    if (!cascade && !table.name().equals(name)) throw new SqlException("2BP01", "column has foreign key dependents");
                    changed = true; continue;
                }
                List<Integer> local = table.name().equals(name) ? key.local().stream().map(i -> i > removed ? i - 1 : i).toList() : key.local();
                List<Integer> referenced = key.parent().equals(name) ? key.referenced().stream().map(i -> i > removed ? i - 1 : i).toList() : key.referenced();
                changed |= !local.equals(key.local()) || !referenced.equals(key.referenced());
                keys.add(new Engine.ForeignKey(key.name(), local, key.parent(), referenced, key.onDelete(), key.onUpdate(), key.matchFull()));
            }
            if (changed) snapshot = snapshot.put(table.withForeignKeys(keys));
        }
        return snapshot;
    }
    static Engine.Snapshot validateKeys(Engine.Snapshot snapshot, String edited, boolean cascade) {
        for (Engine.Table table : snapshot.tables().values()) {
            List<Engine.ForeignKey> keys = new ArrayList<>();
            for (Engine.ForeignKey key : table.foreignKeys()) {
                if (!table.name().equals(edited) && !key.parent().equals(edited)) { keys.add(key); continue; }
                Engine.Table parent = snapshot.tables().get(key.parent());
                try { unique(parent, key.referenced()); }
                catch (SqlException failure) {
                    if (!failure.sqlState().equals("42830")) throw failure;
                    if (cascade) continue;
                    throw new SqlException("2BP01", "key has foreign key dependents");
                }
                for (int i = 0; i < key.local().size(); i++) if (!comparable(table.columns().get(key.local().get(i)).type(), parent.columns().get(key.referenced().get(i)).type())) throw SqlException.typeMismatch("incompatible foreign key columns");
                keys.add(key);
            }
            if (keys.size() != table.foreignKeys().size()) snapshot = snapshot.put(table.withForeignKeys(keys));
        }
        return snapshot;
    }
    private static List<Integer> ordinals(Engine.Table table, List<String> names) {
        List<Integer> ordinals = new ArrayList<>();
        for (String name : names) { int ordinal = table.column(name); if (ordinals.contains(ordinal)) throw new SqlException("42701", "repeated foreign key column"); ordinals.add(ordinal); }
        return ordinals;
    }
    private static int action(String name) { return name == null ? 0 : switch (name) { case "restrict" -> 1; case "cascade" -> 2; case "set_null" -> 3; case "set_default" -> 4; default -> 0; }; }
    static boolean comparable(SqlType child, SqlType parent) {
        if (child.equals(parent)) return true;
        if (child.isInteger()) return parent.isInteger() || parent.kind() == SqlType.Kind.DECIMAL;
        if (child.isFloat()) return parent.isFloat();
        return child.kind() == parent.kind() && List.of(SqlType.Kind.TEXT, SqlType.Kind.DECIMAL, SqlType.Kind.TIMESTAMP, SqlType.Kind.TIMESTAMPTZ, SqlType.Kind.INTERVAL).contains(child.kind());
    }
    /** Null denotes the table's primary tree; otherwise returns a matching non-partial unique index. */
    static Engine.Index unique(Engine.Table table, List<Integer> columns) {
        int[] requested = columns.stream().mapToInt(Integer::intValue).sorted().toArray();
        if (Arrays.equals(requested, Arrays.stream(table.primaryKey()).sorted().toArray())) return null;
        for (Engine.Index index : table.indexes()) if (index.unique() && index.predicate() == null && index.keys().stream().allMatch(key -> key.column() >= 0)
            && Arrays.equals(requested, index.keys().stream().mapToInt(Engine.IndexKey::column).sorted().toArray())) return index;
        throw new SqlException("42830", "referenced columns have no unique key");
    }
    private static SqlValue equalAs(SqlValue value, SqlType target) {
        if (value.isNull()) return null;
        if (value.type().kind() == target.kind()) return value;
        try {
            SqlValue cast = SqlCasts.cast(value, target);
            if (value.type().isFloat() && target.isFloat() && !(Double.isNaN(value.asFloat()) && Double.isNaN(cast.asFloat())) && value.asFloat() != cast.asFloat()) return null;
            if (value.type().kind() == SqlType.Kind.DECIMAL && target.isInteger() && value.asDecimal().compareTo(Decimal.parse(Long.toString(cast.asInteger()))) != 0) return null;
            return cast;
        } catch (SqlException failure) { if (failure.sqlState().equals("22003")) return null; throw failure; }
    }
    static boolean exists(Engine.Table parent, List<Integer> referenced, List<SqlValue> tuple, CostMeter meter) throws IOException {
        Environment host = meter.environment();
        if (!CollationCompatibility.table(parent, meter)) {
            try (var cursor = parent.rows().cursor()) { PersistentTree.Entry entry;
                while ((entry = cursor.next()) != null) {
                    meter.guard(); List<SqlValue> values = Encoding.row(entry, parent.types()); boolean equal = true;
                    for (int i = 0; i < referenced.size(); i++) {
                        int column = referenced.get(i); SqlValue value = equalAs(tuple.get(i), parent.columns().get(column).type());
                        if (value == null || values.get(column).isNull() || ScalarPlan.compare(value, values.get(column), null) != 0) { equal = false; break; }
                    }
                    if (equal) return true;
                }
            }
            return false;
        }
        Engine.Index index = unique(parent, referenced); Encoding.Sink prefix = new Encoding.Sink();
        List<Integer> order = index == null ? Arrays.stream(parent.primaryKey()).boxed().toList() : index.keys().stream().map(Engine.IndexKey::column).toList();
        for (int i = 0; i < order.size(); i++) {
            int ordinal = order.get(i), position = referenced.indexOf(ordinal); SqlValue value = equalAs(tuple.get(position), parent.columns().get(ordinal).type());
            if (value == null) return false;
            prefix.raw(index == null ? IndexCollations.key(value, parent.columns().get(ordinal).collation(), host) : Indexes.slot(value, index.keys().get(i), host));
        }
        byte[] key = prefix.bytes();
        if (index == null) return parent.rows().getEntry(key) != null;
        try (var cursor = index.entries().cursor(key, true, Indexes.after(key), false)) { return cursor.next() != null; }
    }
    static void child(Engine.Table table, List<SqlValue> values, Engine.Snapshot snapshot, CostMeter meter) throws IOException {
        for (Engine.ForeignKey key : table.foreignKeys()) {
            List<SqlValue> tuple = key.local().stream().map(values::get).toList(); long nulls = tuple.stream().filter(SqlValue::isNull).count();
            if (nulls > 0) { if (key.matchFull() && nulls < tuple.size()) throw violation(key); continue; }
            Engine.Table parent = snapshot.tables().get(key.parent());
            if (parent == null || !exists(parent, key.referenced(), tuple, meter)) throw violation(key);
        }
    }
    private static SqlException violation(Engine.ForeignKey key) { return new SqlException("23503", "foreign key violated: " + key.name()); }
    static void validateAlter(Engine.Table original, Engine.Table table, Engine.Snapshot snapshot, QueryEnvironment environment, boolean rewrite) throws IOException {
        List<Engine.ForeignKey> checks = table.foreignKeys().stream().filter(key -> rewrite || !original.foreignKeys().contains(key)).toList();
        if (checks.isEmpty()) return;
        try (var rows = table.rows().cursor()) {
            PersistentTree.Entry entry;
            while ((entry = rows.next()) != null) {
                List<SqlValue> values = Encoding.row(entry, table.types());
                for (Engine.ForeignKey key : checks) {
                    List<SqlValue> tuple = key.local().stream().map(values::get).toList(); long nulls = tuple.stream().filter(SqlValue::isNull).count();
                    if (nulls > 0) { if (key.matchFull() && nulls < tuple.size()) throw violation(key); continue; }
                    if (!key.parent().equals(table.name())) {
                        Engine.Table parent = snapshot.tables().get(key.parent());
                        if (parent == null || !exists(parent, key.referenced(), tuple, environment.meter())) throw violation(key);
                        continue;
                    }
                    boolean found = false;
                    try (var candidates = table.rows().cursor()) { PersistentTree.Entry candidate;
                        while ((candidate = candidates.next()) != null) {
                            environment.meter().charge(Costs.CONSTRAINT_CHECK); environment.meter().guard();
                            List<SqlValue> parent = Encoding.row(candidate, table.types()); boolean equal = true;
                            for (int i = 0; i < key.referenced().size(); i++) {
                                int column = key.referenced().get(i); SqlValue left = equalAs(tuple.get(i), table.columns().get(column).type()), right = parent.get(column);
                                if (left == null || right.isNull() || !Arrays.equals(Encoding.key(left), Encoding.key(right))) { equal = false; break; }
                            }
                            if (equal) { found = true; break; }
                        }
                    }
                    if (!found) throw violation(key);
                }
            }
        }
    }

    static final class Deferred implements AutoCloseable {
        private record Pending(String domain, Batch batch) {}
        String domain = "main";
        private final List<Pending> pending = new ArrayList<>();
        void validate(Engine.Snapshot snapshot) throws IOException { for (Pending item : pending) item.batch().validate(snapshot.domain(item.domain())); }
        void retain(List<Batch> batches) { for (Batch batch : batches) { batch.retained = true; pending.add(new Pending(domain, batch)); } }
        @Override public void close() { for (Pending item : pending) { item.batch().retained = false; item.batch().close(); } pending.clear(); }
    }
    static final class Batch implements AutoCloseable {
        final Engine.Table original;
        final Database database;
        final QueryEnvironment environment;
        final long workMem;
        final CostMeter meter;
        final RowSpool rows;
        private boolean retained;
        Batch(Engine.Table original, Database database, QueryEnvironment environment, long workMem, Engine.Snapshot snapshot) {
            this.original = original; this.database = database; this.environment = environment; this.workMem = workMem; meter = environment.meter();
            List<SqlType> types = new ArrayList<>(List.of(SqlType.BOOLEAN, SqlType.BOOLEAN)); types.addAll(original.types()); types.addAll(original.types());
            boolean needed = !original.foreignKeys().isEmpty() || snapshot.tables().values().stream().anyMatch(table -> table.foreignKeys().stream().anyMatch(key -> key.parent().equals(original.name())));
            rows = needed ? new RowSpool(types, workMem, database.scratchDirectory(), meter.memory()) : null;
        }
        void record(List<SqlValue> old, List<SqlValue> next) {
            if (rows == null) return;
            List<SqlValue> values = new ArrayList<>(); values.add(SqlValue.ofBoolean(old != null)); values.add(SqlValue.ofBoolean(next != null));
            List<SqlValue> nulls = original.types().stream().map(SqlValue::nullValue).toList();
            values.addAll(old == null ? nulls : old); values.addAll(next == null ? nulls : next); rows.append(values);
        }
        private List<SqlValue> old(List<SqlValue> row) { return row.getFirst().asBoolean() ? row.subList(2, 2 + original.columns().size()) : null; }
        private List<SqlValue> next(List<SqlValue> row) { return row.get(1).asBoolean() ? row.subList(2 + original.columns().size(), row.size()) : null; }
        private static final class Closure implements AutoCloseable {
            final List<Batch> pending = new ArrayList<>(), owned = new ArrayList<>();
            @Override public void close() { for (Batch batch : owned) batch.close(); }
        }
        Engine.Snapshot apply(Engine.Snapshot snapshot) throws IOException {
            return apply(snapshot, null);
        }
        Engine.Snapshot apply(Engine.Snapshot snapshot, Deferred deferred) throws IOException {
            try (Closure closure = new Closure()) {
                snapshot = apply(snapshot, 0, closure);
                if (deferred == null) for (Batch batch : closure.pending) batch.validate(snapshot);
                else deferred.retain(closure.pending);
                return snapshot;
            }
        }
        private Engine.Snapshot apply(Engine.Snapshot snapshot, int depth, Closure closure) throws IOException {
            if (rows == null || rows.size() == 0) return snapshot;
            if (depth > 256) throw new SqlException("54001", "foreign key action depth exceeded");
            // Each incoming action sees the prior sibling's complete write batch.
            for (Engine.Table initialChild : snapshot.tables().values()) for (Engine.ForeignKey key : initialChild.foreignKeys()) if (key.parent().equals(original.name())) {
                if (key.onDelete() >= 2 || key.onUpdate() >= 2) snapshot = action(snapshot, initialChild.name(), key, depth, closure);
            }
            closure.pending.add(this);
            return snapshot;
        }
        private void validate(Engine.Snapshot snapshot) throws IOException {
            for (Engine.Table child : snapshot.tables().values()) for (Engine.ForeignKey key : child.foreignKeys()) if (key.parent().equals(original.name())) {
                try (var cursor = rows.cursor()) { List<SqlValue> row; while ((row = cursor.next()) != null) {
                    List<SqlValue> old = old(row), next = next(row); if (old == null || (next == null ? key.onDelete() : key.onUpdate()) >= 2) continue;
                    List<SqlValue> tuple = key.referenced().stream().map(old::get).toList();
                    if (tuple.stream().anyMatch(SqlValue::isNull) || exists(snapshot.tables().get(original.name()), key.referenced(), tuple, meter)) continue;
                    if (referenced(child, key, tuple)) throw violation(key);
                } }
            }
            Engine.Table current = snapshot.tables().get(original.name());
            if (current != null && !current.foreignKeys().isEmpty()) try (var cursor = rows.cursor()) { List<SqlValue> row; while ((row = cursor.next()) != null) {
                List<SqlValue> next = next(row); if (next == null) continue;
                if (current.primaryKey().length > 0) { PersistentTree.Entry entry = current.rows().getEntry(Engine.key(current, next, 0, database.environment())); if (entry == null) continue; next = Encoding.row(entry, current.types()); }
                child(current, next, snapshot, meter);
            } }
        }
        private boolean referenced(Engine.Table child, Engine.ForeignKey key, List<SqlValue> tuple) throws IOException {
            ReverseBound bound = reverseBound(child, key, tuple);
            if (bound != null) {
                if (bound.prefix() == null) return false;
                byte[] upper = Indexes.after(bound.prefix());
                meter.charge(bound.tree().rangePageCount(bound.prefix(), true, upper, false) * Costs.PAGE_READ); meter.guard();
                try (var cursor = bound.tree().cursor(bound.prefix(), true, upper, false)) { return cursor.next() != null; }
            }
            boolean[] touched = new boolean[child.columns().size()]; for (int column : key.local()) touched[column] = true;
            try (TableScan scan = new TableScan(child, null, child.columns().stream().map(Engine.Column::name).toList(), null, new SqlValue[0], meter, touched, environment)) {
                PersistentTree.Entry row;
                while ((row = scan.next()) != null) { meter.charge(Costs.STORAGE_ROW_READ); meter.guard(); if (matches(Encoding.row(row, child.types()), key, tuple)) return true; }
            }
            return false;
        }
        private record ReverseBound(PersistentTree tree, byte[] prefix, Engine.Index index) {}
        private ReverseBound reverseBound(Engine.Table child, Engine.ForeignKey key, List<SqlValue> tuple) {
            if (!CollationCompatibility.table(child, meter)) return null;
            int width = key.local().size();
            int[] local = key.local().stream().mapToInt(Integer::intValue).sorted().toArray();
            if (child.primaryKey().length >= width && Arrays.equals(Arrays.stream(child.primaryKey()).limit(width).sorted().toArray(), local)) {
                Encoding.Sink prefix = new Encoding.Sink();
                for (int i = 0; i < width; i++) {
                    int column = child.primaryKey()[i]; SqlValue value = equalAs(tuple.get(key.local().indexOf(column)), child.columns().get(column).type());
                    if (value == null) return new ReverseBound(child.rows(), null, null); prefix.raw(IndexCollations.key(value, child.columns().get(column).collation(), database.environment()));
                }
                return new ReverseBound(child.rows(), prefix.bytes(), null);
            }
            for (Engine.Index index : child.indexes()) {
                if (index.kind() != 0 || index.predicate() != null || index.keys().size() < width) continue;
                if (!Arrays.equals(index.keys().stream().limit(width).mapToInt(Engine.IndexKey::column).sorted().toArray(), local)) continue;
                if (index.keys().stream().skip(width).anyMatch(part -> part.column() < 0 || !fixed(part.type()))) continue;
                Encoding.Sink prefix = new Encoding.Sink();
                for (int i = 0; i < width; i++) {
                    Engine.IndexKey part = index.keys().get(i); SqlValue value = equalAs(tuple.get(key.local().indexOf(part.column())), part.type());
                    if (value == null) return new ReverseBound(index.entries(), null, index); prefix.raw(Indexes.slot(value, part, database.environment()));
                }
                return new ReverseBound(index.entries(), prefix.bytes(), index);
            }
            return null;
        }
        private boolean fixed(SqlType type) {
            return switch (type.kind()) { case I16, I32, I64, F32, F64, BOOLEAN, UUID, DATE, TIMESTAMP, TIMESTAMPTZ, INTERVAL -> true; default -> false; };
        }
        private final class ActionScan implements AutoCloseable {
            final Engine.Table child;
            TableScan full;
            ExternalSorter keys;
            RowSpool.Cursor cursor;
            byte[] previous;
            ActionScan(Engine.Table child, Engine.ForeignKey key, boolean[] touched) throws IOException {
                this.child = child;
                List<SqlType> tupleTypes = key.referenced().stream().map(i -> original.columns().get(i).type()).toList();
                try (ExternalSorter tuples = new ExternalSorter(List.of(SqlType.BYTEA), workMem, database.scratchDirectory(), meter.memory(), (a, b) -> Arrays.compareUnsigned(a.getFirst().asBytes(), b.getFirst().asBytes()))) {
                    try (var source = rows.cursor()) { List<SqlValue> row; while ((row = source.next()) != null) {
                        List<SqlValue> old = old(row), next = Batch.this.next(row); if (old == null || (next == null ? key.onDelete() : key.onUpdate()) < 2 || next != null && sameTuple(old, next, key.referenced())) continue;
                        List<SqlValue> tuple = key.referenced().stream().map(old::get).toList(); if (tuple.stream().anyMatch(SqlValue::isNull)) continue;
                        if (reverseBound(child, key, tuple) == null) { full = new TableScan(child, null, child.columns().stream().map(Engine.Column::name).toList(), null, new SqlValue[0], meter, touched, environment); return; }
                        tuples.append(List.of(SqlValue.ofBytes(Encoding.row(tuple))));
                    } }
                    keys = new ExternalSorter(List.of(SqlType.BYTEA), workMem, database.scratchDirectory(), meter.memory(), (a, b) -> Arrays.compareUnsigned(a.getFirst().asBytes(), b.getFirst().asBytes()));
                    byte[] last = null;
                    try (var source = tuples.finish()) { List<SqlValue> row; while ((row = source.next()) != null) {
                        byte[] encoded = row.getFirst().asBytes(); if (last != null && Arrays.equals(last, encoded)) continue; last = encoded;
                        ReverseBound bound = reverseBound(child, key, Encoding.row(encoded, tupleTypes)); if (bound.prefix() == null) continue;
                        byte[] upper = Indexes.after(bound.prefix());
                        meter.charge(bound.tree().rangePageCount(bound.prefix(), true, upper, false) * Costs.PAGE_READ); meter.guard();
                        try (var matches = bound.tree().cursor(bound.prefix(), true, upper, false)) { PersistentTree.Entry entry; while ((entry = matches.next()) != null) {
                            byte[] storageKey = entry.key();
                            if (bound.index() != null) {
                                int offset = bound.prefix().length;
                                for (int i = key.local().size(); i < bound.index().keys().size(); i++) {
                                    Engine.IndexKey part = bound.index().keys().get(i); byte nullTag = Indexes.slot(SqlValue.nullValue(part.type()), part, database.environment())[0];
                                    if (storageKey[offset++] != nullTag) offset += Encoding.fixedWidth(part.type());
                                }
                                storageKey = Arrays.copyOfRange(storageKey, offset, storageKey.length);
                                meter.charge(child.rows().pointPageCount(storageKey) * Costs.PAGE_READ); meter.guard(); entry = child.rows().getEntry(storageKey);
                                if (entry == null) throw new SqlException("XX001", "index refers to missing row");
                            }
                            StoredRow.ReadCost cost = Encoding.readCost(entry, child.types(), touched, child.rows().layout());
                            meter.charge(cost.pages() * Costs.PAGE_READ + cost.decompressions() * Costs.VALUE_DECOMPRESS); meter.guard();
                            keys.append(List.of(SqlValue.ofBytes(storageKey)));
                        } }
                    } }
                    cursor = keys.finish();
                } catch (IOException | RuntimeException failure) { close(); throw failure; }
            }
            PersistentTree.Entry next() throws IOException {
                if (full != null) return full.next();
                List<SqlValue> row;
                while ((row = cursor.next()) != null) {
                    byte[] key = row.getFirst().asBytes(); if (previous != null && Arrays.equals(previous, key)) continue; previous = key;
                    return child.rows().getEntry(key);
                }
                return null;
            }
            @Override public void close() { if (full != null) full.close(); if (cursor != null) cursor.close(); if (keys != null) keys.close(); }
        }
        private boolean matches(List<SqlValue> row, Engine.ForeignKey key, List<SqlValue> tuple) {
            for (int i = 0; i < tuple.size(); i++) {
                SqlValue value = equalAs(row.get(key.local().get(i)), tuple.get(i).type());
                if (value == null || !Arrays.equals(Encoding.key(value), Encoding.key(tuple.get(i)))) return false;
            }
            return true;
        }
        private byte[] tupleKey(List<SqlValue> row, List<Integer> columns, List<SqlType> types) {
            Encoding.Sink encoded = new Encoding.Sink();
            for (int i = 0; i < columns.size(); i++) { SqlValue value = equalAs(row.get(columns.get(i)), types.get(i)); if (value == null) return null; encoded.raw(Encoding.key(value)); }
            return encoded.bytes();
        }
        private Engine.Snapshot action(Engine.Snapshot snapshot, String childName, Engine.ForeignKey key, int depth, Closure closure) throws IOException {
            boolean needed = false;
            try (var cursor = rows.cursor()) { List<SqlValue> row; while ((row = cursor.next()) != null) {
                List<SqlValue> old = old(row), next = next(row);
                if (old != null && (next == null ? key.onDelete() : key.onUpdate()) >= 2 && (next == null || !sameTuple(old, next, key.referenced()))
                    && key.referenced().stream().noneMatch(column -> old.get(column).isNull())) { needed = true; break; }
            } }
            if (!needed) return snapshot;
            Engine.Table child = snapshot.tables().get(childName); boolean[] touched = new boolean[child.columns().size()], assigned = new boolean[child.columns().size()];
            for (int column : key.local()) touched[column] = assigned[column] = true;
            PersistentTree tree = child.rows(), replacements = PersistentTree.empty(database.pageSize(), new TableLeafCodec(child.types(), child.primaryKey()), database.pageCodec());
            Engine.CheckEvaluator checks = new Engine.CheckEvaluator(child, environment);
            List<ScalarPlan.Expression> defaults = new ArrayList<>();
            if (key.onDelete() == 4 || key.onUpdate() == 4) for (int ordinal : key.local()) defaults.add(Engine.assignment(environment.resolver(), Engine.defaultOf(child.columns().get(ordinal)), child.columns().get(ordinal).type()));
            Batch generated = new Batch(child, database, environment, workMem, snapshot); closure.owned.add(generated);
            List<SqlType> parentTypes = key.referenced().stream().map(i -> original.columns().get(i).type()).toList();
            try (HashRows mappings = new HashRows(database.scratchDirectory(), workMem, meter.memory(), parentTypes.size() + 1);
                 IndexMutation indexes = new IndexMutation(child, environment, workMem, database.scratchDirectory())) {
                try (var cursor = rows.cursor()) { List<SqlValue> row; while ((row = cursor.next()) != null) {
                    List<SqlValue> old = old(row), next = next(row); if (old == null) continue;
                    int action = next == null ? key.onDelete() : key.onUpdate(); if (action < 2 || next != null && sameTuple(old, next, key.referenced())) continue;
                    byte[] lookup = tupleKey(old, key.referenced(), parentTypes); if (lookup == null) continue;
                    List<SqlValue> mapped = new ArrayList<>(); mapped.add(SqlValue.ofInteger(SqlType.I32, next == null && action == 2 ? 0 : action));
                    for (int i = 0; i < key.referenced().size(); i++) mapped.add(next == null ? SqlValue.nullValue(parentTypes.get(i)) : next.get(key.referenced().get(i)));
                    mappings.append(lookup, mapped);
                } }
                mappings.freeze();
                try (ActionScan scan = new ActionScan(child, key, touched)) {
                PersistentTree.Entry entry;
                while ((entry = scan.next()) != null) {
                    meter.charge(Costs.STORAGE_ROW_READ); meter.guard(); List<SqlValue> values = Encoding.row(entry, child.types());
                    List<SqlValue> selected = null; int action = -1;
                    byte[] lookup = tupleKey(values, key.local(), parentTypes);
                    if (lookup != null) try (var cursor = mappings.probe(lookup)) { HashRows.Entry found; while ((found = cursor.next()) != null) {
                        if (Arrays.equals(lookup, found.key())) { action = (int) found.row().getFirst().asInteger(); selected = found.row().subList(1, found.row().size()); break; }
                    } }
                    if (action < 0) continue;
                    List<SqlValue> replacement = null;
                    if (action != 0) {
                        replacement = new ArrayList<>(values);
                        for (int i = 0; i < key.local().size(); i++) {
                            int ordinal = key.local().get(i); SqlType type = child.columns().get(ordinal).type();
                            SqlValue value = action == 2 ? cascadeValue(selected.get(i), type) : action == 3 ? SqlValue.nullValue(type) : defaults.get(i).run(meter);
                            replacement.set(ordinal, value);
                        }
                        Engine.validate(child, child.rows(), replacement, entry.key(), checks, meter);
                    }
                    byte[] newKey = replacement == null ? null : child.primaryKey().length == 0 ? entry.key() : Engine.key(child, replacement, 0, database.environment());
                    indexes.add(values, entry.key(), replacement, newKey); generated.record(values, replacement); tree = tree.remove(entry.key());
                    if (replacement != null) {
                        if (replacements.getEntry(newKey) != null) throw new SqlException("23505", "duplicate primary key during cascade");
                        replacements = replacements.put(Encoding.entry(newKey, replacement, entry, assigned, database.pageLayout(), meter, child.primaryKey()));
                    }
                }
                if (generated.rows == null || generated.rows.size() == 0) return snapshot;
                try (var cursor = replacements.cursor()) { while ((entry = cursor.next()) != null) { if (tree.getEntry(entry.key()) != null) throw new SqlException("23505", "duplicate primary key during cascade"); tree = tree.put(entry); } }
                snapshot = snapshot.put(indexes.checkExclusions(indexes.apply(child.changed(tree, child.nextRowId()))));
                return generated.apply(snapshot, depth + 1, closure);
                }
            }
        }
        private boolean sameTuple(List<SqlValue> a, List<SqlValue> b, List<Integer> columns) {
            for (int column : columns) { SqlValue left = a.get(column), right = b.get(column); if (left.isNull() != right.isNull() || !left.isNull() && !Arrays.equals(Encoding.key(left), Encoding.key(right))) return false; }
            return true;
        }
        private SqlValue cascadeValue(SqlValue value, SqlType target) {
            // Referential actions admit the declared cross-type FK conversion, while text
            // typmods retain ordinary assignment's rejection of non-space truncation.
            return target.kind() == SqlType.Kind.TEXT
                ? Engine.assignment(environment.resolver(), new SqlParser.StoredValue(value), target).run(meter)
                : SqlCasts.cast(value, target, meter);
        }
        @Override public void close() { if (!retained && rows != null) rows.close(); }
    }
}
