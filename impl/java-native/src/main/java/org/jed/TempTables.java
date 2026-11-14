package org.jed;

import java.util.ArrayList;
import java.util.HashSet;
import java.util.Set;
import org.jed.storage.PersistentTree;

/** Unmetered transactional ON COMMIT actions over the private temporary domain. */
final class TempTables {
    private TempTables() {}
    static Engine.Snapshot applyCommit(Engine.Snapshot roots) {
        Engine.Snapshot temp = roots.domains().get("temp");
        if (temp == null || temp.tempActions().isEmpty()) return roots;
        Set<String> truncate = new HashSet<>(), drop = new HashSet<>();
        for (var action : temp.tempActions().entrySet()) {
            if (action.getValue().equals("delete")) truncate.add(action.getKey());
            if (action.getValue().equals("drop")) drop.add(action.getKey());
        }
        for (var table : temp.tables().values()) if (!truncate.contains(table.name()))
            for (var key : table.foreignKeys()) if (truncate.contains(key.parent()))
                throw new SqlException("0A000", "unsupported ON COMMIT and foreign key combination");
        for (String name : truncate) {
            var table = temp.tables().get(name); if (table == null || table.rows().size() == 0 && table.indexes().stream().allMatch(index -> index.entries().size() == 0)) continue;
            var rows = PersistentTree.empty(table.rows().pageSize(), new TableLeafCodec(table.types(), table.primaryKey()));
            var indexes = new ArrayList<Engine.Index>();
            for (var index : table.indexes()) indexes.add(index.changed(PersistentTree.empty(table.rows().pageSize(), TableLeafCodec.index())));
            temp = temp.put(table.changed(rows, table.nextRowId()).withIndexes(indexes));
        }
        Set<String> sequences = new HashSet<>();
        for (var sequence : temp.sequences().values()) if (drop.contains(sequence.owner())) sequences.add(sequence.name());
        Set<String> views = new HashSet<>(); boolean more;
        do {
            more = false;
            for (var view : temp.views().values()) if (!views.contains(view.name()) && view.dependencies().stream().anyMatch(dependency ->
                dependency.kind() == 0 && drop.contains(dependency.name()) || dependency.kind() == 1 && views.contains(dependency.name())
                    || dependency.kind() == 2 && sequences.contains(dependency.name()))) { views.add(view.name()); more = true; }
        } while (more);
        for (String view : views) temp = temp.dropView(view);
        for (String sequence : sequences) temp = temp.dropSequence(sequence);
        for (String name : drop) temp = temp.drop(name);
        for (var table : new ArrayList<>(temp.tables().values())) {
            var keys = table.foreignKeys().stream().filter(key -> !drop.contains(key.parent())).toList();
            if (keys.size() != table.foreignKeys().size()) temp = temp.put(table.withForeignKeys(keys));
        }
        return roots.withDomain("temp", temp);
    }
}
