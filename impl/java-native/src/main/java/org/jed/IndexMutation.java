package org.jed;

import java.io.IOException;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import org.jed.storage.PersistentTree;

/** Stage keys and validate final uniqueness before applying the statement's canonical edit order. */
final class IndexMutation implements AutoCloseable {
    private final List<Indexes.Evaluator> evaluators = new ArrayList<>();
    private final List<GinIndex.Changes> gin = new ArrayList<>();
    private final List<RowSpool> changes = new ArrayList<>();
    private final CostMeter meter;
    private final Engine.Table original;
    private final QueryEnvironment environment;
    private final long workMem;
    private final Path scratch;
    private List<PersistentTree> insertTrees;
    private IndexMutation additions;
    private boolean pairedUpdates;
    IndexMutation update(boolean primaryKeyAssigned) { pairedUpdates = !primaryKeyAssigned; return this; }
    IndexMutation upsert() { pairedUpdates = true; additions = new IndexMutation(original, environment, workMem, scratch); return this; }
    IndexMutation insertOnly() { insertTrees = new ArrayList<>(); for (Engine.Index index : original.indexes()) insertTrees.add(index.unique() ? index.entries() : null); return this; }
    IndexMutation(Engine.Table table, QueryEnvironment environment, long workMem, Path scratch) {
        meter = environment.meter(); original = table; this.environment = environment; this.workMem = workMem; this.scratch = scratch;
        CollationCompatibility.require(table, meter);
        try {
            for (Engine.Index index : table.indexes()) {
                IndexDependencies.require(index, meter.environment());
                if (index.kind() == 1) {
                    gin.add(new GinIndex.Changes(index, table, environment, workMem, scratch)); evaluators.add(null); changes.add(null); continue;
                }
                gin.add(null); evaluators.add(new Indexes.Evaluator(index, table, environment));
                changes.add(new RowSpool(List.of(SqlType.BYTEA, SqlType.BYTEA, SqlType.BYTEA, SqlType.BYTEA), workMem, scratch, meter.memory()));
            }
        } catch (RuntimeException failure) { close(); throw failure; }
    }
    void add(List<SqlValue> oldRow, byte[] oldKey, List<SqlValue> newRow, byte[] newKey) throws IOException {
        if (additions != null) {
            if (oldRow == null) { additions.add(null, null, newRow, newKey); return; }
            if (!Arrays.equals(oldKey, newKey)) pairedUpdates = false;
        }
        for (int i = 0; i < evaluators.size(); i++) {
            if (gin.get(i) != null) { gin.get(i).add(oldRow, oldKey, newRow, newKey); continue; }
            Indexes.Key oldValue = oldRow == null ? null : evaluators.get(i).key(oldRow, oldKey, meter);
            Indexes.Key newValue = newRow == null ? null : evaluators.get(i).key(newRow, newKey, meter);
            if (insertTrees != null && insertTrees.get(i) != null && newValue != null) insertTrees.set(i, Indexes.insert(insertTrees.get(i), newValue));
            if (oldValue == null && newValue == null || oldValue != null && newValue != null && Arrays.equals(oldValue.entry(), newValue.entry())) continue;
            changes.get(i).append(List.of(bytes(oldValue == null ? null : oldValue.entry()), bytes(newValue == null ? null : newValue.entry()), bytes(newValue == null ? null : newValue.uniquePrefix()), bytes(newValue == null || Exclusions.forIndex(original, original.indexes().get(i).name()) != null && !Exclusions.participates(Exclusions.forIndex(original, original.indexes().get(i).name()), newRow) ? null : newKey)));
        }
    }
    Engine.Table apply(Engine.Table table) throws IOException {
        List<Engine.Index> indexes = new ArrayList<>();
        for (int i = 0; i < evaluators.size(); i++) {
            Engine.Index index = table.indexes().get(i); PersistentTree tree = index.entries();
            if (gin.get(i) != null) {
                if (additions == null) tree = gin.get(i).apply(tree, pairedUpdates);
                else if (pairedUpdates) tree = gin.get(i).apply(additions.gin.get(i).insert(tree), true);
                else tree = gin.get(i).insert(additions.gin.get(i).insert(gin.get(i).remove(tree)));
                indexes.add(index.changed(tree)); continue;
            }
            if (pairedUpdates && !index.unique()) {
                if (additions != null) tree = additions.insert(tree, i, false);
                tree = applyPaired(tree, i);
                indexes.add(index.changed(tree)); continue;
            }
            try (var cursor = changes.get(i).cursor()) { List<SqlValue> row; while ((row = cursor.next()) != null) if (!row.getFirst().isNull()) tree = tree.remove(row.getFirst().asBytes()); }
            if (additions != null) tree = additions.insert(tree, i, true);
            tree = insert(tree, i, true);
            // Uniqueness is an end-state rule, even though ordinary UPDATE maintains
            // physical entries in row arrival order (indexes.md §4).
            if (pairedUpdates) tree = applyPaired(additions == null ? index.entries() : additions.insert(index.entries(), i, false), i);
            index = index.changed(tree);
            indexes.add(index);
        }
        return table.withIndexes(indexes);
    }
    private PersistentTree insert(PersistentTree tree, int index, boolean validate) throws IOException {
        try (var cursor = changes.get(index).cursor()) {
            List<SqlValue> row;
            while ((row = cursor.next()) != null) if (!row.get(1).isNull())
                tree = validate ? Indexes.insert(tree, new Indexes.Key(row.get(1).asBytes(), row.get(2).isNull() ? null : row.get(2).asBytes())) : tree.put(row.get(1).asBytes(), new byte[0]);
        }
        return tree;
    }
    private PersistentTree applyPaired(PersistentTree tree, int index) throws IOException {
        try (var cursor = changes.get(index).cursor()) {
            List<SqlValue> row;
            while ((row = cursor.next()) != null) {
                if (!row.getFirst().isNull()) tree = tree.remove(row.getFirst().asBytes());
                if (!row.get(1).isNull()) tree = tree.put(row.get(1).asBytes(), new byte[0]);
            }
        }
        return tree;
    }
    Engine.Table checkExclusions(Engine.Table table) throws IOException {
        if (additions != null) additions.checkExclusions(table);
        for (int i = 0; i < table.indexes().size(); i++) {
            Engine.Index index = table.indexes().get(i);
            if (Exclusions.forIndex(table, index.name()) != null) try (var cursor = changes.get(i).cursor()) {
                List<SqlValue> row; while ((row = cursor.next()) != null) if (!row.get(1).isNull() && !row.get(3).isNull()) Exclusions.check(table, index, row.get(1).asBytes(), row.get(3).asBytes());
            }
        }
        return table;
    }
    private static SqlValue bytes(byte[] value) { return value == null ? SqlValue.nullValue(SqlType.BYTEA) : SqlValue.ofBytes(value); }
    public void close() { if (additions != null) additions.close(); for (RowSpool change : changes) if (change != null) change.close(); for (GinIndex.Changes change : gin) if (change != null) change.close(); }
}
