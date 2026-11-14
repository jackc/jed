package org.jed;

import java.io.IOException;
import java.util.*;
import org.jed.storage.*;

/** Walks the committed in-memory roots and independently reopened page image. */
final class SimulationStorageChecks {
    record Facts(long version, long live, long pages, int free, Set<Long> reachable) { }
    static void require(boolean valid, String detail) { if (!valid) throw new AssertionError("simulation invariant: " + detail); }
    static Facts live(Database database) throws IOException {
        try (var lease = database.storageLeaseForTesting()) {
            Facts facts = inspect(lease.snapshot(), lease.pager(), lease.selected());
            try (Session session = database.session(); QueryEnvironment environment = new QueryEnvironment(null, new SqlValue[0], new CostMeter(session, new Cancellation()))) {
                for (var table : lease.snapshot().tables().values()) {
                    List<List<SqlValue>> rows = new ArrayList<>(); List<byte[]> rowKeys = new ArrayList<>();
                    try (var cursor = table.rows().cursor()) {
                        for (PersistentTree.Entry entry; (entry = cursor.next()) != null;) {
                            List<SqlValue> values = List.copyOf(Encoding.row(entry, table.types()));
                            if (table.primaryKey().length != 0) require(Arrays.equals(entry.key(), Engine.key(table, values, 0, database.environment())), "primary key differs from row values");
                            rows.add(values); rowKeys.add(entry.key());
                        }
                    }
                    for (var index : table.indexes()) if (index.kind() == 0) {
                        var evaluator = new Indexes.Evaluator(index, table, environment); List<byte[]> expected = new ArrayList<>();
                        for (int i = 0; i < rows.size(); i++) { var value = evaluator.key(rows.get(i), rowKeys.get(i), environment.meter()); if (value != null) expected.add(value.entry()); }
                        expected.sort(Arrays::compareUnsigned); int at = 0;
                        try (var cursor = index.entries().cursor()) { for (PersistentTree.Entry entry; (entry = cursor.next()) != null;) require(at < expected.size() && Arrays.equals(expected.get(at++), entry.key()), "secondary index differs from independently derived row keys"); }
                        require(at == expected.size(), "secondary index entry count differs from table derivation");
                    }
                }
            }
            return facts;
        }
    }
    static Facts inspect(Engine.Snapshot snapshot, Pager pager, PlaintextRecovery.Selected selected) throws IOException {
        var meta = selected.meta(); var free = FreeList.read(pager, selected); Set<Long> pages = new HashSet<>();
        try (var cursor = CatalogChain.cursor(pager, meta.root(), meta.pageCount(), meta.rootTag())) { while (cursor.next() != null) { } add(pages, cursor.visitedPages(), meta, free); }
        for (var table : snapshot.tables().values()) {
            add(pages, table.rows().physicalPages(), meta, free); long rows = 0; byte[] previous = null;
            try (var cursor = table.rows().cursor()) {
                for (PersistentTree.Entry entry; (entry = cursor.next()) != null;) {
                    require(previous == null || Arrays.compareUnsigned(previous, entry.key()) < 0, "table key order"); previous = entry.key(); rows++;
                    // Demand every cell, including overflow. A corrupt untouched cell must fail this oracle.
                    var values = Encoding.row(entry, table.columns().stream().map(Engine.Column::type).toList()); for (int i = 0; i < values.size(); i++) values.get(i);
                }
            }
            require(rows == table.rows().size(), "catalog count differs from leaf total");
            for (var index : table.indexes()) {
                add(pages, index.physicalPages(), meta, free);
                if (index.kind() != 2) try (var cursor = index.entries().cursor()) { previous = null; for (PersistentTree.Entry entry; (entry = cursor.next()) != null;) { require(previous == null || Arrays.compareUnsigned(previous, entry.key()) < 0, "index key order"); previous = entry.key(); } }
            }
        }
        require(pages.size() == meta.livePages(), "meta live-page count differs from reachability");
        require(Collections.disjoint(pages, selected.protectedPages().stream().filter(free::contains).toList()), "free-list overlaps protected generation");
        return new Facts(meta.txid(), meta.livePages(), meta.pageCount(), free.size(), Set.copyOf(pages));
    }
    private static void add(Set<Long> pages, Collection<Long> added, PlaintextRecovery.Meta meta, FreeList free) {
        for (long page : added) { require(page >= meta.layout().firstBody() && page < meta.pageCount(), "live page outside high-water"); require(!free.contains(page), "live page also free"); require(pages.add(page), "duplicate live page reference"); }
    }
}
