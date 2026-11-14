package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeMap;
import org.jed.storage.PersistentTree;

/** Explicit atomic collation adoption stages replacement trees before advancing any file pin. */
final class CollationMaintenance {
    record Change(Engine.Snapshot snapshot, int upgraded) {}
    private CollationMaintenance() {}
    static Change setDefault(Engine.Snapshot snapshot, String name, Environment environment) {
        Collation collation = environment.collation(name);
        if (CollationCompatibility.defaultName(snapshot).equals(name)) return new Change(snapshot, 0);
        snapshot = snapshot.useCollation(collation);
        var references = new TreeMap<String, Engine.CollationReference>();
        for (Engine.CollationReference reference : snapshot.collations().values()) references.put(reference.name(), new Engine.CollationReference(reference.name(), reference.unicodeVersion(), reference.cldrVersion(), reference.description(), reference.name().equals(name)));
        return new Change(new Engine.Snapshot(snapshot.tables(), snapshot.types(), snapshot.views(), snapshot.sequences(), snapshot.version(), references), 0);
    }
    static Change upgrade(Engine.Snapshot snapshot, Database database, Session session, CostMeter meter) throws IOException {
        var references = new TreeMap<>(snapshot.collations()); int upgraded = 0;
        for (Engine.CollationReference reference : snapshot.collations().values()) {
            if (CollationCompatibility.matches(reference.name(), snapshot.collations(), database.environment())) continue;
            Collation loaded = database.environment().collation(reference.name());
            references.put(reference.name(), new Engine.CollationReference(reference.name(), loaded.unicodeVersion(), loaded.cldrVersion(), reference.description(), reference.isDefault())); upgraded++;
        }
        if (upgraded == 0) return new Change(snapshot, 0);
        Engine.Snapshot next = snapshot;
        try (QueryEnvironment environment = new QueryEnvironment((query, scope) -> { throw new SqlException("0A000", "subquery in stored index expression"); }, new SqlValue[0], meter)) {
            environment.namedTypes(snapshot.types());
            for (Engine.Table original : snapshot.tables().values()) {
                boolean primary = false;
                for (int column : original.primaryKey()) if (!CollationCompatibility.matches(original.columns().get(column).collation(), snapshot.collations(), database.environment())) primary = true;
                Engine.Table table = original;
                if (primary) {
                    PersistentTree rows = PersistentTree.empty(database.pageSize(), new TableLeafCodec(table.types(), table.primaryKey()), database.pageCodec());
                    try (var cursor = original.rows().cursor()) {
                        PersistentTree.Entry entry;
                        while ((entry = cursor.next()) != null) {
                            meter.guard(); List<SqlValue> values = Encoding.row(entry, original.types());
                            byte[] key = Engine.key(original, values, 0, database.environment());
                            if (rows.getEntry(key) != null) throw new SqlException("23505", "collation upgrade produced duplicate primary key");
                            rows = rows.put(Encoding.entry(key, values, entry, new boolean[values.size()], database.pageLayout(), meter, original.primaryKey()));
                        }
                    }
                    table = table.changed(rows, table.nextRowId());
                }
                List<Engine.Index> indexes = new ArrayList<>(); boolean changed = primary;
                for (Engine.Index index : table.indexes()) {
                    if (primary || !CollationCompatibility.index(index, meter)) { index = Indexes.build(index, table, new SqlValue[0], environment, database, session.workMem()); changed = true; }
                    indexes.add(index);
                }
                var retained = new TreeMap<>(original.statistics());
                retained.entrySet().removeIf(entry -> !CollationCompatibility.matches(original.columns().get(entry.getKey()).collation(), snapshot.collations(), database.environment()));
                if (changed || retained.size() != original.statistics().size()) next = next.put(table.withIndexes(indexes).withStatistics(retained));
            }
        }
        return new Change(new Engine.Snapshot(next.tables(), next.types(), next.views(), next.sequences(), next.version(), references), upgraded);
    }
}
