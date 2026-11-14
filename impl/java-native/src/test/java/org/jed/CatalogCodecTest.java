package org.jed;

import java.nio.file.Path;
import java.util.List;
import org.jed.storage.*;

/** Primitive catalog entries and B+tree reads against shared canonical images. */
public final class CatalogCodecTest {
    public static void main(String[] args) throws Exception {
        Path fixtures = Path.of(args.length == 0 ? "spec/fileformat/fixtures" : args[0]);
        List<String> names = List.of("unparsable_definitions", "statistics_table", "statistics_terms_table", "statistics_decimal_table", "hostfunc_index_table", "hostfunc_check_table", "gist_exclude_table", "exclude_followon_table", "gist_range_table", "gist_scalar_table", "fk_table", "collation_table", "collation_pk_table", "collation_array_table", "timezone_index_table", "gin_array_table", "gin_float_table", "gin_uuid_table", "gin_text_table", "gin_interval_table", "gin_decimal_bytea_table", "gin_jsonb_table", "gin_multi_table", "index_table", "unique_table", "expr_index_table", "partial_index_table", "ordered_index_table", "identity_table", "sequence_table", "serial_table", "view_table", "view_options_table", "check_table", "default_table", "default_expr_table", "pk_table", "nopk_table", "one_table_empty", "bool_table", "bool_pk_table",
            "text_table", "text_pk_table", "bytea_table", "bytea_pk_table", "decimal_table", "decimal_pk_table", "decimal_fixed_table",
            "float32_table", "float32_pk_table", "float64_table", "float64_pk_table", "composite_pk_table", "varchar_table", "uuid_table", "date_table", "timestamp_table", "timestamptz_table", "interval_table", "interval_pk_table", "array_table", "array_pk_table",
            "composite_type_table", "composite_key_table", "array_composite_table", "composite_array_field_table", "nested_composite_table", "temporal_typmod_table", "json_table", "jsonb_table", "range_table", "range_pk_table", "composite_range_field_table");
        for (String name : names) {
            try (var store = FileBlockStore.open(fixtures.resolve(name + ".jed"), false)) {
                var selected = PlaintextRecovery.recover(store);
                try (var pager = new Pager(store, selected.meta().pageSize(), 512, false)) {
                    var snapshot = CatalogCodec.read(pager, selected.meta());
                    var entries = new java.util.ArrayList<byte[]>();
                    for (var type : CatalogCodec.orderedTypes(snapshot)) entries.add(CatalogCodec.definition(type));
                    for (var sequence : CatalogCodec.orderedSequences(snapshot)) entries.add(CatalogCodec.sequence(sequence));
                    for (var collation : CatalogCodec.orderedCollations(snapshot)) entries.add(CatalogCodec.collation(collation));
                    for (var table : CatalogCodec.ordered(snapshot)) entries.add(CatalogCodec.table(table, table.rows().rootPage()));
                    entries.addAll(CatalogCodec.statistics(snapshot));
                    for (var view : CatalogCodec.orderedViews(snapshot)) entries.add(CatalogCodec.view(view));
                    var planned = CatalogChain.prepare(pager.pageSize(), entries, new PageAllocator(selected.meta().pageCount(), new long[0]),
                        selected.meta().txid() + 1, pager, selected.meta().root(), selected.meta().pageCount());
                    check(planned.writes().isEmpty(), name + " catalog bytes differ");
                    for (var table : snapshot.tables().values()) {
                        long count = 0;
                        try (var cursor = table.rows().cursor()) {
                            PersistentTree.Entry entry;
                            while ((entry = cursor.next()) != null) { var row = Encoding.row(entry, table.types()); for (var value : row) { /* Materialize each requested field. */ } count++; }
                        }
                        check(count == table.rows().size(), name + " persisted row count differs");
                        for (Engine.Index index : table.indexes()) {
                            if (index.kind() == 1) checkGin(table, index);
                            if (index.kind() == 2) {
                                var rebuilt = GistTree.build(index.keys().stream().map(Engine.IndexKey::type).toList(), index.entries(), null);
                                var plan = rebuilt.prepare(new PageAllocator(selected.meta().pageCount(), index.physicalPages().stream().mapToLong(Long::longValue).sorted().toArray()), selected.meta().txid(), pager.layout(), pager.codec());
                                check(plan.root().equals(index.rootPointer()), name + " canonical GiST root differs");
                                for (var write : plan.writes()) check(java.util.Arrays.equals(write.bytes(), pager.readRaw(write.page())), name + " GiST node bytes differ");
                            }
                            try (var cursor = index.entries().cursor()) { while (cursor.next() != null) { } }
                            for (long page : index.entries().physicalPages()) {
                                { var lease = pager.readBody(page);
                                    if (lease.header().type() != 2) continue;
                                    var payload = lease.buffer().slice(16, pager.layout().capacity());
                                    var codec = TableLeafCodec.index(); var rows = codec.decode(payload, (int) lease.header().count());
                                    byte[] encoded = codec.encode(rows), actual = new byte[encoded.length]; payload.get(actual);
                                    check(java.util.Arrays.equals(encoded, actual), name + " index leaf encoding differs");
                                }
                            }
                        }
                    }
                }
            }
        }
        invalidDefinitions();
        System.out.println("CatalogCodecTest: passed " + names.size() + " shared catalogs and trees; dangling and cyclic type references rejected");
    }
    private static void checkGin(Engine.Table table, Engine.Index index) throws Exception {
        var postings = new java.util.TreeMap<byte[], java.util.List<byte[]>>(java.util.Arrays::compareUnsigned);
        check(index.predicate() == null, "GIN fixture predicate needs explicit test evaluator");
        try (var cursor = table.rows().cursor()) {
            PersistentTree.Entry entry;
            while ((entry = cursor.next()) != null) {
                var row = Encoding.row(entry, table.types());
                for (int i = 0; i < index.keys().size(); i++) for (byte[] term : GinIndex.terms(row.get(index.keys().get(i).column()))) {
                    if (index.keys().size() > 1) { byte[] tagged = new byte[term.length + 1]; tagged[0] = (byte)i; System.arraycopy(term, 0, tagged, 1, term.length); term = tagged; }
                    postings.computeIfAbsent(term, ignored -> new java.util.ArrayList<>()).add(entry.key());
                }
            }
        }
        long expectedCount = 0;
        for (var posting : postings.entrySet()) {
            try (var cursor = new org.jed.storage.GinPostingRuns.Cursor(index.entries(), posting.getKey())) {
                for (byte[] key : posting.getValue()) { check(java.util.Arrays.equals(cursor.next(), key), "GIN extracted term differs from canonical posting"); expectedCount++; }
                check(cursor.next() == null, "GIN posting has unexpected key");
            }
        }
        long actualCount = 0;
        try (var cursor = index.entries().cursor()) {
            PersistentTree.Entry entry;
            while ((entry = cursor.next()) != null) {
                var term = postings.floorEntry(entry.key()); check(term != null, "GIN run has unknown term");
                var keys = org.jed.storage.GinPostingRuns.decode(entry.key(), term.getKey()); actualCount += keys.size();
                check(java.util.Arrays.equals(org.jed.storage.GinPostingRuns.encode(term.getKey(), keys), entry.key()), "GIN run re-encoding differs");
            }
        }
        check(expectedCount == actualCount, "GIN posting count differs");
    }
    private static void invalidDefinitions() throws Exception {
        SqlType a = SqlType.composite("a", List.of()), b = SqlType.composite("b", List.of());
        for (var definitions : List.of(
                List.of(SqlType.composite("a", List.of(new SqlType.Field("value", b)))),
                List.of(SqlType.composite("a", List.of(new SqlType.Field("value", a)))),
                List.of(SqlType.composite("a", List.of(new SqlType.Field("values", SqlType.array(b)))), SqlType.composite("b", List.of(new SqlType.Field("parent", a)))))) {
            var catalog = CatalogChain.prepare(256, definitions.stream().map(CatalogCodec::definition).toList(), new PageAllocator(2, new long[0]), 1, null, 0, 2);
            try (var store = new MemoryBlockStore(256)) {
                for (var write : catalog.writes()) store.writeAt(write.page() * 256, write.bytes());
                byte[] encoded = ManifestEncoder.encode(256, 1, catalog.root(), catalog.allocator().highWater(), 0, catalog.writes().size(), List.of(), new long[0], List.of()).meta();
                try (var pager = new Pager(store, 256, 256, false)) {
                    try { CatalogCodec.read(pager, PlaintextRecovery.parseMeta(encoded)); throw new AssertionError("invalid type graph accepted"); }
                    catch (SqlException failure) { check(failure.sqlState().equals("XX001"), "type graph error classification"); }
                }
            }
        }
    }
    private static void check(boolean value, String message) { if (!value) throw new AssertionError(message); }
}
