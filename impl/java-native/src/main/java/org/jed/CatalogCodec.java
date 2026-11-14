package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import org.jed.storage.CatalogChain;
import org.jed.storage.Pager;
import org.jed.storage.PersistentTree;
import org.jed.storage.PlaintextRecovery;

/** Canonical table and named-type catalog entries. Unsupported facts fail explicitly. */
final class CatalogCodec {
    private CatalogCodec() {}

    static Engine.Snapshot read(Pager pager, PlaintextRecovery.Meta meta) throws IOException {
        Map<String, Engine.Table> tables = new TreeMap<>();
        Map<String, Engine.View> views = new TreeMap<>();
        Map<String, Engine.Sequence> sequences = new TreeMap<>();
        Map<String, Engine.CollationReference> collations = new TreeMap<>();
        Map<String, SqlType> rawTypes = new TreeMap<>(), types = null;
        StatisticsCodec.Decoder statistics = new StatisticsCodec.Decoder();
        byte[] previous = null; int previousRank = -1;
        try (var cursor = CatalogChain.cursor(pager, meta.root(), meta.pageCount(), meta.rootTag())) {
            CatalogChain.Chunk chunk;
            while ((chunk = cursor.next()) != null) {
                byte[] bytes = new byte[chunk.payload().remaining()]; chunk.payload().get(bytes);
                Encoding.Reader in = new Encoding.Reader(bytes);
                for (int i = 0; i < chunk.itemCount(); i++) {
                    int kind = in.u8();
                    if (kind > 5) throw corrupt("unknown catalog object kind");
                    int rank = kind == 0 ? 3 : kind <= 3 ? kind - 1 : kind;
                    if (rank < previousRank) throw corrupt("catalog object group order");
                    if (rank != previousRank) previous = null;
                    previousRank = rank;
                    if (kind == 4) { statistics.read(in, tables); continue; }
                    int collationFlags = kind == 3 ? in.u8() : 0;
                    if ((collationFlags & ~1) != 0) throw corrupt("invalid collation reference flags");
                    String name = name(in);
                    byte[] encodedName = SqlText.encode(name);
                    if (previous != null && Arrays.compareUnsigned(previous, encodedName) >= 0) throw corrupt("catalog object name order");
                    previous = encodedName;
                    if (kind == 3) {
                        if (name.equals("C") || collationFlags != 0 && collations.values().stream().anyMatch(Engine.CollationReference::isDefault)) throw corrupt("invalid default collation reference");
                        collations.put(name, new Engine.CollationReference(name, text(in.sized()), text(in.sized()), text(in.sized()), collationFlags != 0)); continue;
                    }
                    if (kind == 2) { sequences.put(name, readSequence(in, name)); continue; }
                    int count = in.u16();
                    if (kind == 5) { views.put(name, readView(in, name, count)); continue; }
                    if (kind == 1) { rawTypes.put(name, readDefinition(in, name, count)); continue; }
                    if (types == null) types = resolveDefinitions(rawTypes);
                    List<Engine.Column> columns = new ArrayList<>();
                    HashSet<String> names = new HashSet<>();
                    for (int col = 0; col < count; col++) {
                        String colName = name(in);
                        if (!names.add(colName)) throw corrupt("duplicate catalog column");
                        int typeCode = in.u8(), flags = in.u8();
                        if ((flags & 0x81) != 0 || (flags & 12) == 12 || (flags & 16) != 0 && (flags & 10) != 10
                            || (flags & 32) != 0 && (flags & 16) == 0) throw corrupt("invalid column flags");
                        SqlType type = resolveDescriptor(type(in, typeCode), types);
                        if (type.kind() == SqlType.Kind.COMPOSITE && (flags & ~10) != 0) throw corrupt("invalid composite column flags");
                        if ((flags & 64) != 0 && !CollationDerivation.collatable(type)) throw corrupt("collation on non-text column");
                        SqlParser.Expr value = (flags & 8) != 0 ? storedExpression(text(in.sized()), "default of column \"" + colName + "\" of table \"" + name + "\"")
                            : (flags & 4) == 0 ? null : new SqlParser.StoredValue(Encoding.read(in, type));
                        String collation = (flags & 64) == 0 ? "C" : name(in);
                        if ((flags & 64) != 0 && (collation.equals("C") || !collations.containsKey(collation))) throw corrupt("invalid column collation reference");
                        columns.add(new Engine.Column(colName, type, (flags & 2) != 0, value, (flags & 16) == 0 ? 0 : (flags & 32) == 0 ? 1 : 2, collation));
                    }
                    int[] primary = new int[in.u16()];
                    HashSet<Integer> ordinals = new HashSet<>();
                    for (int col = 0; col < primary.length; col++) {
                        int ordinal = in.u16();
                        if (ordinal >= count || !ordinals.add(ordinal) || !columns.get(ordinal).notNull()) throw corrupt("invalid primary key ordinal");
                        primary[col] = ordinal;
                    }
                    int checkCount = in.u16();
                    List<Engine.Check> checks = new ArrayList<>(); byte[] previousCheck = null;
                    for (int c = 0; c < checkCount; c++) {
                        String checkName = text(in.sized()); byte[] checkBytes = SqlText.encode(checkName);
                        if (previousCheck != null && Arrays.compareUnsigned(previousCheck, checkBytes) >= 0) throw corrupt("unordered check constraints");
                        previousCheck = checkBytes;
                        SqlParser.StoredExpression expression = storedExpression(text(in.sized()), "check constraint \"" + checkName + "\" of table \"" + name + "\"");
                        int flags = in.u8();
                        if ((flags & ~1) != 0) throw corrupt("invalid check flags");
                        checks.add(new Engine.Check(checkName, expression, flags == 0 ? List.of() : readHostDependencies(in)));
                    }
                    List<Engine.Index> indexes = readIndexes(in, name, columns, pager, meta);
                    List<Engine.ForeignKey> foreignKeys = new ArrayList<>(); byte[] previousForeignKey = null;
                    for (int f = in.u16(); f > 0; f--) {
                        String fkName = name(in); byte[] encoded = SqlText.encode(fkName);
                        if (previousForeignKey != null && Arrays.compareUnsigned(previousForeignKey, encoded) >= 0) throw corrupt("unordered foreign keys");
                        previousForeignKey = encoded;
                        List<Integer> local = new ArrayList<>(), referenced = new ArrayList<>();
                        for (int c = in.u16(); c > 0; c--) { int ordinal = in.u16(); if (ordinal >= columns.size() || local.contains(ordinal)) throw corrupt("invalid local foreign key ordinal"); local.add(ordinal); }
                        String parent = name(in);
                        for (int c = in.u16(); c > 0; c--) { int ordinal = in.u16(); if (referenced.contains(ordinal)) throw corrupt("repeated referenced foreign key ordinal"); referenced.add(ordinal); }
                        int actions = in.u8();
                        if (local.isEmpty() || local.size() != referenced.size() || (actions & 128) != 0 || (actions & 7) > 4 || ((actions >>> 3) & 7) > 4) throw corrupt("invalid foreign key metadata");
                        foreignKeys.add(new Engine.ForeignKey(fkName, local, parent, referenced, actions & 7, (actions >>> 3) & 7, (actions & 64) != 0));
                    }
                    List<Engine.Exclusion> exclusions = new ArrayList<>(); byte[] previousExclusion = null;
                    for (int e = in.u16(); e > 0; e--) {
                        String exclusionName = name(in), indexName = name(in); byte[] encoded = SqlText.encode(exclusionName);
                        if (previousExclusion != null && Arrays.compareUnsigned(previousExclusion, encoded) >= 0) throw corrupt("unordered exclusions"); previousExclusion = encoded;
                        List<Integer> ordinalsForExclusion = new ArrayList<>(); List<String> operators = new ArrayList<>();
                        for (int c = in.u16(); c > 0; c--) { int ordinal = in.u16(), operator = in.u8();
                            if (ordinal >= columns.size() || operator > 1) throw corrupt("invalid exclusion element"); ordinalsForExclusion.add(ordinal); operators.add(operator == 0 ? "&&" : "="); }
                        Engine.Index backing = indexes.stream().filter(index -> index.name().equals(indexName)).findFirst().orElse(null);
                        if (ordinalsForExclusion.isEmpty() || !exclusionName.equals(indexName) || backing == null || backing.unique() || backing.kind() == 1
                            || !backing.keys().stream().map(Engine.IndexKey::column).toList().equals(ordinalsForExclusion)
                            || backing.kind() == 0 && operators.stream().anyMatch(operator -> !operator.equals("="))) throw corrupt("invalid exclusion backing index");
                        exclusions.add(new Engine.Exclusion(exclusionName, indexName, ordinalsForExclusion, operators));
                    }
                    for (var index : indexes) if (index.kind() == 2 && (index.keys().size() != 1 || index.predicate() != null) && exclusions.stream().noneMatch(exclusion -> exclusion.index().equals(index.name()))) throw corrupt("GiST constraint index has no exclusion");
                    long root = in.unsigned(4); byte[] rootTag = in.raw(pager.layout().tagBytes()); long rows = in.unsigned(8);
                    if (root == 0) for (byte value : rootTag) if (value != 0) throw corrupt("nonzero empty table tag");
                    if (rows < 0 || (root == 0) != (rows == 0) || root != 0 && (root < pager.layout().firstBody() || root >= meta.pageCount())
                        || rows > meta.pageCount() * pager.layout().capacity()) throw corrupt("invalid table root or cardinality");
                    var tree = PersistentTree.open(pager, root, rows, meta.pageCount(), meta.remaps(),
                        new TableLeafCodec(columns.stream().map(Engine.Column::type).toList(), primary, pager, meta.pageCount()), rootTag);
                    long nextRow = 0;
                    if (primary.length == 0 && rows != 0) {
                        byte[] last = tree.lastKey();
                        if (last.length != 8) throw corrupt("invalid implicit rowid key");
                        nextRow = new Encoding.Reader(last).signed(8);
                        if (nextRow < 0) throw corrupt("negative implicit rowid");
                        if (nextRow != Long.MAX_VALUE) nextRow++;
                    }
                    List<int[]> unique = indexes.stream().filter(index -> index.constraint() && index.keys().stream().allMatch(key -> key.column() >= 0))
                        .map(index -> index.keys().stream().mapToInt(Engine.IndexKey::column).toArray()).toList();
                    tables.put(name, new Engine.Table(name, columns, primary, unique, tree, nextRow, checks, indexes, foreignKeys, exclusions));
                }
                while (in.remaining() != 0) if (in.u8() != 0) throw corrupt("nonzero catalog padding");
            }
        }
        statistics.finish(tables);
        Engine.Snapshot snapshot = new Engine.Snapshot(tables, types == null ? resolveDefinitions(rawTypes) : types, views, sequences, meta.txid(), collations);
        for (Engine.Sequence sequence : sequences.values()) {
            if (tables.containsKey(sequence.name()) || views.containsKey(sequence.name())) throw corrupt("duplicate relation name");
            if (sequence.owner() != null) {
                Engine.Table table = tables.get(sequence.owner());
                if (table == null || sequence.ownerColumn() >= table.columns().size()) throw corrupt("invalid sequence owner");
            }
        }
        HashSet<String> relations = new HashSet<>(tables.keySet()); relations.addAll(views.keySet()); relations.addAll(sequences.keySet());
        for (Engine.Table table : tables.values()) for (Engine.Index index : table.indexes()) if (!relations.add(index.name())) throw corrupt("duplicate relation or index name");
        for (Engine.Table table : tables.values()) for (Engine.ForeignKey key : table.foreignKeys()) {
            Engine.Table parent = tables.get(key.parent());
            if (parent == null || key.referenced().stream().anyMatch(ordinal -> ordinal >= parent.columns().size())) throw corrupt("invalid foreign key parent");
            for (int i = 0; i < key.local().size(); i++) if (!ForeignKeys.comparable(table.columns().get(key.local().get(i)).type(), parent.columns().get(key.referenced().get(i)).type()))
                throw corrupt("incompatible foreign key column types");
        }
        validateViews(snapshot);
        return snapshot;
    }

    private static List<Engine.Index> readIndexes(Encoding.Reader in, String table, List<Engine.Column> columns, Pager pager, PlaintextRecovery.Meta meta) throws IOException {
        int count = in.u16(); List<Engine.Index> indexes = new ArrayList<>(count); byte[] previous = null;
        var bindings = columns.stream().map(column -> new ScalarPlan.Binding(List.of(table), column.name(), column.type(), false, false, CollationDerivation.collatable(column.type()) ? CollationDerivation.implicit(column.collation()) : CollationDerivation.NONE)).toList();
        for (int n = 0; n < count; n++) {
            String indexName = name(in); byte[] encodedName = SqlText.encode(indexName);
            if (previous != null && Arrays.compareUnsigned(previous, encodedName) >= 0) throw corrupt("unordered index catalog"); previous = encodedName;
            int keyCount = in.u16(); if (keyCount == 0) throw corrupt("empty index key list");
            List<Engine.IndexKey> keys = new ArrayList<>(keyCount);
            for (int k = 0; k < keyCount; k++) {
                int ordinal = in.u16(); SqlParser.StoredExpression expression = null; SqlType type;
                if (ordinal == 65535) { ordinal = -1; expression = storedExpression(text(in.sized()), "index \"" + indexName + "\" of table \"" + table + "\"");
                    type = null; }
                else { if (ordinal >= columns.size()) throw corrupt("invalid index column ordinal"); type = columns.get(ordinal).type(); }
                keys.add(new Engine.IndexKey(ordinal, expression, type, false, false, ordinal >= 0 ? columns.get(ordinal).collation() : IndexCollations.derive(expression, bindings)));
            }
            int flags = in.u8(), kind = in.u8();
            if ((flags & 192) != 0 || (flags & 32) != 0 && (flags & 1) == 0 || kind > 2 || kind != 0 && (flags & 17) != 0) throw corrupt("invalid index flags or kind");
            if (kind == 2) for (var key : keys) { if (key.column() < 0) throw corrupt("GiST expression key"); try { GistIndex.validateType(key.type()); } catch (SqlException failure) { throw corrupt("invalid GiST key type"); } }
            if (kind == 1) {
                if (keyCount > 32) throw corrupt("too many GIN keys");
                for (var key : keys) {
                    if (key.column() < 0) throw corrupt("GIN expression key");
                    try { GinIndex.validateType(key.type()); } catch (SqlException failure) { throw corrupt("invalid GIN key type"); }
                }
            }
            long root = in.unsigned(4); byte[] tag = in.raw(pager.layout().tagBytes());
            if (root == 0) for (byte value : tag) if (value != 0) throw corrupt("nonzero empty index tag");
            SqlParser.StoredExpression predicate = (flags & 2) == 0 ? null : storedExpression(text(in.sized()), "index \"" + indexName + "\" of table \"" + table + "\"");
            List<HostDependencies.Dependency> hostDependencies = (flags & 4) == 0 ? List.of() : readHostDependencies(in);
            if (kind != 0 && !hostDependencies.isEmpty()) throw corrupt("host dependencies on non-B-tree index");
            ScalarPlan.Resolver schemaResolver = new ScalarPlan.Resolver(new SqlValue[0], bindings, new ScalarPlan.Frame()).persisted().schemaFunctions(hostDependencies.stream().map(HostDependencies.Dependency::schemaFunction).toList());
            for (int k = 0; k < keys.size(); k++) {
                var key = keys.get(k);
                if (key.expression() != null) keys.set(k, new Engine.IndexKey(-1, key.expression(), StoredDefinitions.parsed(key.expression()) ? schemaResolver.resolve(key.expression(), null).type() : SqlType.UNKNOWN, false, false, key.collation()));
            }
            IndexDependencies.TimeZones zones = IndexDependencies.TimeZones.none();
            if ((flags & 8) != 0) {
                if (kind != 0) throw corrupt("timezone dependencies on non-B-tree index");
                int dynamic = in.u8(), zoneCount = in.u16();
                if (dynamic > 1 || dynamic == 0 && zoneCount == 0) throw corrupt("invalid timezone dependency list");
                List<IndexDependencies.ZonePin> pins = new ArrayList<>(); byte[] lastName = null;
                for (int z = 0; z < zoneCount; z++) {
                    byte[] nameBytes = in.sized(); String zoneName = text(nameBytes), version = text(in.sized());
                    if (zoneName.isEmpty() || version.isEmpty() || lastName != null && Arrays.compareUnsigned(lastName, nameBytes) >= 0) throw corrupt("invalid timezone dependency");
                    pins.add(new IndexDependencies.ZonePin(zoneName, version, (int)in.unsigned(4))); lastName = nameBytes;
                }
                zones = new IndexDependencies.TimeZones(dynamic != 0, pins);
            }
            if ((flags & 16) != 0) {
                int any = 0;
                for (int k = 0; k < keyCount; k++) { int option = in.u8(); if ((option & ~3) != 0) throw corrupt("reserved index key option"); any |= option;
                    var old = keys.get(k); keys.set(k, new Engine.IndexKey(old.column(), old.expression(), old.type(), (option & 1) != 0, (option & 2) != 0, old.collation())); }
                if (any == 0) throw corrupt("noncanonical empty index options");
            }
            if (kind == 2) {
                var loaded = GistTree.open(pager, new org.jed.storage.PagePointer(root, tag), meta.pageCount(), keys.stream().map(Engine.IndexKey::type).toList());
                indexes.add(new Engine.Index(indexName, keys, false, false, predicate, kind, loaded.entries(), zones, loaded.tree(), hostDependencies));
            } else {
                var tree = PersistentTree.openIndex(pager, root, meta.pageCount(), meta.remaps(), TableLeafCodec.index(), tag);
                indexes.add(new Engine.Index(indexName, keys, (flags & 1) != 0, (flags & 32) != 0, predicate, kind, tree, zones, null, hostDependencies));
            }
        }
        return List.copyOf(indexes);
    }
    static List<Engine.Index> orderedIndexes(Engine.Table table) {
        return table.indexes().stream().sorted((a,b) -> Arrays.compareUnsigned(SqlText.encode(a.name()),SqlText.encode(b.name()))).toList();
    }

    static List<Engine.CollationReference> orderedCollations(Engine.Snapshot snapshot) {
        return snapshot.collations().values().stream().sorted((a, b) -> Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name()))).toList();
    }
    static byte[] collation(Engine.CollationReference collation) {
        Encoding.Sink out = new Encoding.Sink(); out.u8(3); out.u8(collation.isDefault() ? 1 : 0);
        for (String value : new String[]{collation.name(), collation.unicodeVersion(), collation.cldrVersion(), collation.description()}) out.sized(SqlText.encode(value));
        return out.bytes();
    }

    static List<Engine.Sequence> orderedSequences(Engine.Snapshot snapshot) {
        return snapshot.sequences().values().stream().sorted((a, b) -> Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name()))).toList();
    }
    static byte[] sequence(Engine.Sequence sequence) {
        Encoding.Sink out = new Encoding.Sink(); out.u8(2); out.sized(SqlText.encode(sequence.name()));
        for (long value : new long[]{sequence.increment(), sequence.minimum(), sequence.maximum(), sequence.start(), sequence.cache(), sequence.last()}) out.unsigned(value, 8);
        out.u8((sequence.cycle() ? 1 : 0) | (sequence.called() ? 2 : 0) | (sequence.owner() == null ? 0 : 4));
        if (sequence.owner() != null) { out.sized(SqlText.encode(sequence.owner())); out.u16(sequence.ownerColumn()); }
        return out.bytes();
    }
    private static Engine.Sequence readSequence(Encoding.Reader in, String name) {
        long increment = in.unsigned(8), minimum = in.unsigned(8), maximum = in.unsigned(8), start = in.unsigned(8), cache = in.unsigned(8), last = in.unsigned(8);
        int flags = in.u8();
        if ((flags & ~7) != 0 || increment == 0 || minimum >= maximum || start < minimum || start > maximum || cache <= 0 || last < minimum || last > maximum) throw corrupt("invalid sequence definition");
        String owner = (flags & 4) == 0 ? null : name(in); int column = owner == null ? -1 : in.u16();
        return new Engine.Sequence(name, increment, minimum, maximum, start, cache, last, (flags & 1) != 0, (flags & 2) != 0, owner, column);
    }

    static List<byte[]> statistics(Engine.Snapshot snapshot) { return StatisticsCodec.encode(snapshot); }

    static List<Engine.View> orderedViews(Engine.Snapshot snapshot) {
        return snapshot.views().values().stream().sorted((a, b) -> Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name()))).toList();
    }
    static byte[] view(Engine.View view) {
        Encoding.Sink out = new Encoding.Sink(); out.u8(5); out.sized(SqlText.encode(view.name())); out.u16(view.columns().size());
        for (Engine.ViewColumn column : view.columns()) {
            out.sized(SqlText.encode(column.name())); out.u8(column.defaultText() == null ? 0 : 1);
            if (column.defaultText() != null) longText(out, column.defaultText());
        }
        longText(out, view.definition()); out.u16(view.dependencies().size());
        for (Engine.ViewDependency dependency : view.dependencies()) {
            out.u8(dependency.kind()); out.sized(SqlText.encode(dependency.name())); out.u16(dependency.columns().size());
            for (String column : dependency.columns()) out.sized(SqlText.encode(column));
        }
        out.u8(view.checkOption()); out.u8(view.flags()); return out.bytes();
    }
    private static void longText(Encoding.Sink out, String text) {
        byte[] bytes = SqlText.encode(text); out.unsigned(bytes.length, 4); out.raw(bytes);
    }
    private static String longText(Encoding.Reader in) {
        long size = in.unsigned(4); if (size > in.remaining()) throw corrupt("truncated view text"); return text(in.raw((int)size));
    }
    private static Engine.View readView(Encoding.Reader in, String name, int count) {
        if (count == 0) throw corrupt("zero-column view");
        List<Engine.ViewColumn> columns = new ArrayList<>(); HashSet<String> names = new HashSet<>();
        for (int c = 0; c < count; c++) {
            String column = name(in); if (column.isEmpty() || !names.add(column)) throw corrupt("invalid view column name");
            int flags = in.u8(); if ((flags & ~1) != 0) throw corrupt("invalid view column flags");
            columns.add(new Engine.ViewColumn(column, flags == 0 ? null : longText(in)));
        }
        String definition = longText(in); int dependencies = in.u16();
        List<Engine.ViewDependency> deps = new ArrayList<>(); byte[] previous = null; int previousKind = -1;
        for (int d = 0; d < dependencies; d++) {
            int kind = in.u8(); if (kind > 3) throw corrupt("invalid view dependency kind");
            String dep = name(in); byte[] bytes = SqlText.encode(dep);
            if (previous != null) {
                int order = Arrays.compareUnsigned(previous, bytes);
                if (order > 0 || order == 0 && previousKind >= kind) throw corrupt("invalid view dependency order");
            }
            previous = bytes; previousKind = kind;
            int width = in.u16(); if (kind != 0 && width != 0) throw corrupt("non-table dependency columns");
            List<String> fields = new ArrayList<>(); byte[] last = null;
            for (int c = 0; c < width; c++) {
                String field = name(in); byte[] encoded = SqlText.encode(field);
                if (last != null && Arrays.compareUnsigned(last, encoded) >= 0) throw corrupt("invalid view dependency column order");
                last = encoded; fields.add(field);
            }
            deps.add(new Engine.ViewDependency(kind, dep, fields));
        }
        int check = in.u8(), flags = in.u8();
        if (check > 2 || (flags & ~3) != 0) throw corrupt("invalid view options");
        return new Engine.View(name, columns, definition, deps, check, flags);
    }
    private static void validateViews(Engine.Snapshot snapshot) {
        Map<String, Integer> pending = new TreeMap<>(); Map<String, List<String>> users = new TreeMap<>();
        for (Engine.View view : snapshot.views().values()) {
            if (snapshot.tables().containsKey(view.name())) throw corrupt("duplicate relation name");
            int count = 0;
            for (Engine.ViewDependency dependency : view.dependencies()) {
                if (dependency.kind() == 0) {
                    Engine.Table table = snapshot.tables().get(dependency.name()); if (table == null) throw corrupt("dangling view table dependency");
                    for (String column : dependency.columns()) if (table.columns().stream().noneMatch(c -> c.name().equals(column))) throw corrupt("dangling view column dependency");
                } else if (dependency.kind() == 1) {
                    if (!snapshot.views().containsKey(dependency.name())) throw corrupt("dangling view dependency");
                    count++; users.computeIfAbsent(dependency.name(), ignored -> new ArrayList<>()).add(view.name());
                } else if (dependency.kind() == 3) {
                    if (!snapshot.types().containsKey(dependency.name())) throw corrupt("dangling view type dependency");
                } else if (!snapshot.sequences().containsKey(dependency.name())) throw corrupt("dangling view sequence dependency");
            }
            pending.put(view.name(), count);
        }
        java.util.ArrayDeque<String> ready = new java.util.ArrayDeque<>();
        pending.forEach((name, count) -> { if (count == 0) ready.add(name); }); int visited = 0;
        while (!ready.isEmpty()) {
            String name = ready.remove(); visited++;
            for (String user : users.getOrDefault(name, List.of())) if (pending.compute(user, (ignored, count) -> count - 1) == 0) ready.add(user);
        }
        if (visited != pending.size()) throw corrupt("cyclic view dependencies");
    }

    static List<Engine.Table> ordered(Engine.Snapshot snapshot) {
        return snapshot.tables().values().stream().sorted((a, b) -> Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name()))).toList();
    }
    static List<SqlType> orderedTypes(Engine.Snapshot snapshot) {
        return snapshot.types().values().stream().sorted((a, b) -> Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name()))).toList();
    }
    static byte[] definition(SqlType type) {
        if (type.kind() != SqlType.Kind.COMPOSITE || type.fields().isEmpty()) throw new IllegalArgumentException("invalid named type definition");
        Encoding.Sink out = new Encoding.Sink(); out.u8(1); out.sized(SqlText.encode(type.name())); out.u16(type.fields().size());
        for (SqlType.Field field : type.fields()) {
            SqlType member = field.type(); out.sized(SqlText.encode(field.name())); out.u8(code(member));
            if (member.kind() == SqlType.Kind.COMPOSITE) out.sized(SqlText.encode(member.name()));
            else if (member.kind() == SqlType.Kind.ARRAY) descriptor(out, member.element());
            else if (member.kind() == SqlType.Kind.RANGE) out.u8(code(member.element()));
            out.u8(field.notNull() ? 1 : 0);
            switch (member.kind()) {
                case TEXT -> out.unsigned(Math.max(0, member.maxLength()), 4);
                case DECIMAL -> { out.u16(Math.max(0, member.precision())); out.u16(Math.max(0, member.scale())); }
                case TIMESTAMP, TIMESTAMPTZ -> out.u8(member.precision() < 0 ? 255 : member.precision());
                case INTERVAL -> { out.u8(member.intervalFields()); out.u8(member.precision() < 0 ? 255 : member.precision()); }
                default -> { }
            }
        }
        return out.bytes();
    }
    private static void descriptor(Encoding.Sink out, SqlType type) {
        out.u8(code(type)); if (type.kind() == SqlType.Kind.COMPOSITE) out.sized(SqlText.encode(type.name()));
    }
    private static SqlType readDefinition(Encoding.Reader in, String name, int count) {
        if (count == 0) throw corrupt("empty composite definition");
        List<SqlType.Field> fields = new ArrayList<>(); HashSet<String> names = new HashSet<>();
        for (int i = 0; i < count; i++) {
            String fieldName = name(in); if (!names.add(fieldName)) throw corrupt("duplicate composite field");
            int code = in.u8(); boolean container = code == 14 || code == 15 || code == 17;
            SqlType member = container ? type(in, code) : null;
            int flags = in.u8(); if (flags > 1) throw corrupt("invalid composite field flags");
            if (!container) member = type(in, code);
            fields.add(new SqlType.Field(fieldName, member, flags == 1));
        }
        return SqlType.composite(name, fields);
    }
    private static Map<String, SqlType> resolveDefinitions(Map<String, SqlType> raw) {
        Map<String, SqlType> resolved = new TreeMap<>();
        Map<String, Integer> pending = new TreeMap<>(); Map<String, List<String>> users = new TreeMap<>();
        var ready = new java.util.ArrayDeque<String>();
        for (var entry : raw.entrySet()) {
            HashSet<String> dependencies = new HashSet<>();
            for (SqlType.Field field : entry.getValue().fields()) {
                SqlType member = field.type(); if (member.kind() == SqlType.Kind.ARRAY) member = member.element();
                if (member.kind() == SqlType.Kind.COMPOSITE) dependencies.add(member.name());
            }
            for (String dependency : dependencies) {
                if (!raw.containsKey(dependency)) throw corrupt("dangling composite type reference");
                users.computeIfAbsent(dependency, ignored -> new ArrayList<>()).add(entry.getKey());
            }
            pending.put(entry.getKey(), dependencies.size()); if (dependencies.isEmpty()) ready.add(entry.getKey());
        }
        while (!ready.isEmpty()) {
            String name = ready.remove(); List<SqlType.Field> fields = new ArrayList<>();
            for (SqlType.Field field : raw.get(name).fields()) fields.add(new SqlType.Field(field.name(), resolveDescriptor(field.type(), resolved), field.notNull()));
            resolved.put(name, SqlType.composite(name, fields));
            for (String user : users.getOrDefault(name, List.of())) if (pending.compute(user, (ignored, count) -> count - 1) == 0) ready.add(user);
        }
        if (resolved.size() != raw.size()) throw corrupt("cyclic composite type definitions");
        return resolved;
    }
    private static SqlType resolveDescriptor(SqlType type, Map<String, SqlType> definitions) {
        if (type.kind() == SqlType.Kind.ARRAY) return SqlType.array(resolveDescriptor(type.element(), definitions));
        if (type.kind() != SqlType.Kind.COMPOSITE) return type;
        SqlType resolved = definitions.get(type.name()); if (resolved == null) throw corrupt("dangling composite type reference"); return resolved;
    }

    static byte[] table(Engine.Table table, long root) {
        return table(table, root, new byte[0]);
    }
    static byte[] table(Engine.Table table, long root, byte[] rootTag) {
        return table(table, root, rootTag, Map.of());
    }
    static byte[] table(Engine.Table table, long root, byte[] rootTag, Map<String, org.jed.storage.PagePointer> indexRoots) {
        if (!table.unique().isEmpty() && table.indexes().stream().noneMatch(Engine.Index::constraint)) throw unsupported("unique constraint missing backing index");
        Encoding.Sink out = new Encoding.Sink(); out.u8(0); out.sized(SqlText.encode(table.name())); out.u16(table.columns().size());
        for (Engine.Column column : table.columns()) {
            SqlParser.StoredExpression expression = column.defaultValue() instanceof SqlParser.StoredExpression stored ? stored : null;
            SqlValue value = column.defaultValue() == null || expression != null ? null : constant(column.defaultValue(), column.type());
            if (column.type().kind() == SqlType.Kind.COMPOSITE && value != null) throw unsupported("composite default without retained expression text");
            out.sized(SqlText.encode(column.name())); out.u8(code(column.type()));
            out.u8((column.notNull() ? 2 : 0) | (expression != null ? 8 : value == null ? 0 : 4) | (column.identity() == 0 ? 0 : column.identity() == 1 ? 16 : 48) | (column.collation().equals("C") ? 0 : 64));
            switch (column.type().kind()) {
                case TEXT -> out.unsigned(Math.max(0, column.type().maxLength()), 4);
                case DECIMAL -> { out.u16(Math.max(0, column.type().precision())); out.u16(Math.max(0, column.type().scale())); }
                case TIMESTAMP, TIMESTAMPTZ -> out.u8(column.type().precision() < 0 ? 255 : column.type().precision());
                case INTERVAL -> { out.u8(column.type().intervalFields()); out.u8(column.type().precision() < 0 ? 255 : column.type().precision()); }
                case ARRAY -> descriptor(out, column.type().element());
                case COMPOSITE -> out.sized(SqlText.encode(column.type().name()));
                case RANGE -> out.u8(code(column.type().element()));
                default -> { }
            }
            if (expression != null) out.sized(SqlText.encode(expression.text()));
            else if (value != null) Encoding.write(out, value);
            if (!column.collation().equals("C")) out.sized(SqlText.encode(column.collation()));
        }
        out.u16(table.primaryKey().length);
        for (int ordinal : table.primaryKey()) out.u16(ordinal);
        out.u16(table.checks().size());
        for (Engine.Check check : table.checks()) {
            out.sized(SqlText.encode(check.name())); out.sized(SqlText.encode(check.expression().text())); out.u8(check.hostDependencies().isEmpty() ? 0 : 1);
            if (!check.hostDependencies().isEmpty()) writeHostDependencies(out, check.hostDependencies());
        }
        out.u16(table.indexes().size());
        for (Engine.Index index : orderedIndexes(table)) {
            out.sized(SqlText.encode(index.name())); out.u16(index.keys().size());
            for (Engine.IndexKey key : index.keys()) { out.u16(key.column() < 0 ? 65535 : key.column()); if (key.column() < 0) out.sized(SqlText.encode(key.expression().text())); }
            boolean options = index.keys().stream().anyMatch(key -> key.descending() || key.nullsFirst());
            out.u8((index.unique() ? 1 : 0) | (index.predicate() == null ? 0 : 2) | (index.hostDependencies().isEmpty() ? 0 : 4) | (index.timezoneDependencies().present() ? 8 : 0) | (options ? 16 : 0) | (index.constraint() ? 32 : 0)); out.u8(index.kind());
            var pointer = indexRoots.get(index.name());
            out.unsigned(pointer == null ? index.rootPointer().page() : pointer.page(), 4); out.raw(pointer == null ? index.rootPointer().tag() : pointer.tag());
            if (index.predicate() != null) out.sized(SqlText.encode(index.predicate().text()));
            if (!index.hostDependencies().isEmpty()) writeHostDependencies(out, index.hostDependencies());
            if (index.timezoneDependencies().present()) {
                out.u8(index.timezoneDependencies().dynamic() ? 1 : 0); out.u16(index.timezoneDependencies().pins().size());
                for (var pin : index.timezoneDependencies().pins()) { out.sized(SqlText.encode(pin.name())); out.sized(SqlText.encode(pin.version())); out.unsigned(Integer.toUnsignedLong(pin.crc()), 4); }
            }
            if (options) for (Engine.IndexKey key : index.keys()) out.u8((key.descending() ? 1 : 0) | (key.nullsFirst() ? 2 : 0));
        }
        out.u16(table.foreignKeys().size());
        for (Engine.ForeignKey key : table.foreignKeys()) {
            out.sized(SqlText.encode(key.name())); out.u16(key.local().size()); for (int ordinal : key.local()) out.u16(ordinal);
            out.sized(SqlText.encode(key.parent())); out.u16(key.referenced().size()); for (int ordinal : key.referenced()) out.u16(ordinal);
            out.u8(key.onDelete() | key.onUpdate() << 3 | (key.matchFull() ? 64 : 0));
        }
        out.u16(table.exclusions().size());
        for (Engine.Exclusion exclusion : table.exclusions()) {
            out.sized(SqlText.encode(exclusion.name())); out.sized(SqlText.encode(exclusion.index())); out.u16(exclusion.columns().size());
            for (int i = 0; i < exclusion.columns().size(); i++) { out.u16(exclusion.columns().get(i)); out.u8(exclusion.operators().get(i).equals("&&") ? 0 : 1); }
        }
        out.unsigned(root, 4); out.raw(rootTag); out.unsigned(table.rows().size(), 8);
        return out.bytes();
    }

    private static SqlValue constant(SqlParser.Expr expression, SqlType target) {
        if (expression instanceof SqlParser.StoredValue value) return SqlCasts.cast(value.value(), target);
        if (expression instanceof SqlParser.Cast cast) return SqlCasts.cast(constant(cast.value(), new ScalarPlan.Resolver(new SqlValue[0]).type(cast.target())), target);
        if (expression instanceof SqlParser.Unary unary && (unary.op().equals("-") || unary.op().equals("+"))
            && unary.value() instanceof SqlParser.Literal literal) {
            return literal(new SqlParser.Literal(literal.kind(), (unary.op().equals("-") ? "-" : "") + literal.text()), target);
        }
        if (expression instanceof SqlParser.Literal literal) return literal(literal, target);
        throw unsupported("expression defaults without retained canonical token text");
    }
    private static SqlParser.StoredExpression storedExpression(String text, String detail) {
        try {
            var query = new SqlParser("SELECT " + text).parse();
            if (query.items().size() != 1 || query.items().getFirst().alias() != null || !query.from().isEmpty() || query.where() != null
                || !query.order().isEmpty() || query.limit() != null || query.offset() != null || query.distinct() || !query.group().isEmpty() || query.having() != null)
                throw corrupt("invalid persisted default expression");
            return new SqlParser.StoredExpression(text, query.items().getFirst().expression());
        } catch (SqlException failure) { return new SqlParser.StoredExpression(text, new SqlParser.UnparsedExpression(text, detail)); }
    }
    private static SqlValue literal(SqlParser.Literal literal, SqlType target) {
        if (literal.kind() == Lexer.Kind.WORD && literal.text().equals("null")) return SqlValue.nullValue(target);
        if (literal.kind() == Lexer.Kind.STRING) return SqlCasts.cast(SqlValue.ofText(literal.text()), target);
        if (literal.kind() == Lexer.Kind.WORD && (literal.text().equals("true") || literal.text().equals("false"))) {
            return SqlCasts.cast(SqlValue.ofBoolean(literal.text().equals("true")), target);
        }
        if (literal.kind() == Lexer.Kind.INTEGER || literal.kind() == Lexer.Kind.DECIMAL) return SqlCasts.cast(SqlValue.ofText(literal.text()), target);
        throw unsupported("catalog literal default");
    }
    private static List<HostDependencies.Dependency> readHostDependencies(Encoding.Reader in) {
        int count = in.u16(); if (count == 0) throw corrupt("empty host dependency list");
        List<HostDependencies.Dependency> dependencies = new ArrayList<>(); HostDependencies.Dependency previous = null;
        for (int i = 0; i < count; i++) {
            String name = name(in); int argc = in.u16(); List<SqlType> arguments = new ArrayList<>();
            for (int a = 0; a < argc; a++) arguments.add(hostType(in));
            SqlType result = hostType(in); String component = name(in); long version = in.unsigned(4);
            var dependency = new HostDependencies.Dependency(name, arguments, result, component, version);
            if (previous != null && HostDependencies.compare(previous, dependency) >= 0) throw corrupt("unordered host dependencies");
            dependencies.add(dependency); previous = dependency;
        }
        return List.copyOf(dependencies);
    }
    private static SqlType hostType(Encoding.Reader in) {
        int code = in.u8();
        if (code < 1 || code > 19 || code == 14 || code == 15 || code == 17) throw corrupt("invalid host signature type");
        return elementType(new Encoding.Reader(new byte[]{(byte)code}));
    }
    private static void writeHostDependencies(Encoding.Sink out, List<HostDependencies.Dependency> dependencies) {
        out.u16(dependencies.size());
        for (var dependency : dependencies) {
            out.sized(SqlText.encode(dependency.name())); out.u16(dependency.arguments().size());
            for (SqlType argument : dependency.arguments()) out.u8(code(argument));
            out.u8(code(dependency.result())); out.sized(SqlText.encode(dependency.componentId())); out.unsigned(dependency.semanticVersion(), 4);
        }
    }
    private static String name(Encoding.Reader in) {
        String name = text(in.sized());
        if (name.isEmpty()) throw corrupt("empty catalog identifier");
        return name;
    }
    private static String text(byte[] bytes) {
        try { return SqlText.decode(bytes); } catch (SqlException failure) { throw corrupt("invalid UTF-8 catalog text"); }
    }
    private static SqlType type(Encoding.Reader in, int code) {
        try {
            return switch (code) {
                case 1 -> SqlType.I16; case 2 -> SqlType.I32; case 3 -> SqlType.I64;
                case 4 -> { long length = in.unsigned(4); if (length > 10_485_760) throw corrupt("invalid text typmod"); yield length == 0 ? SqlType.TEXT : SqlType.varchar((int) length); }
                case 5 -> SqlType.BOOLEAN;
                case 6 -> { int precision = in.u16(), scale = in.u16(); if (precision == 0 && scale != 0) throw corrupt("invalid unconstrained decimal"); yield precision == 0 ? SqlType.DECIMAL : SqlType.decimal(precision, scale); }
                case 7 -> SqlType.BYTEA; case 8 -> SqlType.UUID; case 12 -> SqlType.F64; case 13 -> SqlType.F32; case 16 -> SqlType.DATE;
                case 9, 10 -> { int precision = in.u8(); if (precision != 255 && precision > 6) throw corrupt("invalid timestamp precision"); yield precision == 255 ? code == 10 ? SqlType.TIMESTAMPTZ : SqlType.TIMESTAMP : SqlType.timestamp(code == 10, precision); }
                case 11 -> { int qualifier = in.u8(), precision = in.u8(); if (qualifier > 13 || precision != 255 && precision > 6) throw corrupt("invalid interval typmod"); yield SqlType.interval(qualifier, precision == 255 ? -1 : precision); }
                case 14 -> SqlType.composite(name(in), List.of());
                case 15 -> SqlType.array(elementType(in));
                case 17 -> { SqlType element = elementType(in); if (!(element == SqlType.I32 || element == SqlType.I64 || element == SqlType.DECIMAL || element == SqlType.DATE || element == SqlType.TIMESTAMP || element == SqlType.TIMESTAMPTZ)) throw corrupt("invalid range element descriptor"); yield SqlType.range(element); }
                case 18 -> SqlType.JSON; case 19 -> SqlType.JSONB; case 20 -> SqlType.JSONPATH;
                default -> throw corrupt("unknown catalog type code");
            };
        } catch (SqlException failure) {
            if (failure.sqlState().equals("22023")) throw corrupt("invalid catalog typmod");
            throw failure;
        }
    }
    private static SqlType elementType(Encoding.Reader in) {
        int code = in.u8();
        return switch (code) {
            case 1 -> SqlType.I16; case 2 -> SqlType.I32; case 3 -> SqlType.I64; case 4 -> SqlType.TEXT;
            case 5 -> SqlType.BOOLEAN; case 6 -> SqlType.DECIMAL; case 7 -> SqlType.BYTEA; case 8 -> SqlType.UUID;
            case 9 -> SqlType.TIMESTAMP; case 10 -> SqlType.TIMESTAMPTZ; case 11 -> SqlType.INTERVAL;
            case 12 -> SqlType.F64; case 13 -> SqlType.F32; case 16 -> SqlType.DATE;
            case 14 -> SqlType.composite(name(in), List.of());
            case 18 -> SqlType.JSON; case 19 -> SqlType.JSONB; case 20 -> SqlType.JSONPATH;
            default -> throw corrupt("invalid array element type code");
        };
    }
    static int code(SqlType type) {
        return switch (type.kind()) {
            case I16 -> 1; case I32 -> 2; case I64 -> 3; case TEXT -> 4; case BOOLEAN -> 5; case DECIMAL -> 6;
            case BYTEA -> 7; case UUID -> 8; case TIMESTAMP -> 9; case TIMESTAMPTZ -> 10; case INTERVAL -> 11; case F64 -> 12; case F32 -> 13; case COMPOSITE -> 14; case ARRAY -> 15; case DATE -> 16; case RANGE -> 17;
            case JSON -> 18; case JSONB -> 19; case JSONPATH -> 20;
            default -> throw unsupported("catalog type " + type);
        };
    }
    private static SqlException corrupt(String message) { return Encoding.corrupt(message); }
    private static SqlException unsupported(String message) { return new SqlException("0A000", message + " is not implemented by the experimental Java catalog"); }
}
