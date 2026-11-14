package org.jed;

import java.util.ArrayList;
import java.util.TreeMap;

/** Pending balances belong to immutable working snapshots, excluding the temp domain. */
final class PendingMemory {
    private PendingMemory() {}
    static long bytes(Engine.Snapshot snapshot) {
        long result = localBytes(snapshot.withoutDomains());
        for (var domain : snapshot.domains().entrySet()) if (!domain.getKey().equals("main") && !domain.getKey().equals("temp")) result = MemoryAccount.add(result, localBytes(domain.getValue()));
        return result;
    }
    private static long localBytes(Engine.Snapshot snapshot) {
        long result = 0;
        for (Engine.Table table : snapshot.tables().values()) {
            result = MemoryAccount.add(result, table.rows().pendingBytes());
            for (Engine.Index index : table.indexes()) result = MemoryAccount.add(result, index.entries().pendingBytes());
        }
        return result;
    }
    static Engine.Snapshot forget(Engine.Snapshot snapshot) {
        Engine.Snapshot local = forgetLocal(snapshot.withoutDomains());
        if (snapshot.domains().isEmpty()) return local;
        var domains = new TreeMap<String,Engine.Snapshot>();
        for (var entry : snapshot.domains().entrySet()) domains.put(entry.getKey(), entry.getKey().equals("temp") ? entry.getValue() : entry.getKey().equals("main") ? local : forgetLocal(entry.getValue()));
        return local.withDomains(domains);
    }
    private static Engine.Snapshot forgetLocal(Engine.Snapshot snapshot) {
        var tables = new TreeMap<String,Engine.Table>(); boolean changed = false;
        for (Engine.Table table : snapshot.tables().values()) {
            var rows = table.rows().forgetPendingKeys(); var indexes = new ArrayList<Engine.Index>(); boolean tableChanged = rows != table.rows();
            for (Engine.Index index : table.indexes()) {
                var entries = index.entries().forgetPendingKeys(); tableChanged |= entries != index.entries();
                indexes.add(entries == index.entries() ? index : new Engine.Index(index.name(), index.keys(), index.unique(), index.constraint(), index.predicate(), index.kind(), entries, index.timezoneDependencies(), index.gist(), index.hostDependencies()));
            }
            tables.put(table.name(), tableChanged ? new Engine.Table(table.name(), table.columns(), table.primaryKey(), table.unique(), rows, table.nextRowId(), table.checks(), indexes, table.foreignKeys(), table.exclusions(), table.statistics()) : table);
            changed |= tableChanged;
        }
        return changed ? new Engine.Snapshot(tables, snapshot.types(), snapshot.views(), snapshot.sequences(), snapshot.version(), snapshot.collations(), snapshot.domains(), snapshot.tempActions()) : snapshot;
    }
}
