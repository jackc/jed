package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import org.jed.storage.PersistentTree;
import org.jed.storage.TreeCapacityException;

/** Relational ownership over immutable table roots; all table access stays at the tree seam. */
final class Engine {
    record Column(String name, SqlType type, boolean notNull, SqlParser.Expr defaultValue, int identity, String collation) {
        Column(String name, SqlType type, boolean notNull, SqlParser.Expr defaultValue, int identity) { this(name, type, notNull, defaultValue, identity, "C"); }
        Column(String name, SqlType type, boolean notNull, SqlParser.Expr defaultValue) { this(name, type, notNull, defaultValue, 0); }
    }
    record Check(String name, SqlParser.StoredExpression expression, List<HostDependencies.Dependency> hostDependencies) {
        Check(String name, SqlParser.StoredExpression expression) { this(name, expression, List.of()); }
        Check { hostDependencies = List.copyOf(hostDependencies); }
    }
    record IndexKey(int column, SqlParser.StoredExpression expression, SqlType type, boolean descending, boolean nullsFirst, String collation) {
        IndexKey(int column, SqlParser.StoredExpression expression, SqlType type, boolean descending, boolean nullsFirst) { this(column, expression, type, descending, nullsFirst, "C"); }
    }
    record Index(String name, List<IndexKey> keys, boolean unique, boolean constraint, SqlParser.StoredExpression predicate, int kind, PersistentTree entries, IndexDependencies.TimeZones timezoneDependencies, GistTree gist, List<HostDependencies.Dependency> hostDependencies) {
        Index(String name, List<IndexKey> keys, boolean unique, boolean constraint, SqlParser.StoredExpression predicate, int kind, PersistentTree entries, IndexDependencies.TimeZones timezoneDependencies, GistTree gist) { this(name, keys, unique, constraint, predicate, kind, entries, timezoneDependencies, gist, List.of()); }
        Index(String name, List<IndexKey> keys, boolean unique, boolean constraint, SqlParser.StoredExpression predicate, int kind, PersistentTree entries, IndexDependencies.TimeZones timezoneDependencies) { this(name, keys, unique, constraint, predicate, kind, entries, timezoneDependencies, null); }
        Index(String name, List<IndexKey> keys, boolean unique, boolean constraint, SqlParser.StoredExpression predicate, int kind, PersistentTree entries) { this(name, keys, unique, constraint, predicate, kind, entries, IndexDependencies.TimeZones.none()); }
        Index { keys = List.copyOf(keys); hostDependencies = List.copyOf(hostDependencies); }
        Index changed(PersistentTree tree) {
            GistTree replacement = gist;
            if (kind == 2 && tree != entries) try { replacement = GistTree.build(keys.stream().map(IndexKey::type).toList(), tree, gist); }
            catch (IOException failure) { throw storageError(failure); }
            return new Index(name, keys, unique, constraint, predicate, kind, tree, timezoneDependencies, replacement, hostDependencies);
        }
        org.jed.storage.PagePointer rootPointer() { return kind == 2 && gist != null ? gist.pointer() : new org.jed.storage.PagePointer(entries.rootPage(), entries.rootTag()); }
        java.util.Set<Long> physicalPages() throws IOException { return kind == 2 && gist != null ? gist.physicalPages() : entries.physicalPages(); }
    }
    record ForeignKey(String name, List<Integer> local, String parent, List<Integer> referenced, int onDelete, int onUpdate, boolean matchFull) {
        ForeignKey { local = List.copyOf(local); referenced = List.copyOf(referenced); }
    }
    record Exclusion(String name, String index, List<Integer> columns, List<String> operators) {
        Exclusion { columns = List.copyOf(columns); operators = List.copyOf(operators); }
    }
    record Table(String name, List<Column> columns, int[] primaryKey, List<int[]> unique,
                 PersistentTree rows, long nextRowId, List<Check> checks, List<Index> indexes, List<ForeignKey> foreignKeys, List<Exclusion> exclusions, Map<Integer, ColumnStatistics> statistics) {
        Table(String name, List<Column> columns, int[] primaryKey, List<int[]> unique, PersistentTree rows, long nextRowId, List<Check> checks, List<Index> indexes, List<ForeignKey> foreignKeys, List<Exclusion> exclusions) { this(name, columns, primaryKey, unique, rows, nextRowId, checks, indexes, foreignKeys, exclusions, Map.of()); }
        Table(String name, List<Column> columns, int[] primaryKey, List<int[]> unique, PersistentTree rows, long nextRowId, List<Check> checks, List<Index> indexes, List<ForeignKey> foreignKeys) { this(name, columns, primaryKey, unique, rows, nextRowId, checks, indexes, foreignKeys, List.of()); }
        Table(String name, List<Column> columns, int[] primaryKey, List<int[]> unique, PersistentTree rows, long nextRowId, List<Check> checks, List<Index> indexes) { this(name, columns, primaryKey, unique, rows, nextRowId, checks, indexes, List.of()); }
        Table(String name, List<Column> columns, int[] primaryKey, List<int[]> unique, PersistentTree rows, long nextRowId) { this(name, columns, primaryKey, unique, rows, nextRowId, List.of(), List.of()); }
        Table(String name, List<Column> columns, int[] primaryKey, List<int[]> unique, PersistentTree rows, long nextRowId, List<Check> checks) { this(name, columns, primaryKey, unique, rows, nextRowId, checks, List.of()); }
        Table { statistics = java.util.Collections.unmodifiableMap(new TreeMap<>(statistics)); exclusions = List.copyOf(exclusions); foreignKeys = List.copyOf(foreignKeys); indexes = List.copyOf(indexes); checks = List.copyOf(checks); columns = List.copyOf(columns); primaryKey = primaryKey.clone(); unique = List.copyOf(unique); }
        List<SqlType> types() { return columns.stream().map(Column::type).toList(); }
        int column(String name) {
            for (int i = 0; i < columns.size(); i++) if (columns.get(i).name.equals(name)) return i;
            throw new SqlException("42703", "column does not exist: " + name);
        }
        List<ScalarPlan.Binding> bindings(String alias) {
            return columns.stream().map(c -> new ScalarPlan.Binding(List.of(alias == null ? name : alias), c.name, c.type, false, false, CollationDerivation.collatable(c.type) ? CollationDerivation.implicit(c.collation) : CollationDerivation.NONE).catalog(c.name, c.notNull)).toList();
        }
        Table changed(PersistentTree tree, long rowId) {
            Map<Integer,ColumnStatistics> facts = statistics;
            if (tree != rows && !statistics.isEmpty()) { facts = new TreeMap<>(); for (var entry : statistics.entrySet()) facts.put(entry.getKey(), entry.getValue().markStale()); }
            return new Table(name, columns, primaryKey, unique, tree, rowId, checks, indexes, foreignKeys, exclusions, facts);
        }
        Table withStatistics(Map<Integer,ColumnStatistics> facts) { return new Table(name, columns, primaryKey, unique, rows, nextRowId, checks, indexes, foreignKeys, exclusions, facts); }
        Table withIndexes(List<Index> replacement) { return new Table(name, columns, primaryKey, unique, rows, nextRowId, checks, replacement, foreignKeys, exclusions, statistics); }
        Table withExclusions(List<Exclusion> replacement) { return new Table(name, columns, primaryKey, unique, rows, nextRowId, checks, indexes, foreignKeys, replacement, statistics); }
        Table withForeignKeys(List<ForeignKey> replacement) { return new Table(name, columns, primaryKey, unique, rows, nextRowId, checks, indexes, replacement, exclusions, statistics); }
    }
    record ViewDependency(int kind, String name, List<String> columns) { ViewDependency { columns = List.copyOf(columns); } }
    record ViewColumn(String name, String defaultText) {}
    static final class View {
        private final String name, definition;
        private final List<ViewColumn> columns;
        private final List<ViewDependency> dependencies;
        private final int checkOption, flags;
        private volatile SqlParser.Statement parsed;
        View(String name, List<ViewColumn> columns, String definition, List<ViewDependency> dependencies, int checkOption, int flags) {
            this.name = name; this.columns = List.copyOf(columns); this.definition = definition;
            this.dependencies = dependencies.stream().sorted((a, b) -> { int order = SqlText.compare(a.name(), b.name()); return order == 0 ? Integer.compare(a.kind(), b.kind()) : order; }).toList();
            this.checkOption = checkOption; this.flags = flags;
        }
        String name() { return name; }
        String definition() { return definition; }
        List<ViewColumn> columns() { return columns; }
        List<ViewDependency> dependencies() { return dependencies; }
        int checkOption() { return checkOption; }
        int flags() { return flags; }
        SqlParser.Statement query() {
            SqlParser.Statement query = parsed;
            if (query != null) return query;
            synchronized (this) {
                if (parsed != null) return parsed;
                try {
                    query = new SqlParser(definition).parseStatement();
                    if (writes(query)) throw new SqlException("XX003", "stored view definition is not a read query");
                    parsed = query; return query;
                } catch (SqlException error) { throw StoredDefinitions.error("view \"" + name + "\""); }
            }
        }
        @Override public boolean equals(Object other) {
            return other instanceof View view && name.equals(view.name) && columns.equals(view.columns) && definition.equals(view.definition)
                && dependencies.equals(view.dependencies) && checkOption == view.checkOption && flags == view.flags;
        }
        @Override public int hashCode() { return java.util.Objects.hash(name, columns, definition, dependencies, checkOption, flags); }
    }
    record Sequence(String name, long increment, long minimum, long maximum, long start, long cache, long last,
                    boolean cycle, boolean called, String owner, int ownerColumn) {}
    record CollationReference(String name, String unicodeVersion, String cldrVersion, String description, boolean isDefault) {}
    record Snapshot(Map<String, Table> tables, Map<String, SqlType> types, Map<String, View> views, Map<String, Sequence> sequences, long version, Map<String, CollationReference> collations, Map<String, Snapshot> domains, Map<String, String> tempActions) {
        Snapshot(Map<String, Table> tables, Map<String, SqlType> types, Map<String, View> views, Map<String, Sequence> sequences, long version, Map<String, CollationReference> collations, Map<String, Snapshot> domains) { this(tables, types, views, sequences, version, collations, domains, Map.of()); }
        Snapshot(Map<String, Table> tables, Map<String, SqlType> types, Map<String, View> views, Map<String, Sequence> sequences, long version, Map<String, CollationReference> collations) { this(tables, types, views, sequences, version, collations, Map.of()); }
        Snapshot(Map<String, Table> tables, Map<String, SqlType> types, Map<String, View> views, Map<String, Sequence> sequences, long version) { this(tables, types, views, sequences, version, Map.of()); }
        Snapshot { tempActions = Map.copyOf(tempActions); domains = java.util.Collections.unmodifiableMap(new TreeMap<>(domains)); collations = java.util.Collections.unmodifiableMap(new TreeMap<>(collations)); tables = java.util.Collections.unmodifiableMap(new TreeMap<>(tables)); types = java.util.Collections.unmodifiableMap(new TreeMap<>(types)); views = java.util.Collections.unmodifiableMap(new TreeMap<>(views)); sequences = java.util.Collections.unmodifiableMap(new TreeMap<>(sequences)); }
        Snapshot(Map<String, Table> tables, Map<String, SqlType> types, Map<String, View> views, long version) { this(tables, types, views, Map.of(), version); }
        Snapshot(Map<String, Table> tables, Map<String, SqlType> types, long version) { this(tables, types, Map.of(), version); }
        Snapshot(Map<String, Table> tables, long version) { this(tables, Map.of(), version); }
        Snapshot withoutDomains() { return domains.getOrDefault("main", this); }
        Snapshot domain(String name) {
            if (name.equals("main")) return withoutDomains();
            Snapshot found = domains.get(name);
            if (found == null) throw new SqlException("42P01", "database \"" + name + "\" is not attached");
            return found;
        }
        Snapshot withDomains(Map<String, Snapshot> values) {
            if (values.isEmpty()) return withoutDomains();
            var combined = new TreeMap<>(values); combined.put("main", withoutDomains());
            return new Snapshot(tables, types, views, sequences, version, collations, combined, tempActions);
        }
        Snapshot withDomain(String name, Snapshot value) {
            value = value.withoutDomains();
            if (name.equals("main")) return domains.isEmpty() ? value : value.withDomains(domains);
            var combined = new TreeMap<>(domains); combined.put("main", withoutDomains()); combined.put(name, value);
            return new Snapshot(tables, types, views, sequences, version, collations, combined, tempActions);
        }
        private Snapshot mainChanged(Snapshot value) { value = value.withTempActions(tempActions); return domains.isEmpty() ? value : value.withDomains(domains); }
        Snapshot withTempActions(Map<String, String> actions) {
            Snapshot value = new Snapshot(tables, types, views, sequences, version, collations, Map.of(), actions);
            return domains.isEmpty() ? value : value.withDomains(domains);
        }
        Snapshot withTempAction(String name, String action) { var next = new TreeMap<>(tempActions); next.put(name, action); return withTempActions(next); }
        Snapshot renameTempAction(String before, String after) { if (!tempActions.containsKey(before)) return this; var next = new TreeMap<>(tempActions); next.put(after, next.remove(before)); return withTempActions(next); }
        Snapshot putSequence(Sequence sequence) { var next = new TreeMap<>(sequences); next.put(sequence.name(), sequence); return mainChanged(new Snapshot(tables, types, views, next, version, collations)); }
        Snapshot dropSequence(String name) { var next = new TreeMap<>(sequences); next.remove(name); return mainChanged(new Snapshot(tables, types, views, next, version, collations)); }
        Snapshot useCollation(Collation collation) {
            if (collation == Collation.C || collations.containsKey(collation.name())) return this;
            var next = new TreeMap<>(collations); next.put(collation.name(), new CollationReference(collation.name(), collation.unicodeVersion(), collation.cldrVersion(), "", false));
            return mainChanged(new Snapshot(tables, types, views, sequences, version, next));
        }
        static Snapshot empty() { return new Snapshot(Map.of(), 0); }
        Snapshot put(Table table) { var next = new TreeMap<>(tables); next.put(table.name, table); return mainChanged(new Snapshot(next, types, views, sequences, version, collations)); }
        Snapshot putType(SqlType type) { var next = new TreeMap<>(types); next.put(type.name(), type); return mainChanged(new Snapshot(tables, next, views, sequences, version, collations)); }
        Snapshot dropType(String name) { var next = new TreeMap<>(types); next.remove(name); return mainChanged(new Snapshot(tables, next, views, sequences, version, collations)); }
        Snapshot drop(String name) { var next = new TreeMap<>(tables); next.remove(name); var actions = new TreeMap<>(tempActions); actions.remove(name); return mainChanged(new Snapshot(next, types, views, sequences, version, collations)).withTempActions(actions); }
        Snapshot withViews(Map<String, View> value) { return views.equals(value) ? this : mainChanged(new Snapshot(tables, types, value, sequences, version, collations)); }
        Snapshot putView(View view) { var next = new TreeMap<>(views); next.put(view.name(), view); return mainChanged(new Snapshot(tables, types, next, sequences, version, collations)); }
        Snapshot dropView(String name) { var next = new TreeMap<>(views); next.remove(name); return mainChanged(new Snapshot(tables, types, next, sequences, version, collations)); }
    }
    private record Savepoint(String name, Snapshot snapshot, Sequences.History history) {}
    private final Database database;
    private final Session session;
    private Snapshot working;
    private Snapshot base;
    private Sequences.History sequenceHistory = Sequences.History.empty(), transactionHistory;
    private Database.Pin transactionPin;
    private ForeignKeys.Deferred deferredReferences;
    private long mutationAffected;
    private Database mutationDatabase;
    private Snapshot mutationPendingRoot;
    private String mutationDomain = "main";
    private Database storageDatabase() { return mutationDatabase == null ? database : mutationDatabase; }
    private boolean explicit;
    private boolean readOnly;
    private boolean failed;
    private boolean writer;
    private final List<Savepoint> savepoints = new ArrayList<>();

    Engine(Database database, Session session) { this.database = database; this.session = session; }
    void openReadHandle() {
        transactionPin = database.pinSnapshot(session); base = working = transactionPin.snapshot(); explicit = true; readOnly = true; transactionHistory = sequenceHistory;
    }
    QueryPlan prepare(SqlParser.Statement statement, SqlValue[] args, CostMeter meter) { return prepare(statement, args, meter, args.length); }
    QueryPlan prepare(SqlParser.Statement statement, SqlValue[] args, CostMeter meter, int parameterCount) {
        try (var tracking = org.jed.storage.PendingBytes.tracking(session.maxQueryMemoryBytes() != 0)) {
            return prepareTracked(statement, args, meter, parameterCount);
        }
    }
    private QueryPlan prepareTracked(SqlParser.Statement statement, SqlValue[] args, CostMeter meter, int parameterCount) {
        if (session.readHandle() && (writes(statement) || statement instanceof SqlParser.Transaction)) throw new SqlException("25006", "read handle cannot modify the database or transaction");
        if (transactionPin != null && !(statement instanceof SqlParser.Transaction transaction && transaction.action().equals("rollback"))) transactionPin.check();
        if (statement instanceof SqlParser.Transaction command) return transaction(command, meter);
        if (failed) throw new SqlException("25P02", "transaction is aborted");
        boolean writes = writes(statement);
        boolean temporaryWrite = writes && DomainRouting.tempOnly(statement, explicit ? working : database.snapshot(session));
        if (writes && !temporaryWrite && explicit && readOnly) throw new SqlException("25006", "read-only transaction");
        boolean automaticWriter = writes && !temporaryWrite && !explicit;
        if (automaticWriter) { database.acquireWriter(session, meter); writer = true; }
        Database.Pin pin = null;
        Returning returning = null;
        QueryEnvironment environment = null;
        try {
            pin = writes ? null : explicit ? database.pin(session, working) : database.pinSnapshot(session);
            Snapshot snapshot = pin == null ? explicit ? working : database.snapshot(session) : pin.snapshot();
            meter.pendingBytes(PendingMemory.bytes(snapshot));
            ViewLimits.check(statement, snapshot.views());
            ParameterBindings.bind(statement, snapshot, args, parameterCount, meter);
            environment = new QueryEnvironment((query, scope) -> read(query, snapshot, args, scope), args, meter);
            environment.readRoot = snapshot;
            environment.namedTypes(snapshot.types());
            meter.collations(snapshot.collations());
            environment.sequences(new Sequences(snapshot, sequenceHistory));
            session.privileges().check(statement, snapshot, environment);
            if (!writes) {
                QueryPlan plan = read(statement, snapshot, args, environment);
                if (statement instanceof SqlParser.Explain || !environment.sequences().mutating()) return pinned(plan, pin, environment);
                boolean temporarySequence = environment.sequences().temporaryOnly();
                plan.close(); pin.close(); pin = null; environment.close(); environment = null;
                return sequenceQuery(statement, args, meter, temporarySequence);
            }
            if (statement instanceof SqlParser.With with) {
                MutationResult execution = writableStatement(with, snapshot, args, meter, environment);
                try {
                    Snapshot changed = environment.sequences().apply(execution.snapshot());
                meter.pendingBytes(PendingMemory.bytes(changed));
                    if (explicit) working = changed;
                    else if (changed != snapshot) database.publish(session, snapshot, TempTables.applyCommit(changed));
                    sequenceHistory = environment.sequences().history();
                    environment.close(); return execution.result();
                } catch (RuntimeException | Error failure) { execution.result().close(); throw failure; }
            }
            MutationResult execution = mutate(statement, snapshot, snapshot, args, meter, environment);
            try {
                Snapshot changed = environment.sequences().apply(execution.snapshot());
                meter.pendingBytes(PendingMemory.bytes(changed));
                if (explicit) working = changed; else if (changed != snapshot) database.publish(session, snapshot, TempTables.applyCommit(changed));
                sequenceHistory = environment.sequences().history(); environment.close(); return execution.result();
            } catch (RuntimeException | Error failure) { execution.result().close(); throw failure; }
        } catch (IOException failure) { if (pin != null) pin.close(); if (returning != null) returning.close(); if (environment != null) environment.close(); throw storageError(failure); }
        catch (RuntimeException | Error failure) { if (pin != null) pin.close(); if (returning != null) returning.close(); if (environment != null) environment.close(); throw failure; }
        finally { if (automaticWriter) { writer = false; database.releaseWriter(session); } }
    }
    private QueryPlan sequenceQuery(SqlParser.Statement statement, SqlValue[] args, CostMeter meter, boolean temporary) {
        if (explicit && readOnly && !temporary) throw new SqlException("25006", "sequence advance in read-only transaction");
        boolean automatic = !explicit && !temporary;
        if (automatic) { database.acquireWriter(session, meter); writer = true; }
        QueryPlan result = null;
        try {
            Snapshot snapshot = explicit ? working : database.snapshot(session);
            try (QueryEnvironment environment = new QueryEnvironment((query, scope) -> read(query, snapshot, args, scope), args, meter)) {
                environment.readRoot = snapshot;
            environment.namedTypes(snapshot.types());
            meter.collations(snapshot.collations()); environment.sequences(new Sequences(snapshot, sequenceHistory));
                result = collectResult(read(statement, snapshot, args, environment), meter);
                Snapshot changed = environment.sequences().apply(snapshot);
                if (explicit) working = changed; else if (changed != snapshot) database.publish(session, snapshot, TempTables.applyCommit(changed));
                sequenceHistory = environment.sequences().history(); return result;
            }
        } catch (RuntimeException | Error failure) { if (result != null) result.close(); throw failure; }
        finally { if (automatic) { writer = false; database.releaseWriter(session); } }
    }

    private record MutationResult(Snapshot snapshot, QueryPlan result) {}
    private MutationResult mutate(SqlParser.Statement statement, Snapshot snapshot, Snapshot pending, SqlValue[] args, CostMeter meter, QueryEnvironment environment) throws IOException {
        if (statement instanceof SqlParser.Reindex reindex && (reindex.target().equals("database") || reindex.target().equals("system"))) {
            if (explicit) { failed = true; throw new SqlException("25001", "REINDEX form cannot run inside a transaction block"); }
            String domain = reindex.name() == null ? "main" : name(reindex.name()); if (domain.equals("temp") || !domain.equals("main") && !snapshot.domains().containsKey(domain)) throw unsupported("unknown database for REINDEX"); snapshot.domain(domain); database.requireWritable(domain);
            List<String> domains = domain.equals("main") ? List.of("main", "temp") : List.of(domain);
            for (String selected : domains) { Snapshot before = pending.domain(selected); Snapshot after = Reindex.run(new SqlParser.Reindex(reindex.target(), null, reindex.concurrently()), before, environment, database.domainDatabase(session, selected), session.workMem()); if (before != after) pending = pending.withDomain(selected, after); }
            return new MutationResult(pending, emptyPlan());
        }
        List<SqlParser.Statement> drops = DomainRouting.splitDrop(statement, snapshot);
        if (drops.size() > 1) { for (SqlParser.Statement drop : drops) { MutationResult result = mutate(drop, snapshot, pending, args, meter, environment); pending = result.snapshot(); result.result().close(); } return new MutationResult(pending, emptyPlan()); }
        ViewWrites.Expanded expanded = ViewWrites.expand(statement, snapshot, environment);
        statement = expanded.statement();
        DomainRouting.Route route = DomainRouting.route(statement, snapshot);
        DomainRouting.validate(statement, snapshot, route.domain());
        database.requireWritable(route.domain());
        Snapshot local = snapshot.domain(route.domain()), staged = pending.domain(route.domain());
        Database previousDatabase = mutationDatabase; String previousDomain = mutationDomain; Snapshot previousPending = mutationPendingRoot;
        mutationPendingRoot = pending;
        mutationDatabase = database.domainDatabase(session, route.domain()); mutationDomain = route.domain();
        if (deferredReferences != null) deferredReferences.domain = mutationDomain;
        try {
            if (route.domain().equals("temp")) session.checkTempStorage();
            SqlParser.Name target = DomainRouting.target(route.statement());
            if ((route.domain().equals("main") || route.domain().equals("temp")) && (route.statement() instanceof SqlParser.CreateTable || route.statement() instanceof SqlParser.CreateView || route.statement() instanceof SqlParser.CreateSequence || route.statement() instanceof SqlParser.CreateIndex)) {
                Snapshot other = snapshot.domains().get(route.domain().equals("main") ? "temp" : "main");
                SqlParser.Name createdName = route.statement() instanceof SqlParser.CreateIndex index ? index.name() : target;
                if (createdName != null && !createdName.parts().isEmpty() && other != null && relationTaken(other, name(createdName))) {
                    boolean ifNotExists = switch (route.statement()) { case SqlParser.CreateTable create -> create.ifNotExists(); case SqlParser.CreateView create -> create.ifNotExists(); case SqlParser.CreateSequence create -> create.ifNotExists(); case SqlParser.CreateIndex create -> create.ifNotExists(); default -> false; };
                    if (ifNotExists) return new MutationResult(pending, emptyPlan());
                    throw new SqlException("42P07", "relation already exists in visible scope");
                }
            }
            if (route.statement() instanceof SqlParser.DropType drop && snapshot.domains().containsKey("temp"))
                for (SqlParser.Name type : drop.names()) if (snapshot.domain("temp").tables().values().stream().anyMatch(table -> table.columns().stream().anyMatch(column -> dependsOn(column.type(), name(type))))) throw new SqlException("2BP01", "type has temporary dependent columns");
            Snapshot original = local;
            boolean includeTempViews = route.domain().equals("main") && (statement instanceof SqlParser.DropTable || statement instanceof SqlParser.DropView || statement instanceof SqlParser.DropSequence || statement instanceof SqlParser.AlterTable || statement instanceof SqlParser.AlterSequence);
            java.util.Set<String> temporaryViews = includeTempViews ? snapshot.domain("temp").views().keySet() : java.util.Set.of();
            if (!temporaryViews.isEmpty()) {
                var visible = new TreeMap<>(local.views()); visible.putAll(snapshot.domain("temp").views()); local = local.withViews(visible);
                var stagedVisible = new TreeMap<>(staged.views()); stagedVisible.putAll(pending.domain("temp").views()); staged = staged.withViews(stagedVisible);
            }
            MutationResult result;
            if (route.statement() instanceof SqlParser.CreateTable create && create.asQuery() instanceof SqlParser.With with && writes(with)) {
                if (relationTaken(local, name(create.name()))) { if (create.ifNotExists()) return new MutationResult(pending, emptyPlan()); throw new SqlException("42P07", "table already exists"); }
                if (create.withData()) {
                    MutationResult input = writableStatement(with, snapshot, args, meter, environment);
                    pending = input.snapshot(); staged = pending.domain(route.domain());
                    result = new MutationResult(createFromPlan(create, staged, environment, input.result()), emptyPlan());
                } else result = new MutationResult(createFromPlan(create, staged, environment, read(with, snapshot, args, environment.shape())), emptyPlan());
            } else result = mutateLocal(route.statement(), local, staged, args, meter, environment, expanded.checks());
            if (route.statement() instanceof SqlParser.AlterTable alter)
                revalidateViews(local, result.snapshot(), name(alter.table()), local.views().containsKey(name(alter.table())) ? 1 : 0, false, environment);
            else if (route.statement() instanceof SqlParser.CreateView create && create.replace() && local.views().containsKey(name(create.name())))
                revalidateViews(local, result.snapshot(), name(create.name()), 1, true, environment);
            if (!temporaryViews.isEmpty()) {
                var mainViews = new TreeMap<>(result.snapshot().views()); var tempViews = new TreeMap<>(pending.domain("temp").views());
                for (String viewName : temporaryViews) { View edited = mainViews.remove(viewName); if (edited == null) tempViews.remove(viewName); else tempViews.put(viewName, edited); }
                Snapshot temp = pending.domain("temp"); Snapshot editedTemp = temp.withViews(tempViews);
                if (editedTemp != temp) pending = pending.withDomain("temp", editedTemp);
                Snapshot editedMain = result.snapshot().withViews(mainViews);
                if (result.snapshot() == local) editedMain = original;
                result = new MutationResult(editedMain, result.result());
            }
            if (route.domain().equals("temp") && route.statement() instanceof SqlParser.CreateTable create && result.snapshot() != staged && create.onCommit() != null)
                result = new MutationResult(result.snapshot().withTempAction(name(create.name()), create.onCommit()), result.result());
            return new MutationResult(result.snapshot() == staged ? pending : pending.withDomain(route.domain(), result.snapshot()), result.result());
        } finally { mutationDatabase = previousDatabase; mutationDomain = previousDomain; mutationPendingRoot = previousPending; }
    }
    private Snapshot writePhase(Snapshot local, CostMeter meter) {
        meter.pendingBytes(PendingMemory.bytes(mutationPendingRoot == null ? local : mutationPendingRoot.withDomain(mutationDomain, local)));
        return local;
    }
    private MutationResult mutateLocal(SqlParser.Statement statement, Snapshot snapshot, Snapshot pending, SqlValue[] args, CostMeter meter, QueryEnvironment environment, List<ViewWrites.Check> checks) throws IOException {
        ViewWrites.Checks viewChecks = new ViewWrites.Expanded(statement, checks).resolve(snapshot, environment);
        Returning returning = null;
        if (statement instanceof SqlParser.Merge merge) {
            MergePlan.Result execution = merge(merge, snapshot, pending, args, environment, viewChecks);
            return new MutationResult(execution.snapshot(), execution.rows());
        }
        long savedAffected = mutationAffected; mutationAffected = 0;
        try {
            Snapshot changed;
            if (statement instanceof SqlParser.Analyze analyze) changed = analyze(analyze, snapshot, args, environment);
            else if (statement instanceof SqlParser.CreateTable create) changed = create(create, snapshot, args, environment);
            else if (statement instanceof SqlParser.CreateView create) changed = createView(create, snapshot, environment);
            else if (statement instanceof SqlParser.DropView drop) changed = dropView(drop, snapshot);
            else if (statement instanceof SqlParser.CreateSequence create) changed = createSequence(create, snapshot, environment);
            else if (statement instanceof SqlParser.AlterSequence alter) changed = alterSequence(alter, snapshot, environment);
            else if (statement instanceof SqlParser.DropSequence drop) changed = dropSequence(drop, snapshot);
            else if (statement instanceof SqlParser.CreateIndex create) changed = createIndex(create, snapshot, args, environment);
            else if (statement instanceof SqlParser.DropIndex drop) changed = dropIndex(drop, snapshot);
            else if (statement instanceof SqlParser.Reindex command) {
                if (explicit && (command.concurrently() || command.target().equals("database") || command.target().equals("system"))) { failed = true; throw new SqlException("25001", "REINDEX form cannot run inside a transaction block"); }
                changed = Reindex.run(command, snapshot, environment, storageDatabase(), session.workMem());
            }
            else if (statement instanceof SqlParser.AlterTable alter) changed = alter(alter, snapshot, args, environment);
            else if (statement instanceof SqlParser.CreateType create) changed = createType(create, snapshot, environment);
            else if (statement instanceof SqlParser.DropType drop) changed = dropType(drop, snapshot);
            else if (statement instanceof SqlParser.DropTable drop) changed = drop(drop, snapshot);

            else if (statement instanceof SqlParser.Insert insert) {
                if (!insert.returning().isEmpty()) returning = new Returning(table(snapshot, insert.table()), insert.alias(), insert.returning(), insert.oldAlias(), insert.newAlias(), false, args, environment);
                changed = insert(insert, snapshot, pending, args, meter, returning, environment, viewChecks);
            } else if (statement instanceof SqlParser.Update update) {
                if (!update.returning().isEmpty()) returning = new Returning(table(snapshot, update.table()), update.alias(), update.returning(), update.oldAlias(), update.newAlias(), false, args, environment);
                changed = update(update, snapshot, pending, args, meter, returning, environment, viewChecks);
            } else if (statement instanceof SqlParser.Delete delete) {
                if (!delete.returning().isEmpty()) returning = new Returning(table(snapshot, delete.table()), delete.alias(), delete.returning(), delete.oldAlias(), delete.newAlias(), true, args, environment);
                changed = delete(delete, snapshot, pending, args, meter, returning, environment);
            } else throw unsupported("data-modifying CTE statement");
            long affected = statement instanceof SqlParser.Insert || statement instanceof SqlParser.Update
                || statement instanceof SqlParser.Delete || statement instanceof SqlParser.Analyze ? mutationAffected : -1;
            if (statement instanceof SqlParser.CreateTable create && create.asQuery() != null && create.withData() && changed != snapshot)
                affected = table(changed, create.name()).rows().size();
            return new MutationResult(changed, QueryPlan.counted(returning == null ? emptyPlan() : returning.finish(meter), affected));
        } catch (IOException | RuntimeException | Error failure) { if (returning != null) returning.close(); throw failure; }
        finally { mutationAffected = savedAffected; }
    }
private MutationResult writableStatement(SqlParser.With with, Snapshot snapshot, SqlValue[] args, CostMeter meter, QueryEnvironment environment) throws IOException {
    try (ForeignKeys.Deferred deferred = new ForeignKeys.Deferred()) {
        deferredReferences = deferred;
        MutationResult execution = writableWith(with, snapshot, args, meter, environment);
        try { deferred.validate(execution.snapshot()); return execution; }
        catch (IOException | RuntimeException | Error failure) { execution.result().close(); throw failure; }
    } finally { deferredReferences = null; }
}
    private MutationResult writableWith(SqlParser.With with, Snapshot snapshot, SqlValue[] args, CostMeter meter, QueryEnvironment environment) throws IOException {
        boolean measured = meter.writeMetrics != null;
        long started = meter.cost(); PlanNode estimate = null;
        if (measured) {
            boolean previous = environment.explainProbe(); environment.explainProbe(true);
            try (QueryPlan metadata = read(with, snapshot, args, environment)) { metadata.requireAnalyze(); estimate = metadata.explain(false); }
            finally { environment.explainProbe(previous); }
        }
        QueryEnvironment scope = environment.fork(); Snapshot pending = snapshot;
        java.util.Set<String> names = new java.util.HashSet<>();
        for (int i = 0; i < with.ctes().size(); i++) {
            SqlParser.Cte cte = with.ctes().get(i);
            if (!names.add(cte.name())) throw new SqlException("42712", "duplicate CTE name");
            SearchCycle.requireRecursive(cte, with.recursive());
            if (writes(cte.body())) {
                MutationResult execution = measuredMutation(cte.body(), snapshot, pending, args, meter, scope);
                pending = execution.snapshot();
                QueryPlan result = execution.result();
                CtePlan definition = new CtePlan(() -> result, cte.columns(), true).sideEffectsOnly(result.types().isEmpty());
                scope.cte(cte.name(), definition); definition.eager(meter);
            } else {
                QueryEnvironment bodyScope = scope.fork();
                if (with.recursive() && CteReferences.count(cte.body(), cte.name()) != 0) {
                    scope.cte(cte.name(), new CtePlan(() -> new RecursivePlan(cte, bodyScope), List.of(), true).correlated(bodyScope.correlated));
                    continue;
                }
                int references = CteReferences.count(with.body(), cte.name());
                boolean readReference = references != 0 && !writes(with.body());
                for (int j = i + 1; j < with.ctes().size(); j++) {
                    SqlParser.Statement body = with.ctes().get(j).body(); int count = CteReferences.count(body, cte.name());
                    references += count; if (count != 0 && !writes(body)) readReference = true;
                }
                boolean materialized = writes(cte.body()) || "materialized".equals(cte.materialization()) || references > 1 && !"not_materialized".equals(cte.materialization());
                CtePlan definition = new CtePlan(() -> read(cte.body(), snapshot, args, bodyScope), cte.columns(), materialized).correlated(bodyScope.correlated).inline(references == 1 && readReference ? cte.body() : null, bodyScope);
                scope.cte(cte.name(), definition);
                if (materialized) definition.eager(meter);
            }
        }
        QueryPlan result;
        if (writes(with.body())) {
            MutationResult body = measuredMutation(with.body(), snapshot, pending, args, meter, scope);
            pending = body.snapshot(); result = body.result();
        } else result = collectResult(read(with.body(), snapshot, args, scope), meter);
        if (measured) {
            List<PlanNode> children = new ArrayList<>();
            for (SqlParser.Cte cte : with.ctes()) children.add(scope.cte(cte.name()).actualDefinition(cte.name(), false));
            children.add(result.actualPlan(false));
            PlanNode actual = new PlanNode(estimate.label(), estimate.detail(), estimate.rows(), meter.cost() - started, children, estimate.lane());
            result = new CapturedPlan(result, estimate, actual);
        }
        return new MutationResult(pending, result);
    }
    private MutationResult measuredMutation(SqlParser.Statement statement, Snapshot snapshot, Snapshot pending, SqlValue[] args, CostMeter meter, QueryEnvironment scope) throws IOException {
        if (meter.writeMetrics == null) return mutate(statement, snapshot, pending, args, meter, scope);
        boolean previous = scope.explainProbe(); scope.explainProbe(true);
        QueryPlan metadata;
        try { metadata = read(statement, snapshot, args, scope); metadata.requireAnalyze(); }
        finally { scope.explainProbe(previous); }
        PlanMetrics outer = meter.writeMetrics; PlanNode outerSource = meter.writeSourceActual;
        meter.writeMetrics = new PlanMetrics(); meter.writeSourceActual = null;
        long started = meter.cost();
        try (metadata) {
            MutationResult result = mutate(statement, snapshot, pending, args, meter, scope);
            meter.writeMetrics.total(started, meter);
            if (statement instanceof SqlParser.Merge) return result;
            if (!(metadata instanceof DmlExplanation write)) { result.result().close(); throw unsupported("writable CTE actual checkpoints"); }
            return new MutationResult(result.snapshot(), new CapturedPlan(result.result(), write.explain(false), write.actualWrite(false, meter.writeMetrics, meter.writeSourceActual)));
        } finally { meter.writeMetrics = outer; meter.writeSourceActual = outerSource; }
    }
    private static QueryPlan collectResult(QueryPlan source, CostMeter meter) {
        RowSpool rows = new RowSpool(source.types(), 0, null, meter.memory());
        List<String> names = source.names(); List<SqlType> types = source.types();
        try (source) {
            Row row; while ((row = source.next(meter)) != null) rows.append(row.values());
            PlanNode estimate = meter.writeMetrics == null ? null : source.explain(false);
            PlanNode actual = meter.writeMetrics == null ? null : source.actualPlan(false);
            RowSpool.Cursor cursor = rows.cursor();
            return new QueryPlan() {
                public List<String> names() { return names; }
                public List<SqlType> types() { return types; }
                public void requireAnalyze() { if (actual == null) throw unsupported("uncaptured result checkpoints"); }
                public PlanNode explain(boolean verbose) { if (estimate == null) throw unsupported("uncaptured result metadata"); return estimate; }
                public PlanNode actualPlan(boolean verbose) { if (actual == null) throw unsupported("uncaptured result checkpoints"); return actual; }
                public Row next(CostMeter ignored) { List<SqlValue> values = cursor.next(); return values == null ? null : new Row(names, values); }
                public void close() { cursor.close(); rows.close(); }
            };
        } catch (RuntimeException | Error failure) { rows.close(); throw failure; }
    }
    String defaultCollation() { return CollationCompatibility.defaultName(explicit ? working : database.snapshot(session)); }
    int changeCollations(String defaultName, CostMeter meter) {
        if (failed) throw new SqlException("25P02", "transaction is aborted");
        if (explicit && readOnly) throw new SqlException("25006", "read-only transaction");
        boolean automatic = !explicit;
        if (automatic) { database.acquireWriter(session, meter); writer = true; }
        try {
            Snapshot snapshot = explicit ? working : database.snapshot(session); meter.collations(snapshot.collations());
            CollationMaintenance.Change change = defaultName == null ? CollationMaintenance.upgrade(snapshot, database, session, meter) : CollationMaintenance.setDefault(snapshot, defaultName, database.environment());
            if (explicit) working = change.snapshot(); else if (change.snapshot() != snapshot) database.publish(session, snapshot, TempTables.applyCommit(change.snapshot()));
            return change.upgraded();
        } catch (IOException failure) { throw storageError(failure); }
        finally { if (automatic) { writer = false; database.releaseWriter(session); } }
    }
    void fail(SqlException error) { if (explicit && !session.readHandle() && !error.sqlState().equals("25001")) failed = true; }
    boolean hasWriteTransaction() { return explicit && !readOnly; }
    void close() { rollback(); }

    private QueryPlan transaction(SqlParser.Transaction command, CostMeter meter) {
        String action = command.action();
        if (failed && !List.of("commit", "rollback", "rollback_to").contains(action)) throw new SqlException("25P02", "transaction is aborted");
        switch (action) {
            case "begin" -> {
                if (explicit) throw new SqlException("25001", "transaction already active");
                if (!command.readOnly()) { database.acquireWriter(session, meter); writer = true; }
                transactionPin = command.readOnly() ? database.pinSnapshot(session) : database.pinWriterSnapshot(session); base = working = transactionPin.snapshot(); explicit = true; readOnly = command.readOnly(); transactionHistory = sequenceHistory;
            }
            case "commit" -> {
                try {
                    if (explicit && !failed && !readOnly) { Snapshot committed = TempTables.applyCommit(working); if (committed != base) database.publish(session, base, committed); }
                    if (!failed && !readOnly) transactionHistory = sequenceHistory;
                } catch (RuntimeException | Error failure) {
                    try { rollback(); } catch (RuntimeException | Error cleanup) { failure.addSuppressed(cleanup); }
                    throw failure;
                }
                rollback();
            }
            case "rollback" -> rollback();
            case "savepoint" -> {
                requireTransaction(); working = PendingMemory.forget(working); savepoints.add(new Savepoint(command.name(), working, sequenceHistory));
            }
            case "release", "rollback_to" -> {
                requireTransaction();
                int found = savepoints.size() - 1;
                while (found >= 0 && !savepoints.get(found).name.equals(command.name())) found--;
                if (found < 0) throw new SqlException("3B001", "savepoint does not exist");
                if (action.equals("rollback_to")) { working = savepoints.get(found).snapshot; sequenceHistory = savepoints.get(found).history; failed = false; found++; }
                savepoints.subList(found, savepoints.size()).clear();
            }
            default -> throw unsupported("transaction command");
        }
        return emptyPlan();
    }
    private void requireTransaction() { if (!explicit) throw new SqlException("25P01", "no active transaction"); }
    private void rollback() {
        if (transactionHistory != null) sequenceHistory = transactionHistory; transactionHistory = null;
        explicit = false; readOnly = false; failed = false; working = base = null; savepoints.clear();
        if (transactionPin != null) { transactionPin.close(); transactionPin = null; }
        if (writer) { writer = false; database.releaseWriter(session); }
    }

    private Snapshot create(SqlParser.CreateTable command, Snapshot snapshot, SqlValue[] args, QueryEnvironment environment) throws IOException {
        if (command.asQuery() != null) return createAs(command, snapshot, args, environment);
        String name = name(command.name());
        if (relationTaken(snapshot, name)) {
            if (command.ifNotExists()) return snapshot;
            throw new SqlException("42P07", "table already exists: " + name);
        }
        if (command.onCommit() != null && !mutationDomain.equals("temp")) throw new SqlException("42P16", "ON COMMIT requires a temporary table");
        ScalarPlan.Resolver resolver = environment.resolver();
        List<Column> columns = new ArrayList<>();
        List<SqlParser.Constraint> constraints = new ArrayList<>(command.constraints());
        java.util.Set<String> reservedNames = new java.util.HashSet<>();
        for (SqlParser.Constraint constraint : command.constraints()) if (constraint.name() != null && (constraint.kind().equals("unique") || constraint.kind().equals("exclude"))) reservedNames.add(constraint.name());
        for (SqlParser.Column column : command.columns()) for (SqlParser.Constraint constraint : column.constraints()) if (constraint.name() != null && constraint.kind().equals("unique")) reservedNames.add(constraint.name());
        for (SqlParser.Column syntax : command.columns()) {
            if (columns.stream().anyMatch(c -> c.name.equals(syntax.name()))) throw new SqlException("42701", "duplicate column: " + syntax.name());
            SqlType serialType = Sequences.serialType(syntax.type());
            SqlType type = serialType == null ? resolver.type(syntax.type()) : serialType;
            storable(type);
            if (syntax.collation() != null) { if (!CollationDerivation.collatable(type)) throw SqlException.typeMismatch("COLLATE requires text or text array"); snapshot = snapshot.useCollation(storageDatabase().environment().collation(syntax.collation())); }
            boolean notNull = serialType != null; SqlParser.Expr defaultValue = null;
            for (SqlParser.Constraint constraint : syntax.constraints()) {
                switch (constraint.kind()) {
                    case "not_null" -> notNull = true;
                    case "null" -> { }
                    case "default" -> { if (defaultValue != null) throw new SqlException("42601", "multiple defaults"); defaultValue = constraint.expression(); }
                    case "check", "foreign_key" -> constraints.add(constraint);
                    case "primary_key", "unique" -> constraints.add(new SqlParser.Constraint(constraint.kind(), constraint.name(), List.of(syntax.name()), null, null, List.of(), null, null));
                    default -> throw unsupported("constraint " + constraint.kind());
                }
            }
            if (serialType != null || syntax.identity() != null) {
                if (defaultValue != null || serialType != null && syntax.identity() != null) throw new SqlException("42601", "multiple defaults for serial column");
                if (!type.isInteger()) throw new SqlException("22023", "identity column must have integer type");
                String sequenceName = syntax.identity() == null || syntax.identity().sequenceName() == null ? Sequences.autoName(snapshot, name, syntax.name(), "seq", reservedNames) : syntax.identity().sequenceName();
                if (sequenceName.startsWith("jed_") && syntax.identity() != null && syntax.identity().sequenceName() != null) throw new SqlException("42939", "reserved sequence name");
                if (sequenceName.equals(name) || relationTaken(snapshot, sequenceName)) throw new SqlException("42P07", "sequence name already exists");
                List<SqlParser.SequenceOption> options = new ArrayList<>(syntax.identity() == null ? List.of() : syntax.identity().options());
                options.add(new SqlParser.SequenceOption("as", null, new SqlParser.TypeName(type == SqlType.I16 ? "i16" : type == SqlType.I32 ? "i32" : "i64", -1, -1, 0)));
                Sequence definition = Sequences.definition(sequenceName, null, options, resolver);
                Sequence sequence = new Sequence(sequenceName, definition.increment(), definition.minimum(), definition.maximum(), definition.start(), definition.cache(), definition.last(), definition.cycle(), definition.called(), name, columns.size());
                notNull = true;
                snapshot = snapshot.putSequence(sequence); environment.sequences().install(mutationDomain, sequence);
                defaultValue = Sequences.defaultExpression(sequenceName);
            }
            defaultValue = defaultValue(defaultValue, type, environment);
            columns.add(new Column(syntax.name(), type, notNull, defaultValue, syntax.identity() == null ? 0 : syntax.identity().always() ? 2 : 1, syntax.collation() == null ? CollationDerivation.collatable(type) ? CollationCompatibility.defaultName(snapshot) : "C" : syntax.collation()));
        }
        int[] primary = new int[0]; List<int[]> unique = new ArrayList<>();
        for (SqlParser.Constraint constraint : constraints) {
            if (constraint.kind().equals("check") || constraint.kind().equals("foreign_key") || constraint.kind().equals("exclude")) continue;
            if (!constraint.kind().equals("primary_key") && !constraint.kind().equals("unique")) throw unsupported("constraint " + constraint.kind());
            int[] indexes = new int[constraint.columns().size()];
            for (int i = 0; i < indexes.length; i++) {
                int found = -1;
                for (int c = 0; c < columns.size(); c++) if (columns.get(c).name.equals(constraint.columns().get(i))) found = c;
                if (found < 0) throw new SqlException("42703", "constraint column does not exist");
                indexes[i] = found;
                for (int j = 0; j < i; j++) if (indexes[j] == found) throw new SqlException("42701", "duplicate constraint column");
            }
            if (constraint.kind().equals("primary_key")) {
                if (primary.length != 0) throw new SqlException("42P16", "multiple primary keys");
                primary = indexes;
                for (int i : primary) { Column c = columns.get(i); columns.set(i, new Column(c.name, c.type, true, c.defaultValue, c.identity, c.collation)); }
            } else unique.add(indexes);
        }
        List<SqlType> types = columns.stream().map(Column::type).toList();
        Table table = new Table(name, columns, primary, unique, PersistentTree.empty(storageDatabase().pageSize(), new TableLeafCodec(types, primary), storageDatabase().pageCodec()), 0);
        List<int[]> distinctUnique = new ArrayList<>();
        for (SqlParser.Constraint constraint : constraints) if (constraint.kind().equals("unique")) {
            int[] key = constraint.columns().stream().mapToInt(table::column).toArray();
            if (Arrays.equals(primary, key) || distinctUnique.stream().anyMatch(existing -> Arrays.equals(existing, key))) continue;
            distinctUnique.add(key);
            table = addUniqueIndex(table, constraint.name(), key, snapshot, constraints);
        }
        table = new Table(name, columns, primary, distinctUnique, table.rows(), 0, table.checks(), table.indexes(), table.foreignKeys(), table.exclusions(), table.statistics());
        List<SqlParser.Constraint> checkSyntax = constraints.stream().filter(c -> c.kind().equals("check")).sorted(java.util.Comparator.comparingInt(c -> ((SqlParser.StoredExpression)c.expression()).position())).toList();
        table = withChecks(table, checkSyntax, snapshot, environment);
        for (SqlParser.Constraint constraint : constraints) if (constraint.kind().equals("foreign_key")) table = ForeignKeys.define(table, constraint, snapshot);
        for (SqlParser.Constraint constraint : constraints) if (constraint.kind().equals("exclude")) table = Exclusions.define(table, constraint, snapshot, environment, database);
        return snapshot.put(table);
    }

    private Table addUniqueIndex(Table table, String requestedName, int[] ordinals, Snapshot snapshot, List<SqlParser.Constraint> declarations) {
        String name = requestedName == null ? Sequences.autoName(snapshot.put(table), table.name(), String.join("_", Arrays.stream(ordinals).mapToObj(i -> table.columns().get(i).name()).toList()), "key") : requestedName;
        if (table.checks().stream().anyMatch(check -> check.name().equals(name)) || declarations.stream().anyMatch(c -> c.kind().equals("check") && name.equals(c.name()))) throw new SqlException("42710", "constraint already exists: " + name);
        if (relationTaken(snapshot.put(table), name)) throw new SqlException("42P07", "relation already exists: " + name);
        List<IndexKey> keys = new ArrayList<>();
        for (int ordinal : ordinals) {
            SqlType type = table.columns().get(ordinal).type();
            if (!Indexes.keyable(type)) throw unsupported("unique key type " + type);
            keys.add(new IndexKey(ordinal, null, type, false, false, table.columns().get(ordinal).collation()));
        }
        List<Index> indexes = new ArrayList<>(table.indexes());
        indexes.add(new Index(name, keys, true, true, null, 0, PersistentTree.empty(storageDatabase().pageSize(), TableLeafCodec.index(), storageDatabase().pageCodec())));
        indexes.sort((a, b) -> Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name())));
        return table.withIndexes(indexes);
    }

    private static Table withChecks(Table table, List<SqlParser.Constraint> syntax, Snapshot snapshot, QueryEnvironment environment) {
        List<Check> checks = new ArrayList<>(table.checks());
        List<String> references = new ArrayList<>();
        List<List<HostDependencies.Dependency>> dependencies = new ArrayList<>();
        for (SqlParser.Constraint constraint : syntax) {
            DefaultExpression.validateCheck(constraint.expression());
            ScalarPlan.Resolver resolver = environment.resolver(table.bindings(null), new ScalarPlan.Frame());
            ScalarPlan.Expression expression = resolver.resolve(constraint.expression(), SqlType.BOOLEAN);
            if (expression.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("argument of CHECK must be boolean");
            String referenced = null; int count = 0;
            for (int c = 0; c < table.columns().size(); c++) if (resolver.touched()[c]) { referenced = table.columns().get(c).name(); count++; }
            references.add(count == 1 ? referenced : null);
            dependencies.add(HostDependencies.collect(constraint.expression(), resolver, false));
        }
        for (int i = 0; i < syntax.size(); i++) {
            SqlParser.Constraint constraint = syntax.get(i); String name = constraint.name();
            if (name == null) {
                for (int suffix = 0; ; suffix++) {
                    name = Sequences.objectName(table.name(), references.get(i), "check" + (suffix == 0 ? "" : suffix));
                    if (!checkNameTaken(name, checks, snapshot)) break;
                }
            } else {
                for (Check existing : checks) if (existing.name().equals(name)) throw new SqlException("42710", "constraint already exists: " + name);
                for (Index existing : table.indexes()) if (existing.constraint() && existing.name().equals(name)) throw new SqlException("42710", "constraint already exists: " + name);
            }
            checks.add(new Check(name, (SqlParser.StoredExpression)constraint.expression(), dependencies.get(i)));
        }
        checks.sort((a, b) -> Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name())));
        return new Table(table.name(), table.columns(), table.primaryKey(), table.unique(), table.rows(), table.nextRowId(), checks, table.indexes(), table.foreignKeys(), table.exclusions(), table.statistics());
    }
    private static boolean checkNameTaken(String name, List<Check> checks, Snapshot snapshot) {
        for (Check check : checks) if (check.name().equals(name)) return true;
        for (Table table : snapshot.tables().values()) for (Check check : table.checks()) if (check.name().equals(name)) return true;
        return false;
    }
    static final class CheckEvaluator {
        final ScalarPlan.Frame frame = new ScalarPlan.Frame();
        final List<ScalarPlan.Expression> expressions = new ArrayList<>();
        final List<Check> checks;
        CheckEvaluator(Table table, QueryEnvironment environment) {
            checks = table.checks();
            ScalarPlan.Resolver resolver = environment.resolver(table.bindings(null), frame);
            for (Check check : checks) {
                StoredDefinitions.require(check.expression(), "check constraint \"" + check.name() + "\" of table \"" + table.name() + "\"");
                expressions.add(resolver.resolve(check.expression(), SqlType.BOOLEAN));
                HostDependencies.require(check.hostDependencies(), environment.meter().environment());
                if (!HostDependencies.collect(check.expression(), resolver, false).equals(check.hostDependencies())) throw new SqlException("XX002", "host dependency changed for check constraint " + check.name());
            }
        }
        void evaluate(List<SqlValue> values, CostMeter meter) {
            frame.values = values;
            for (int i = 0; i < expressions.size(); i++) {
                SqlValue value = expressions.get(i).run(meter);
                if (!value.isNull() && !value.asBoolean()) throw new SqlException("23514", "check constraint violated: " + checks.get(i).name());
            }
        }
    }

    private static SqlParser.Expr defaultValue(SqlParser.Expr syntax, SqlType type, QueryEnvironment environment) {
        if (syntax == null) return null;
        DefaultExpression.validate(syntax);
        SqlParser.Literal literal = DefaultExpression.literal(syntax);
        boolean nullLiteral = literal != null && literal.kind() == Lexer.Kind.WORD && literal.text().equals("null");
        if ((type.kind() == SqlType.Kind.ARRAY || type.kind() == SqlType.Kind.RANGE) && !nullLiteral)
            throw unsupported("non-NULL default for array or range column");
        SqlParser.Expr expression = literal != null ? literal : DefaultExpression.unwrap(syntax);
        if (literal != null && literal.kind() == Lexer.Kind.STRING && (type.isInteger() || type.kind() == SqlType.Kind.DECIMAL || type == SqlType.BOOLEAN))
            throw SqlException.typeMismatch("text literal default cannot be assigned to " + type);
        ScalarPlan.Expression value = assignment(environment.resolver(), expression, type);
        if (literal != null && literal.kind() == Lexer.Kind.STRING && type == SqlType.DATE && Temporal.clockDate(literal.text())) return syntax;
        if (literal != null && !type.isComposite()) return new SqlParser.StoredValue(value.run(environment.meter()));
        return nullLiteral ? null : syntax;
    }

    private Snapshot createAs(SqlParser.CreateTable command, Snapshot snapshot, SqlValue[] args, QueryEnvironment environment) throws IOException {
        String name = name(command.name());
        if (name.startsWith("jed_")) throw new SqlException("42939", "reserved relation name");

        return createFromPlan(command, snapshot, environment, read(command.asQuery(), snapshot, args, environment));
    }
    private Snapshot createFromPlan(SqlParser.CreateTable command, Snapshot snapshot, QueryEnvironment environment, QueryPlan query) throws IOException {
        String name = name(command.name());
        try (query) {
            if (relationTaken(snapshot, name)) {
                if (command.ifNotExists()) return snapshot;
                throw new SqlException("42P07", "table already exists: " + name);
            }
            if (command.columnNames().size() > query.types().size()) throw new SqlException("42601", "too many column names were specified");
            if (command.onCommit() != null && !command.temporary()) throw new SqlException("42P16", "ON COMMIT requires a temporary table");

            List<Column> columns = new ArrayList<>();
            for (int i = 0; i < query.types().size(); i++) {
                String column = i < command.columnNames().size() ? command.columnNames().get(i) : query.names().get(i);
                if (columns.stream().anyMatch(c -> c.name().equals(column))) throw new SqlException("42701", "duplicate column: " + column);
                SqlType type = query.types().get(i);
                if (type == SqlType.UNKNOWN) type = SqlType.TEXT;
                String collation = "C";
                if (CollationDerivation.collatable(type)) {
                    CollationDerivation derivation = query.collations().get(i);
                    collation = derivation.conflict() ? derivation.require() : derivation.name() == null ? CollationCompatibility.defaultName(snapshot) : derivation.require();
                    snapshot = snapshot.useCollation(storageDatabase().environment().collation(collation));
                }
                columns.add(new Column(column, type, false, null, 0, collation));
            }
            for (Column column : columns) storable(column.type());
            Table table = new Table(name, columns, new int[0], List.of(), PersistentTree.empty(storageDatabase().pageSize(), new TableLeafCodec(columns.stream().map(Column::type).toList(), new int[0]), storageDatabase().pageCodec()), 0);
            if (!command.withData()) return snapshot.put(table);
            PersistentTree tree = table.rows(); long rowId = 0; Row row;
            while ((row = query.next(environment.meter())) != null) {
                List<SqlValue> values = new ArrayList<>();
                for (int c = 0; c < columns.size(); c++) values.add(SqlCasts.cast(row.values().get(c), columns.get(c).type()));
                byte[] key = key(table, values, rowId, storageDatabase().environment());
                tree = tree.put(Encoding.entry(key, values, null, null, storageDatabase().pageLayout(), environment.meter(), table.primaryKey()));
                if (rowId == Long.MAX_VALUE) throw SqlException.numericOverflow();
                rowId++;
            }
            if (environment.meter().writeMetrics != null) {
                environment.meter().writeSourceActual = query.actualPlan(false);
                environment.meter().writeAffected = rowId;
            }
            return snapshot.put(table.changed(tree, rowId));
        }
    }
    private static void storable(SqlType type) {
        if (type.kind() == SqlType.Kind.RECORD || type.kind() == SqlType.Kind.ARRAY && type.element().kind() == SqlType.Kind.RECORD)
            throw new SqlException("42P16", "column has pseudo-type record");
        if (type.kind() == SqlType.Kind.ARRAY && (type.element().kind() == SqlType.Kind.ARRAY || type.element().kind() == SqlType.Kind.RANGE))
            throw unsupported("column type " + type);
    }

    private Snapshot alter(SqlParser.AlterTable command, Snapshot snapshot, SqlValue[] args, QueryEnvironment environment) throws IOException {
        String name = name(command.table());
        Table table = snapshot.tables().get(name);
        if (snapshot.views().containsKey(name)) return alterView(command, snapshot, environment);
        if (command.viewOnly() && relationTaken(snapshot, name)) throw new SqlException("42809", "relation is not a view: " + name);
        if (table == null) { if (relationTaken(snapshot, name)) throw new SqlException("42809", "relation is not a table"); if (command.ifExists()) return snapshot; throw new SqlException("42P01", "table does not exist: " + name); }
        Table original = table; boolean rewriteRows = false, validateRows = false, freshRowIds = false;
        java.util.Set<String> addedChecks = new java.util.HashSet<>();
        List<java.util.function.UnaryOperator<List<SqlValue>>> conversions = new ArrayList<>();
        for (SqlParser.AlterAction action : command.actions()) {
            StoredDefinitions.alter(table, action);
            List<Column> columns = new ArrayList<>(table.columns());
            switch (action.action()) {
                case "set_default", "drop_default", "set_not_null", "drop_not_null", "rename_column" -> {
                    int index = table.column(action.name()); Column old = columns.get(index);
                    if (old.identity() != 0 && List.of("set_default", "drop_default", "drop_not_null").contains(action.action())) throw new SqlException("42601", "identity column cannot change its default or nullability");
                    Column replacement = switch (action.action()) {
                        case "set_default" -> new Column(old.name(), old.type(), old.notNull(), defaultValue(action.expression(), old.type(), environment), old.identity(), old.collation());
                        case "drop_default" -> new Column(old.name(), old.type(), old.notNull(), null, old.identity(), old.collation());
                        case "set_not_null" -> new Column(old.name(), old.type(), true, old.defaultValue(), old.identity(), old.collation());
                        case "drop_not_null" -> {
                            for (int key : table.primaryKey()) if (key == index) throw new SqlException("42P16", "primary key column cannot drop NOT NULL");
                            yield new Column(old.name(), old.type(), false, old.defaultValue(), old.identity(), old.collation());
                        }
                        default -> {
                            if (columns.stream().anyMatch(c -> c.name().equals(action.newName()))) throw new SqlException("42701", "duplicate column: " + action.newName());
                            yield new Column(action.newName(), old.type(), old.notNull(), old.defaultValue(), old.identity(), old.collation());
                        }
                    };
                    if (action.action().equals("set_not_null") && !old.notNull()) validateRows = true;
                    columns.set(index, replacement);
                    List<Check> renamed = table.checks();
                    if (action.action().equals("rename_column")) {
                        renamed = new ArrayList<>();
                        for (Check check : table.checks()) renamed.add(new Check(check.name(), SqlParser.renameColumn(check.expression(), table.name(), old.name(), action.newName(), false), check.hostDependencies()));
                    }
                    List<Index> indexes = action.action().equals("rename_column") ? Indexes.rename(table, old.name(), action.newName(), false) : table.indexes();
                    table = new Table(table.name(), columns, table.primaryKey(), table.unique(), table.rows(), table.nextRowId(), renamed, indexes, table.foreignKeys(), table.exclusions(), table.statistics());
                    if (action.action().equals("rename_column")) snapshot = ViewRenames.apply(snapshot, table.name(), table.name(), action.name(), action.newName(), columns.stream().map(c -> c.name().equals(action.newName()) ? action.name() : c.name()).toList());
                }
                case "rename_table" -> {
                    if (relationTaken(snapshot, action.newName())) throw new SqlException("42P07", "relation already exists: " + action.newName());
                    for (Sequence sequence : snapshot.sequences().values()) if (table.name().equals(sequence.owner())) snapshot = snapshot.putSequence(new Sequence(sequence.name(), sequence.increment(), sequence.minimum(), sequence.maximum(), sequence.start(), sequence.cache(), sequence.last(), sequence.cycle(), sequence.called(), action.newName(), sequence.ownerColumn()));
                    List<Check> renamed = new ArrayList<>();
                    for (Check check : table.checks()) renamed.add(new Check(check.name(), SqlParser.renameColumn(check.expression(), table.name(), table.name(), action.newName(), true), check.hostDependencies()));
                    snapshot = ForeignKeys.renameParent(snapshot.put(table), table.name(), action.newName()).renameTempAction(table.name(), action.newName());
                    snapshot = ViewRenames.apply(snapshot, table.name(), action.newName(), null, null, columns.stream().map(Column::name).toList());
                    table = new Table(action.newName(), columns, table.primaryKey(), table.unique(), table.rows(), table.nextRowId(), renamed, Indexes.rename(table, table.name(), action.newName(), true), snapshot.tables().get(table.name()).foreignKeys(), table.exclusions(), table.statistics());
                }
                case "add_column" -> {
                    SqlParser.Column syntax = action.column();
                    if (columns.stream().anyMatch(c -> c.name().equals(syntax.name()))) {
                        if (action.ifExists()) continue;
                        throw new SqlException("42701", "duplicate column: " + syntax.name());
                    }
                    SqlParser.CreateTable declaration = new SqlParser.CreateTable(command.table(), false, false, List.of(syntax), List.of(), null, List.of(), null, true);
                    Snapshot declarationState = create(declaration, snapshot.drop(name), args, environment);
                    Table added = declarationState.tables().get(name);
                    snapshot = new Snapshot(snapshot.tables(), snapshot.types(), snapshot.views(), snapshot.sequences(), snapshot.version(), declarationState.collations());
                    for (Sequence sequence : declarationState.sequences().values()) if (!snapshot.sequences().containsKey(sequence.name())) {
                        Sequence owned = new Sequence(sequence.name(), sequence.increment(), sequence.minimum(), sequence.maximum(), sequence.start(), sequence.cache(), sequence.last(), sequence.cycle(), sequence.called(), name, columns.size());
                        snapshot = snapshot.putSequence(owned); environment.sequences().install(mutationDomain, owned);
                    }
                    Column column = added.columns().getFirst(); columns.add(column);
                    int[] primary = table.primaryKey();
                    if (added.primaryKey().length != 0) {
                        if (primary.length != 0) throw new SqlException("42P16", "multiple primary keys");
                        primary = new int[]{columns.size() - 1};
                    }
                    List<int[]> unique = new ArrayList<>(table.unique());
                    for (int[] ignored : added.unique()) unique.add(new int[]{columns.size() - 1});
                    List<Index> indexes = new ArrayList<>(table.indexes());
                    for (Index index : added.indexes()) {
                        List<IndexKey> keys = new ArrayList<>();
                        for (IndexKey key : index.keys()) keys.add(new IndexKey(columns.size() - 1, null, key.type(), key.descending(), key.nullsFirst(), key.collation()));
                        indexes.add(new Index(index.name(), keys, index.unique(), index.constraint(), index.predicate(), index.kind(), index.entries(), index.timezoneDependencies(), index.gist(), index.hostDependencies()));
                    }
                    indexes.sort((a, b) -> Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name())));
                    List<ForeignKey> foreignKeys = new ArrayList<>(table.foreignKeys());
                    for (ForeignKey key : added.foreignKeys()) foreignKeys.add(new ForeignKey(key.name(), key.local().stream().map(i -> columns.size() - 1).toList(), key.parent(), key.referenced(), key.onDelete(), key.onUpdate(), key.matchFull()));
                    foreignKeys.sort((a, b) -> Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name())));
                    Table rewritten = new Table(table.name(), columns, primary, unique, PersistentTree.empty(storageDatabase().pageSize(), new TableLeafCodec(columns.stream().map(Column::type).toList(), primary), storageDatabase().pageCodec()), table.nextRowId(), table.checks(), indexes, foreignKeys, table.exclusions(), table.statistics());
                    List<Check> combinedChecks = new ArrayList<>(rewritten.checks()); combinedChecks.addAll(added.checks()); for (Check check : added.checks()) addedChecks.add(check.name());
                    combinedChecks.sort((a, b) -> Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name())));
                    rewritten = new Table(rewritten.name(), rewritten.columns(), rewritten.primaryKey(), rewritten.unique(), rewritten.rows(), rewritten.nextRowId(), combinedChecks, rewritten.indexes(), rewritten.foreignKeys(), rewritten.exclusions(), rewritten.statistics());
                    ScalarPlan.Expression value = assignment(environment.resolver(), defaultOf(column), column.type());
                    conversions.add(row -> { List<SqlValue> values = new ArrayList<>(row); values.add(value.run(environment.meter())); return values; });
                    table = rewritten; rewriteRows = true;
                }
                case "drop_column", "drop_column_cascade" -> {
                    int index = -1;
                    for (int i = 0; i < columns.size(); i++) if (columns.get(i).name().equals(action.name())) index = i;
                    if (index < 0) { if (action.ifExists()) continue; throw new SqlException("42703", "column does not exist: " + action.name()); }
                    for (Exclusion exclusion : table.exclusions()) if (!exclusion.columns().contains(index)) {
                        Index backing = table.indexes().stream().filter(i -> i.name().equals(exclusion.index())).findFirst().orElseThrow();
                        if (backing.predicate() != null) {
                            ScalarPlan.Resolver check = environment.resolver(table.bindings(null), new ScalarPlan.Frame()); check.resolve(backing.predicate(), SqlType.BOOLEAN);
                            if (check.touched()[index] && !action.action().endsWith("_cascade")) throw new SqlException("2BP01", "column has dependent exclusion predicate");
                        }
                    }
                    boolean dropPrimary = false; for (int key : table.primaryKey()) dropPrimary |= key == index;
                    if (dropPrimary) throw unsupported("dropping a primary-key column");
                    String tableName = table.name();
                    List<String> dependentViews = snapshot.views().values().stream().filter(view -> view.dependencies().stream().anyMatch(dep -> dep.kind() == 0 && dep.name().equals(tableName) && dep.columns().contains(action.name()))).map(View::name).toList();
                    if (!dependentViews.isEmpty()) {
                        if (!action.action().endsWith("_cascade")) throw new SqlException("2BP01", "column has dependent views");
                        snapshot = dropView(new SqlParser.DropView(dependentViews.stream().map(n -> new SqlParser.Name(List.of(n))).toList(), false, true), snapshot);
                    }
                    List<Check> checks = new ArrayList<>();
                    for (Check check : table.checks()) {
                        ScalarPlan.Resolver resolver = environment.resolver(table.bindings(null), new ScalarPlan.Frame());
                        resolver.resolve(check.expression(), SqlType.BOOLEAN);
                        if (!resolver.touched()[index]) checks.add(check);
                    }
                    for (Sequence sequence : snapshot.sequences().values()) if (tableName.equals(sequence.owner())) {
                        if (sequence.ownerColumn() == index) {
                            List<String> users = snapshot.views().values().stream().filter(view -> view.dependencies().stream().anyMatch(dep -> dep.kind() == 2 && dep.name().equals(sequence.name()))).map(View::name).toList();
                            if (!users.isEmpty()) { if (!action.action().endsWith("_cascade")) throw new SqlException("2BP01", "owned sequence has dependent views"); snapshot = dropView(new SqlParser.DropView(users.stream().map(n -> new SqlParser.Name(List.of(n))).toList(), false, true), snapshot); }
                            snapshot = snapshot.dropSequence(sequence.name()); environment.sequences().discard(mutationDomain, sequence.name());
                        }
                        else if (sequence.ownerColumn() > index) snapshot = snapshot.putSequence(new Sequence(sequence.name(), sequence.increment(), sequence.minimum(), sequence.maximum(), sequence.start(), sequence.cache(), sequence.last(), sequence.cycle(), sequence.called(), sequence.owner(), sequence.ownerColumn() - 1));
                    }
                    snapshot = ViewRenames.dropColumn(snapshot, table.name(), index);
                    columns.remove(index); int removed = index;
                    snapshot = ForeignKeys.dropColumn(snapshot.put(table), table.name(), removed, action.action().endsWith("_cascade"));
                    table = table.withForeignKeys(snapshot.tables().get(table.name()).foreignKeys());
                    int[] primary = dropPrimary ? new int[0] : Arrays.stream(table.primaryKey()).map(key -> key > removed ? key - 1 : key).toArray();
                    freshRowIds |= dropPrimary;
                    List<int[]> unique = new ArrayList<>();
                    for (int[] key : table.unique()) if (Arrays.stream(key).noneMatch(c -> c == removed)) unique.add(Arrays.stream(key).map(c -> c > removed ? c - 1 : c).toArray());
                    conversions.add(row -> { List<SqlValue> values = new ArrayList<>(row); values.remove(removed); return values; });
                    table = new Table(table.name(), columns, primary, unique, table.rows(), table.nextRowId(), checks, Indexes.dropColumn(table, removed, environment), table.foreignKeys(), table.exclusions(), Map.of()); rewriteRows = true;
                    java.util.Set<String> keptIndexes = new java.util.HashSet<>(); for (Index kept : table.indexes()) keptIndexes.add(kept.name());
                    table = table.withExclusions(table.exclusions().stream().filter(e -> keptIndexes.contains(e.index())).map(e -> new Exclusion(e.name(), e.index(), e.columns().stream().map(c -> c > removed ? c - 1 : c).toList(), e.operators())).toList());
                }
                case "type" -> {
                    int index = table.column(action.name()); Column old = columns.get(index);

                    SqlType type = environment.resolver().type(action.type()); storable(type);
                    if (old.identity() != 0 && !type.isInteger()) throw new SqlException("22023", "identity column must have integer type");
                    ScalarPlan.Frame frame = new ScalarPlan.Frame(); ScalarPlan.Resolver resolver = environment.resolver(table.bindings(null), frame);
                    SqlParser.Expr input = action.expression() == null ? new SqlParser.Name(List.of(old.name())) : action.expression();
                    ScalarPlan.Expression conversion = resolver.resolve(new SqlParser.Cast(input, action.type()), type);
                    SqlParser.Expr defaultExpression = old.defaultValue();
                    if (defaultExpression instanceof SqlParser.StoredValue value) defaultExpression = new SqlParser.StoredValue(environment.resolver().resolve(new SqlParser.Cast(value, action.type()), type).run(environment.meter()));
                    else if (defaultExpression != null) defaultExpression = defaultValue(defaultExpression, type, environment);
                    columns.set(index, new Column(old.name(), type, old.notNull(), defaultExpression, old.identity(), old.collation()));
                    if (old.identity() != 0) {
                        SqlParser.Call call = (SqlParser.Call)DefaultExpression.unwrap(old.defaultValue());
                        String sequenceName = Sequences.name(((SqlParser.Literal)call.args().getFirst()).text()); Sequence sequence = snapshot.sequences().get(sequenceName);
                        long oldMin = old.type() == SqlType.I16 ? Short.MIN_VALUE : old.type() == SqlType.I32 ? Integer.MIN_VALUE : Long.MIN_VALUE;
                        long oldMax = old.type() == SqlType.I16 ? Short.MAX_VALUE : old.type() == SqlType.I32 ? Integer.MAX_VALUE : Long.MAX_VALUE;
                        long newMin = type == SqlType.I16 ? Short.MIN_VALUE : type == SqlType.I32 ? Integer.MIN_VALUE : Long.MIN_VALUE;
                        long newMax = type == SqlType.I16 ? Short.MAX_VALUE : type == SqlType.I32 ? Integer.MAX_VALUE : Long.MAX_VALUE;
                        List<SqlParser.SequenceOption> options = List.of(sequenceType(type), new SqlParser.SequenceOption("minimum", sequence.minimum() == oldMin ? newMin : sequence.minimum(), null), new SqlParser.SequenceOption("maximum", sequence.maximum() == oldMax ? newMax : sequence.maximum(), null));
                        Sequence edited = Sequences.definition(sequenceName, sequence, options, environment.resolver()); snapshot = snapshot.putSequence(edited); environment.sequences().install(mutationDomain, edited);
                    }
                    conversions.add(row -> { frame.values = row; List<SqlValue> values = new ArrayList<>(row); values.set(index, conversion.run(environment.meter())); return values; });
                    table = new Table(table.name(), columns, table.primaryKey(), table.unique(), table.rows(), table.nextRowId(), table.checks(), table.indexes(), table.foreignKeys(), table.exclusions(), Map.of()); rewriteRows = true;
                    for (int key : table.primaryKey()) if (!Indexes.keyable(table.columns().get(key).type())) throw unsupported("primary key type " + table.columns().get(key).type());

                }
                case "add_identity", "set_identity", "drop_identity" -> {
                    int index = table.column(action.name()); Column column = columns.get(index);
                    if (action.action().equals("add_identity")) {
                        if (!column.type().isInteger()) throw new SqlException("22023", "identity column must have integer type");
                        String sequenceName = action.identity().sequenceName() == null ? Sequences.autoName(snapshot, table.name(), column.name()) : action.identity().sequenceName();
                        if (sequenceName.startsWith("jed_") && action.identity().sequenceName() != null) throw new SqlException("42939", "reserved sequence name");
                        if (relationTaken(snapshot, sequenceName)) throw new SqlException("42P07", "sequence name already exists");
                        List<SqlParser.SequenceOption> options = new ArrayList<>(action.identity().options()); options.add(sequenceType(column.type()));
                        Sequence definition = Sequences.definition(sequenceName, null, options, environment.resolver());
                        if (!column.notNull() || column.identity() != 0 || column.defaultValue() != null) throw new SqlException("55000", "column must be NOT NULL without default or identity");
                        Sequence owned = new Sequence(sequenceName, definition.increment(), definition.minimum(), definition.maximum(), definition.start(), definition.cache(), definition.last(), definition.cycle(), definition.called(), table.name(), index);
                        snapshot = snapshot.putSequence(owned); environment.sequences().install(mutationDomain, owned);
                        columns.set(index, new Column(column.name(), column.type(), true, Sequences.defaultExpression(sequenceName), action.identity().always() ? 2 : 1, column.collation()));
                    } else {
                        if (column.identity() == 0) { if (action.action().equals("drop_identity") && action.ifExists()) continue; throw new SqlException("55000", "column is not an identity column"); }
                        SqlParser.Call call = (SqlParser.Call)DefaultExpression.unwrap(column.defaultValue());
                        String sequenceName = Sequences.name(((SqlParser.Literal)call.args().getFirst()).text());
                        Sequence sequence = snapshot.sequences().get(sequenceName);
                        if (sequence == null) throw new SqlException("XX001", "identity sequence is missing");
                        if (action.action().equals("drop_identity")) {
                            for (View view : snapshot.views().values()) if (view.dependencies().stream().anyMatch(dep -> dep.kind() == 2 && dep.name().equals(sequenceName))) throw new SqlException("2BP01", "identity sequence has dependent view");
                            snapshot = snapshot.dropSequence(sequenceName);
                            columns.set(index, new Column(column.name(), column.type(), column.notNull(), null, 0, column.collation()));
                        } else {
                            List<SqlParser.SequenceOption> options = new ArrayList<>(action.sequenceOptions()); options.add(sequenceType(column.type()));
                            Sequence edited = Sequences.definition(sequenceName, sequence, options, environment.resolver());
                            snapshot = snapshot.putSequence(edited); environment.sequences().install(mutationDomain, edited);
                            columns.set(index, new Column(column.name(), column.type(), column.notNull(), column.defaultValue(), action.generated() == null ? column.identity() : action.generated() ? 2 : 1, column.collation()));
                        }
                    }
                    table = new Table(table.name(), columns, table.primaryKey(), table.unique(), table.rows(), table.nextRowId(), table.checks(), table.indexes(), table.foreignKeys(), table.exclusions(), table.statistics());
                }
                case "add_constraint" -> {
                    SqlParser.Constraint constraint = action.constraint();
                    if (constraint.kind().equals("check")) {
                        List<Check> previous = table.checks(); table = withChecks(table, List.of(constraint), snapshot, environment);
                        for (Check added : table.checks()) if (!previous.contains(added)) addedChecks.add(added.name()); validateRows = true;
                    }
                    else if (constraint.kind().equals("foreign_key")) { table = ForeignKeys.define(table, constraint, snapshot); validateRows = true; }
                    else if (constraint.kind().equals("primary_key") || constraint.kind().equals("unique")) {
                        int[] key = new int[constraint.columns().size()];
                        for (int i = 0; i < key.length; i++) { key[i] = table.column(constraint.columns().get(i)); for (int j = 0; j < i; j++) if (key[i] == key[j]) throw new SqlException("42701", "duplicate constraint column"); }
                        if (constraint.kind().equals("primary_key")) {
                            if (table.primaryKey().length != 0) throw new SqlException("42P16", "multiple primary keys");
                            for (int index : key) { Column column = columns.get(index); columns.set(index, new Column(column.name(), column.type(), true, column.defaultValue(), column.identity(), column.collation())); }
                            table = new Table(table.name(), columns, key, table.unique(), table.rows(), table.nextRowId(), table.checks(), table.indexes(), table.foreignKeys(), table.exclusions(), table.statistics()); rewriteRows = true;
                        } else {
                            List<int[]> unique = new ArrayList<>(table.unique()); unique.add(key);
                            table = new Table(table.name(), columns, table.primaryKey(), unique, table.rows(), table.nextRowId(), table.checks(), table.indexes(), table.foreignKeys(), table.exclusions(), table.statistics()); validateRows = true;
                            table = addUniqueIndex(table, constraint.name(), key, snapshot, List.of());
                        }
                    } else if (constraint.kind().equals("exclude")) { table = Exclusions.define(table, constraint, snapshot, environment, database); validateRows = true; }
                    else throw unsupported("ALTER constraint " + constraint.kind());
                }
                case "rename_constraint" -> {
                    if (action.newName().startsWith("jed_")) for (Index index : table.indexes()) if (index.name().equals(action.name()) && (index.constraint() || Exclusions.forIndex(table, index.name()) != null)) throw new SqlException("42939", "reserved constraint index name");
                    boolean found = false;
                    for (Check check : table.checks()) { if (check.name().equals(action.newName())) throw new SqlException("42710", "constraint already exists"); found |= check.name().equals(action.name()); }
                    for (Index index : table.indexes()) if (index.constraint()) { if (index.name().equals(action.newName())) throw new SqlException("42710", "constraint already exists"); found |= index.name().equals(action.name()); }
                    for (ForeignKey key : table.foreignKeys()) { if (key.name().equals(action.newName())) throw new SqlException("42710", "constraint already exists"); found |= key.name().equals(action.name()); }
                    for (Exclusion exclusion : table.exclusions()) { if (exclusion.name().equals(action.newName())) throw new SqlException("42710", "constraint already exists"); found |= exclusion.name().equals(action.name()); }
                    if (!found) throw new SqlException("42704", "constraint does not exist");
                    List<Index> indexes = new ArrayList<>();
                    for (Index index : table.indexes()) {
                        if ((index.constraint() || Exclusions.forIndex(table, index.name()) != null) && index.name().equals(action.name())) {
                            if (relationTaken(snapshot.put(table), action.newName())) throw new SqlException("42P07", "relation already exists");
                            index = new Index(action.newName(), index.keys(), index.unique(), index.constraint(), index.predicate(), index.kind(), index.entries(), index.timezoneDependencies(), index.gist(), index.hostDependencies());
                        }
                        indexes.add(index);
                    }
                    indexes.sort((left, right) -> Arrays.compareUnsigned(SqlText.encode(left.name()), SqlText.encode(right.name())));
                    List<Check> checks = new ArrayList<>(); for (Check check : table.checks()) checks.add(check.name().equals(action.name()) ? new Check(action.newName(), check.expression(), check.hostDependencies()) : check);
                    checks.sort((left, right) -> Arrays.compareUnsigned(SqlText.encode(left.name()), SqlText.encode(right.name())));
                    table = new Table(table.name(), columns, table.primaryKey(), table.unique(), table.rows(), table.nextRowId(), checks, indexes, table.foreignKeys(), table.exclusions(), table.statistics());
                    List<ForeignKey> foreignKeys = new ArrayList<>();
                    for (ForeignKey key : table.foreignKeys()) foreignKeys.add(key.name().equals(action.name()) ? new ForeignKey(action.newName(), key.local(), key.parent(), key.referenced(), key.onDelete(), key.onUpdate(), key.matchFull()) : key);
                    foreignKeys.sort((a, b) -> Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name()))); table = table.withForeignKeys(foreignKeys);
                    table = table.withExclusions(table.exclusions().stream().map(e -> e.name().equals(action.name()) ? new Exclusion(action.newName(), action.newName(), e.columns(), e.operators()) : e).sorted(java.util.Comparator.comparing(Exclusion::name)).toList());
                }
                case "drop_primary_key", "drop_primary_key_cascade", "drop_constraint", "drop_constraint_cascade" -> {
                    if (action.action().startsWith("drop_primary_key") && table.primaryKey().length == 0) throw new SqlException("42704", "table has no primary key");
                    if (table.primaryKey().length > 0 && (action.action().startsWith("drop_primary_key") || action.name().equals(Sequences.primaryKeyName(table.name())))) {
                        table = new Table(table.name(), columns, new int[0], table.unique(), table.rows(), table.nextRowId(), table.checks(), table.indexes(), table.foreignKeys(), table.exclusions(), table.statistics()); rewriteRows = true; freshRowIds = true; break;
                    }
                    List<Check> checks = table.checks().stream().filter(c -> !c.name().equals(action.name())).toList();
                    List<Index> indexes = new ArrayList<>(); List<int[]> unique = new ArrayList<>(table.unique());
                    for (Index index : table.indexes()) {
                        if ((index.constraint() || Exclusions.forIndex(table, index.name()) != null) && index.name().equals(action.name())) { int[] key = index.keys().stream().mapToInt(IndexKey::column).toArray(); unique.removeIf(columnsKey -> Arrays.equals(columnsKey, key)); }
                        else indexes.add(index);
                    }
                    List<ForeignKey> foreignKeys = table.foreignKeys().stream().filter(key -> !key.name().equals(action.name())).toList();
                    if (checks.size() == table.checks().size() && indexes.size() == table.indexes().size() && foreignKeys.size() == table.foreignKeys().size() && !action.ifExists()) throw new SqlException("42704", "constraint does not exist: " + action.name());
                    table = new Table(table.name(), table.columns(), table.primaryKey(), unique, table.rows(), table.nextRowId(), checks, indexes, foreignKeys, table.exclusions().stream().filter(e -> !e.name().equals(action.name())).toList(), table.statistics());
                }
                default -> throw unsupported("ALTER TABLE " + action.action());
            }
            snapshot = ForeignKeys.validateKeys(snapshot.drop(name).put(table), table.name(), action.action().endsWith("_cascade"));
            table = table.withForeignKeys(snapshot.tables().get(table.name()).foreignKeys());
        }
        if (command.actions().stream().anyMatch(action -> action.action().equals("type"))) {
            table = table.withIndexes(Indexes.retype(table, environment));
            var rebound = new ArrayList<Check>();
            var checkResolver = environment.resolver(table.bindings(null), new ScalarPlan.Frame());
            for (Check check : table.checks()) {
                checkResolver.resolve(check.expression(), SqlType.BOOLEAN);
                rebound.add(new Check(check.name(), check.expression(), HostDependencies.collect(check.expression(), checkResolver, false)));
            }
            table = new Table(table.name(), table.columns(), table.primaryKey(), table.unique(), table.rows(), table.nextRowId(), rebound, table.indexes(), table.foreignKeys(), table.exclusions(), table.statistics());
        }
        if (rewriteRows || validateRows) {
            Table validated = command.actions().stream().anyMatch(action -> action.action().equals("type")) ? table : new Table(table.name(), table.columns(), table.primaryKey(), table.unique(), table.rows(), table.nextRowId(), table.checks().stream().filter(c -> addedChecks.contains(c.name())).toList(), table.indexes(), table.foreignKeys(), table.exclusions());
            CheckEvaluator checks = new CheckEvaluator(validated, environment);
            PersistentTree tree = rewriteRows ? PersistentTree.empty(storageDatabase().pageSize(), new TableLeafCodec(table.types(), table.primaryKey()), storageDatabase().pageCodec()) : table.rows();
            boolean[] touched = new boolean[original.columns().size()]; Arrays.fill(touched, true);
            environment.initialize(environment.meter()); long nextRowId = freshRowIds && table.primaryKey().length == 0 ? 0 : table.nextRowId();
            try (TableScan scan = new TableScan(original, null, original.columns().stream().map(Column::name).toList(), null, args, environment.meter(), touched, environment)) {
                PersistentTree.Entry entry;
                while ((entry = scan.next()) != null) {
                    environment.meter().charge(Costs.STORAGE_ROW_READ); environment.meter().guard();
                    List<SqlValue> values = Encoding.row(entry, original.types());
                    for (var conversion : conversions) values = conversion.apply(values);
                    validate(table, tree, values, rewriteRows ? null : entry.key(), checks, environment.meter());
                    byte[] finalKey = !rewriteRows || table.primaryKey().length == 0 && !freshRowIds ? entry.key() : key(table, values, nextRowId, storageDatabase().environment());
                    if (rewriteRows) {
                        byte[] key = finalKey;
                        if (table.primaryKey().length == 0 && freshRowIds) nextRowId++;
                        if (tree.getEntry(key) != null) throw new SqlException("23505", "duplicate primary key");
                        tree = tree.put(Encoding.entry(key, values, null, null, storageDatabase().pageLayout(), environment.meter(), table.primaryKey()));
                    }
                }
            }
            table = table.changed(tree, nextRowId).withStatistics(table.statistics());
            if (rewriteRows) {
                boolean[] notNull = new boolean[original.columns().size()];
                for (int i = 0; i < notNull.length; i++) {
                    Column before = original.columns().get(i);
                    notNull[i] = !before.notNull() && table.columns().stream().anyMatch(c -> c.name().equals(before.name()) && c.notNull());
                }
                boolean needed = false; for (boolean check : notNull) needed |= check;
                if (needed) try (TableScan scan = new TableScan(original, null, original.columns().stream().map(Column::name).toList(), null, args, environment.meter(), notNull, environment)) {
                    PersistentTree.Entry entry; while ((entry = scan.next()) != null) {
                        environment.meter().charge(Costs.STORAGE_ROW_READ); environment.meter().guard(); List<SqlValue> row = Encoding.row(entry, original.types());
                        for (int i = 0; i < notNull.length; i++) if (notNull[i] && row.get(i).isNull()) throw new SqlException("23502", "column contains NULL");
                    }
                }
            }
            List<Index> rebuiltIndexes = new ArrayList<>();
            Table finalTable = table;
            boolean preserveIndexEntries = command.actions().stream().allMatch(action -> action.action().equals("add_column"));
            for (Index index : table.indexes()) {
                if (preserveIndexEntries && Arrays.equals(original.primaryKey(), table.primaryKey()) && original.indexes().stream().anyMatch(existing -> existing.name().equals(index.name()))) { rebuiltIndexes.add(index); continue; }
                try {
                    rebuiltIndexes.add(environment.meter().unmetered(() -> {
                        try { return Indexes.build(index, finalTable, args, environment, storageDatabase(), session.workMem()); }
                        catch (IOException failure) { throw new java.io.UncheckedIOException(failure); }
                    }));
                } catch (java.io.UncheckedIOException failure) { throw failure.getCause(); }
            }
            table = table.withIndexes(rebuiltIndexes);
            ForeignKeys.validateAlter(original, table, snapshot.put(table), environment, rewriteRows);
            if (!preserveIndexEntries) Exclusions.validateAlter(table, environment);
        }
        for (SqlParser.AlterAction action : command.actions()) if (action.action().equals("type"))
            for (View view : snapshot.views().values()) for (ViewDependency dep : view.dependencies())
                if (dep.kind() == 0 && dep.name().equals(table.name()) && dep.columns().contains(action.name())) throw unsupported("changing type of column used by view");
        String onCommit = snapshot.tempActions().get(table.name());
        Snapshot result = snapshot.drop(name).put(table);
        return onCommit == null ? result : result.withTempAction(table.name(), onCommit);
    }

    static Table indexOwner(Snapshot snapshot, String name) {
        for (Table table : snapshot.tables().values()) for (Index index : table.indexes()) if (index.name().equals(name)) return table;
        return null;
    }
    private Snapshot createIndex(SqlParser.CreateIndex command, Snapshot snapshot, SqlValue[] args, QueryEnvironment environment) throws IOException {
        if (snapshot.views().containsKey(name(command.table()))) throw new SqlException("42809", "index target is a view");
        Table table = table(snapshot, command.table());
        if (!command.method().equals("btree") && !command.method().equals("gin") && !command.method().equals("gist")) {
            if (command.method().equals("gist")) throw unsupported("index method gist");
            throw new SqlException("42704", "access method does not exist: " + command.method());
        }
        Index definition = Indexes.definition(command, table, snapshot, environment, database);
        if (relationTaken(snapshot, definition.name())) { if (command.ifNotExists()) return snapshot; throw new SqlException("42P07", "relation already exists: " + definition.name()); }
        CollationCompatibility.require(table, environment.meter());
        for (IndexKey key : definition.keys()) snapshot = snapshot.useCollation(storageDatabase().environment().collation(key.collation()));
        Index built = Indexes.build(definition, table, args, environment, storageDatabase(), session.workMem());
        List<Index> indexes = new ArrayList<>(table.indexes()); indexes.add(built);
        indexes.sort((left, right) -> Arrays.compareUnsigned(SqlText.encode(left.name()), SqlText.encode(right.name())));
        return snapshot.put(table.withIndexes(indexes));
    }
    private Snapshot dropIndex(SqlParser.DropIndex command, Snapshot snapshot) {
        for (SqlParser.Name syntax : command.names()) {
            String name = name(syntax); Table table = indexOwner(snapshot, name);
            if (table == null) {
                if (relationTaken(snapshot, name)) throw new SqlException("42809", "relation is not an index");
                if (command.ifExists()) continue; throw new SqlException("42704", "index does not exist: " + name);
            }
            for (Index index : table.indexes()) if (index.name().equals(name) && (index.constraint() || Exclusions.forIndex(table, name) != null)) throw new SqlException("2BP01", "index belongs to a constraint");
            Table changed = table.withIndexes(table.indexes().stream().filter(index -> !index.name().equals(name)).toList());
            for (Table child : snapshot.tables().values()) for (ForeignKey key : child.foreignKeys()) if (key.parent().equals(table.name())) {
                try { ForeignKeys.unique(changed, key.referenced()); } catch (SqlException failure) { if (failure.sqlState().equals("42830")) throw new SqlException("2BP01", "index is required by foreign key " + key.name()); throw failure; }
            }
            snapshot = snapshot.put(changed);
        }
        return snapshot;
    }

    private static SqlParser.SequenceOption sequenceType(SqlType type) { return new SqlParser.SequenceOption("as", null, new SqlParser.TypeName(type == SqlType.I16 ? "i16" : type == SqlType.I32 ? "i32" : "i64", -1, -1, 0)); }
    static boolean relationTaken(Snapshot snapshot, String name) { return snapshot.tables().containsKey(name) || snapshot.views().containsKey(name) || snapshot.sequences().containsKey(name) || indexOwner(snapshot, name) != null; }
    private Snapshot createSequence(SqlParser.CreateSequence command, Snapshot snapshot, QueryEnvironment environment) {
        String name = name(command.name()); if (name.startsWith("jed_")) throw new SqlException("42939", "reserved sequence name");
        if (relationTaken(snapshot, name)) { if (command.ifNotExists()) return snapshot; throw new SqlException("42P07", "relation already exists: " + name); }
        return snapshot.putSequence(Sequences.definition(name, null, command.options(), environment.resolver()));
    }
    private Snapshot alterSequence(SqlParser.AlterSequence command, Snapshot snapshot, QueryEnvironment environment) {
        String name = name(command.name()); Sequence previous = snapshot.sequences().get(name);
        if (previous == null) { if (command.ifExists()) return snapshot; throw new SqlException("42P01", "sequence does not exist: " + name); }
        if (command.newName() != null) {
            if (command.newName().startsWith("jed_")) throw new SqlException("42939", "reserved sequence name");
            if (relationTaken(snapshot, command.newName())) throw new SqlException("42P07", "relation already exists: " + command.newName());
            String newName = command.newName();
            for (Table table : snapshot.tables().values()) {
                List<Column> columns = new ArrayList<>(); boolean changed = false;
                for (Column column : table.columns()) {
                    SqlParser.Expr value = Sequences.renameReferences(column.defaultValue(), name, newName); changed |= value != column.defaultValue();
                    columns.add(new Column(column.name(), column.type(), column.notNull(), value, column.identity(), column.collation()));
                }
                if (changed) snapshot = snapshot.put(new Table(table.name(), columns, table.primaryKey(), table.unique(), table.rows(), table.nextRowId(), table.checks(), table.indexes(), table.foreignKeys(), table.exclusions(), table.statistics()));
            }
            for (View view : snapshot.views().values()) {
                List<ViewColumn> columns = view.columns().stream().map(column -> new ViewColumn(column.name(), column.defaultText() == null ? null : Sequences.renameReferences(column.defaultText(), name, newName))).toList();
                List<ViewDependency> deps = view.dependencies().stream().map(dep -> dep.kind() == 2 && dep.name().equals(name) ? new ViewDependency(2, newName, dep.columns()) : dep).toList();
                snapshot = snapshot.putView(new View(view.name(), columns, Sequences.renameReferences(view.definition(), name, newName), deps, view.checkOption(), view.flags()));
            }
            environment.sequences().renameHistory(mutationDomain, name, newName);
            return snapshot.dropSequence(name).putSequence(new Sequence(newName, previous.increment(), previous.minimum(), previous.maximum(), previous.start(), previous.cache(), previous.last(), previous.cycle(), previous.called(), previous.owner(), previous.ownerColumn()));
        }
        return snapshot.putSequence(Sequences.definition(name, previous, command.options(), environment.resolver()));
    }
    private Snapshot dropSequence(SqlParser.DropSequence command, Snapshot snapshot) {
        for (SqlParser.Name syntax : command.names()) {
            String name = name(syntax); Sequence sequence = snapshot.sequences().get(name);
            if (sequence == null) { if (!command.ifExists()) throw new SqlException("42P01", "sequence does not exist: " + name); continue; }
            if (sequence.owner() != null || sequenceReferenced(snapshot, name)) throw new SqlException(command.cascade() ? "0A000" : "2BP01", "sequence has dependent defaults or owner");
            List<String> users = snapshot.views().values().stream().filter(view -> view.dependencies().stream().anyMatch(dep -> dep.kind() == 2 && dep.name().equals(name))).map(View::name).toList();
            if (!users.isEmpty()) {
                if (!command.cascade()) throw new SqlException("2BP01", "sequence has dependent views");
                snapshot = dropView(new SqlParser.DropView(users.stream().map(n -> new SqlParser.Name(List.of(n))).toList(), false, true), snapshot);
            }
            snapshot = snapshot.dropSequence(name);
        }
        return snapshot;
    }
    private static boolean sequenceReferenced(Snapshot snapshot, String name) {
        for (Table table : snapshot.tables().values()) for (Column column : table.columns()) if (sequenceReferenced(column.defaultValue(), name)) return true;
        for (View view : snapshot.views().values()) for (ViewColumn column : view.columns()) if (column.defaultText() != null && sequenceReferenced(new SqlParser("SELECT " + column.defaultText()).parse().items().getFirst().expression(), name)) return true;
        return false;
    }
    private static boolean sequenceReferenced(SqlParser.Expr expression, String name) {
        if (expression == null) return false;
        expression = DefaultExpression.unwrap(expression);
        if (expression instanceof SqlParser.Call call && List.of("nextval", "currval", "setval").contains(call.name()) && !call.args().isEmpty()
                && call.args().getFirst() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING && Sequences.name(literal.text()).equals(name)) return true;
        for (SqlParser.Expr child : DefaultExpression.children(expression)) if (sequenceReferenced(child, name)) return true;
        return false;
    }

    private Snapshot alterView(SqlParser.AlterTable command, Snapshot snapshot, QueryEnvironment environment) {
        View view = snapshot.views().get(name(command.table()));
        for (SqlParser.AlterAction action : command.actions()) {
            List<ViewColumn> columns = new ArrayList<>(view.columns());
            switch (action.action()) {
                case "set_option", "reset_option" -> {
                    int bits = action.action().equals("reset_option") ? 0 : viewFlags(List.of(new SqlParser.ViewOption(action.name(), action.newName())));
                    int check = view.checkOption(), flags = view.flags();
                    if (action.name().equals("check_option")) {
                        check = (bits & 4) != 0 ? 1 : (bits & 8) != 0 ? 2 : 0;
                        if (check != 0 && !ViewWrites.shape(view.query(), true)) throw unsupported("CHECK OPTION on a non-updatable view");
                    } else {
                        int mask = action.name().equals("security_barrier") ? 1 : action.name().equals("security_invoker") ? 2 : 0;
                        flags = flags & ~mask | bits & mask;
                    }
                    if ((view.flags() & 2) != 0 && (flags & 2) == 0) session.privileges().check(view.query(), environment.readRoot, environment);
                    view = new View(view.name(), columns, view.definition(), view.dependencies(), check, flags);
                }
                case "rename_table" -> {
                    if (action.newName().startsWith("jed_")) throw new SqlException("42939", "reserved view name");
                    if (relationTaken(snapshot, action.newName())) throw new SqlException("42P07", "relation already exists");
                    snapshot = ViewRenames.apply(snapshot, view.name(), action.newName(), null, null, columns.stream().map(ViewColumn::name).toList());
                    snapshot = snapshot.dropView(view.name()); view = new View(action.newName(), columns, view.definition(), view.dependencies(), view.checkOption(), view.flags());
                }
                case "rename_column", "set_default", "drop_default" -> {
                    int ordinal = -1; for (int i = 0; i < columns.size(); i++) if (columns.get(i).name().equals(action.name())) ordinal = i;
                    if (ordinal < 0) throw new SqlException("42703", "view column does not exist: " + action.name());
                    ViewColumn column = columns.get(ordinal);
                    if (action.action().equals("rename_column")) {
                        if (columns.stream().anyMatch(c -> c.name().equals(action.newName()))) throw new SqlException("42701", "duplicate view column");
                        snapshot = ViewRenames.apply(snapshot, view.name(), view.name(), action.name(), action.newName(), columns.stream().map(ViewColumn::name).toList());
                        columns.set(ordinal, new ViewColumn(action.newName(), column.defaultText()));
                    } else {
                        String text = null;
                        if (action.action().equals("set_default")) {
                            SqlType type; try (QueryPlan plan = environment.viewScope(view.name()).plan(view.query())) { type = plan.types().get(ordinal); }
                            if (type == SqlType.UNKNOWN) type = SqlType.TEXT;
                            SqlParser.Expr value = defaultValue(action.expression(), type, environment);
                            if (value instanceof SqlParser.StoredValue constant && !constant.value().isNull()) {
                                SqlParser.Literal literal = DefaultExpression.literal(action.expression());
                                SqlValue displayed = constant.value();
                                if (literal != null && (literal.kind() == Lexer.Kind.INTEGER || literal.kind() == Lexer.Kind.DECIMAL)) displayed = environment.resolver().resolve(literal, null).run(environment.meter());
                                String rendered = SqlCasts.cast(displayed, SqlType.TEXT).asText();
                                text = type.isNumeric() || type == SqlType.BOOLEAN ? rendered : "'" + rendered.replace("'", "''") + "'";
                            } else if (value instanceof SqlParser.StoredExpression expression && !DefaultExpression.unwrap(expression).equals(new SqlParser.Literal(Lexer.Kind.WORD, "null"))) text = expression.text();
                        }
                        columns.set(ordinal, new ViewColumn(column.name(), text));
                    }
                    view = new View(view.name(), columns, view.definition(), view.dependencies(), view.checkOption(), view.flags());
                }
                default -> throw new SqlException("42809", "ALTER action requires a table");
            }
            snapshot = snapshot.putView(view);
        }
        return snapshot;
    }
    private Snapshot createView(SqlParser.CreateView command, Snapshot snapshot, QueryEnvironment environment) {
        String name = name(command.name());
        if (name.startsWith("jed_")) throw new SqlException("42939", "reserved view name");

        boolean taken = relationTaken(snapshot, name);
        if (taken && command.ifNotExists()) return snapshot;
        if (writes(command.query())) throw unsupported("data-modifying WITH in a view");
        QueryEnvironment scope = environment.captureView();
        SqlParser stored = new SqlParser(command.definition()); SqlParser.Statement bodySyntax = stored.parseStatement();
        List<ViewColumn> columns = new ArrayList<>(); List<SqlType> types;
        try (QueryPlan body = read(bodySyntax, snapshot, new SqlValue[0], scope)) {
            types = List.copyOf(body.types());
            int flags = viewFlags(command.options());
            if (command.checkOption() != 0 && (flags & 12) != 0) throw new SqlException("22023", "check option specified more than once");
            int checkOption = command.checkOption() != 0 ? command.checkOption() : (flags & 4) != 0 ? 1 : (flags & 8) != 0 ? 2 : 0;
            flags &= 3;
            if (checkOption != 0 && !ViewWrites.shape(bodySyntax, true)) throw unsupported("CHECK OPTION on a non-updatable view");
            if (command.columns().size() > types.size()) throw new SqlException("42601", "too many view column names");
            java.util.Set<String> names = new java.util.HashSet<>();
            for (int i = 0; i < types.size(); i++) {
                String column = i < command.columns().size() ? command.columns().get(i) : body.names().get(i);
                if (!names.add(column)) throw new SqlException("42701", "duplicate view column: " + column);
                columns.add(new ViewColumn(column, null));
            }
            for (SqlType type : types) if (type.kind() == SqlType.Kind.RECORD || type.kind() == SqlType.Kind.ARRAY && type.element().kind() == SqlType.Kind.RECORD) throw new SqlException("42P16", "view column has pseudo-type record");
            if (columns.isEmpty()) throw unsupported("zero-column views");
            if (taken) {
                if (!command.replace()) throw new SqlException("42P07", "relation already exists: " + name);
                View previous = snapshot.views().get(name);
                if (previous == null) throw new SqlException("42809", "relation is not a view: " + name);
                if (columns.size() < previous.columns().size()) throw new SqlException("42P16", "cannot drop view columns");
                List<SqlType> previousTypes = null; List<CollationDerivation> previousCollations = null;
                try (QueryPlan old = read(previous.query(), snapshot, new SqlValue[0], environment.viewScope(name))) { previousTypes = List.copyOf(old.types()); previousCollations = List.copyOf(old.collations()); }
                catch (SqlException ignored) { /* A broken definition can be repaired. */ }
                for (int i = 0; i < previous.columns().size(); i++) {
                    if (!columns.get(i).name().equals(previous.columns().get(i).name()) || previousTypes != null && !types.get(i).equals(previousTypes.get(i))) throw new SqlException("42P16", "cannot change existing view columns");
                    if (previousCollations != null && CollationDerivation.collatable(types.get(i))) {
                        CollationDerivation before = previousCollations.get(i), after = body.collations().get(i);
                        String a = before.conflict() ? null : before.name() == null ? CollationCompatibility.defaultName(snapshot) : before.name();
                        String b = after.conflict() ? null : after.name() == null ? CollationCompatibility.defaultName(snapshot) : after.name();
                        if (!java.util.Objects.equals(a, b)) throw new SqlException("42P16", "cannot change view column collation");
                    }
                    columns.set(i, new ViewColumn(columns.get(i).name(), previous.columns().get(i).defaultText()));
                }
            }
            if (!scope.freezableStars()) throw unsupported("view star expansion has ambiguous column names");
            View view = new View(name, columns, stored.expanded(scope.viewStars()), scope.viewDependencies(), checkOption, flags);
            Snapshot result = snapshot.putView(view);
            if (viewCycle(result, name, name, new java.util.HashSet<>())) throw new SqlException("42P17", "recursive view definition: " + name);
            return result;
        }
    }
    private void revalidateViews(Snapshot before, Snapshot after, String changed, int kind, boolean replacement, QueryEnvironment environment) {
        java.util.Set<String> dependents = new java.util.TreeSet<>((a, b) -> Arrays.compareUnsigned(SqlText.encode(a), SqlText.encode(b)));
        java.util.ArrayDeque<String> pending = new java.util.ArrayDeque<>();
        for (View view : before.views().values()) if (view.dependencies().stream().anyMatch(dep -> dep.kind() == kind && dep.name().equals(changed))) {
            dependents.add(view.name()); pending.add(view.name());
        }
        while (!pending.isEmpty()) {
            String dependency = pending.removeFirst();
            for (View view : before.views().values()) if (!dependents.contains(view.name()) && view.dependencies().stream().anyMatch(dep -> dep.kind() == 1 && dep.name().equals(dependency))) { dependents.add(view.name()); pending.add(view.name()); }
        }
        if (dependents.isEmpty()) return;
        Snapshot roots = environment.readRoot == null ? after : environment.readRoot.withDomain(mutationDomain, after);
        for (String dependent : dependents) {
            View view = after.views().get(dependent); if (view == null) continue;
            try (QueryEnvironment check = new QueryEnvironment((query, scope) -> read(query, roots, new SqlValue[0], scope), new SqlValue[0], environment.meter())) {
                check.readRoot = roots; check.namedTypes(roots.types()); check.sequences(new Sequences(roots, Sequences.History.empty()));
                QueryEnvironment trace = check.captureView();
                try (QueryPlan plan = read(view.query(), roots, new SqlValue[0], trace)) {
                    boolean direct = before.views().get(dependent).dependencies().stream().anyMatch(dep -> dep.kind() == kind && dep.name().equals(changed));
                    if (direct && (!trace.viewDependencies().equals(view.dependencies()) || replacement && trace.viewColumnAtOrAbove(changed, before.views().get(changed).columns().size())))
                        throw unsupported("catalog change would change view bindings: " + dependent + " (expected " + view.dependencies() + ", found " + trace.viewDependencies() + ")");
                }
            } catch (SqlException error) { if (error.sqlState().equals("XX003")) throw error; throw unsupported("catalog change would invalidate view: " + dependent + ": " + error.getMessage()); }
        }
    }
    private static int viewFlags(List<SqlParser.ViewOption> options) {
        int flags = 0; java.util.Set<String> seen = new java.util.HashSet<>();
        for (SqlParser.ViewOption option : options) {
            if (!seen.add(option.name())) throw new SqlException("22023", "duplicate view option");
            String value = option.value().toLowerCase(java.util.Locale.ROOT);
            if (option.name().equals("check_option")) {
                flags |= switch (value) { case "local" -> 4; case "cascaded" -> 8; default -> throw new SqlException("22023", "invalid view check option"); }; continue;
            }
            int bit = switch (option.name()) { case "security_barrier" -> 1; case "security_invoker" -> 2; default -> throw new SqlException("22023", "unknown view option"); };
            boolean yes = value.equals("1") || value.equals("on") || !value.isEmpty() && ("true".startsWith(value) || "yes".startsWith(value));
            boolean no = value.equals("0") || !value.isEmpty() && ("false".startsWith(value) || "no".startsWith(value)) || value.length() >= 2 && "off".startsWith(value);
            if (!yes && !no) throw new SqlException("22023", "invalid boolean view option");
            if (yes) flags |= bit;
        }
        return flags;
    }
    private static boolean viewCycle(Snapshot snapshot, String start, String current, java.util.Set<String> visited) {
        if (!visited.add(current)) return false;
        View view = snapshot.views().get(current); if (view == null) return false;
        for (ViewDependency dependency : view.dependencies()) if (dependency.kind() == 1
                && (dependency.name().equals(start) || viewCycle(snapshot, start, dependency.name(), visited))) return true;
        return false;
    }
    private Snapshot dropView(SqlParser.DropView command, Snapshot snapshot) {
        java.util.Set<String> drops = new java.util.HashSet<>();
        for (SqlParser.Name syntax : command.names()) {
            String name = name(syntax);
            if (snapshot.tables().containsKey(name)) throw new SqlException("42809", "relation is not a view: " + name);
            if (!snapshot.views().containsKey(name)) { if (!command.ifExists()) throw new SqlException("42P01", "view does not exist: " + name); }
            else drops.add(name);
        }
        boolean changed;
        do {
            changed = false;
            for (View view : snapshot.views().values()) if (!drops.contains(view.name()) && view.dependencies().stream().anyMatch(dep -> dep.kind() == 1 && drops.contains(dep.name()))) {
                if (!command.cascade()) throw new SqlException("2BP01", "view has dependents");
                changed |= drops.add(view.name());
            }
        } while (changed);
        for (String name : drops) snapshot = snapshot.dropView(name);
        return snapshot;
    }

    private Snapshot createType(SqlParser.CreateType command, Snapshot snapshot, QueryEnvironment environment) {
        String name = name(command.name());
        if (snapshot.types().containsKey(name) || ScalarCatalog.FACTS.stream().anyMatch(f -> f.aliases().contains(name))) throw new SqlException("42710", "type already exists: " + name);
        List<SqlType.Field> fields = new ArrayList<>();
        ScalarPlan.Resolver resolver = environment.resolver();
        for (SqlParser.Column column : command.fields()) {
            if (fields.stream().anyMatch(f -> f.name().equals(column.name()))) throw new SqlException("42701", "duplicate composite field");
            boolean notNull = false;
            for (SqlParser.Constraint constraint : column.constraints()) {
                if (constraint.kind().equals("not_null")) notNull = true;
                else if (!constraint.kind().equals("null")) throw unsupported("composite field constraint");
            }
            fields.add(new SqlType.Field(column.name(), resolver.type(column.type()), notNull));
        }
        SqlType type = SqlType.composite(name, fields);
        if (compositeDepth(type) > 32) throw new SqlException("54001", "composite nesting exceeds 32");
        return snapshot.putType(type);
    }
    private static int compositeDepth(SqlType type) {
        if (type.kind() == SqlType.Kind.ARRAY) return compositeDepth(type.element());
        if (type.kind() != SqlType.Kind.COMPOSITE) return 0;
        int depth = 0; for (SqlType.Field field : type.fields()) depth = Math.max(depth, compositeDepth(field.type())); return depth + 1;
    }
    private static boolean dependsOn(SqlType type, String name) {
        if (type.kind() == SqlType.Kind.ARRAY) return dependsOn(type.element(), name);
        if (type.kind() != SqlType.Kind.COMPOSITE) return false;
        if (type.name().equals(name)) return true;
        return type.fields().stream().anyMatch(field -> dependsOn(field.type(), name));
    }
    private Snapshot dropType(SqlParser.DropType command, Snapshot snapshot) {
        for (SqlParser.Name syntax : command.names()) {
            String name = name(syntax);
            if (!snapshot.types().containsKey(name)) { if (command.ifExists()) continue; throw new SqlException("42704", "type does not exist: " + name); }
            boolean used = snapshot.tables().values().stream().anyMatch(table -> table.columns().stream().anyMatch(column -> dependsOn(column.type(), name)))
                || snapshot.types().values().stream().anyMatch(type -> !type.name().equals(name) && dependsOn(type, name));
            if (used) throw new SqlException(command.cascade() ? "0A000" : "2BP01", "type has dependent columns or fields");
            List<String> views = snapshot.views().values().stream().filter(view -> view.dependencies().stream().anyMatch(dep -> dep.kind() == 3 && dep.name().equals(name))).map(View::name).toList();
            if (!views.isEmpty()) { if (!command.cascade()) throw new SqlException("2BP01", "type has dependent views"); snapshot = dropView(new SqlParser.DropView(views.stream().map(n -> new SqlParser.Name(List.of(n))).toList(), false, true), snapshot); }
            snapshot = snapshot.dropType(name);
        }
        return snapshot;
    }

    private Snapshot drop(SqlParser.DropTable command, Snapshot snapshot) {
        for (SqlParser.Name syntax : command.names()) { String target = name(syntax); if (snapshot.views().containsKey(target) || snapshot.sequences().containsKey(target) || indexOwner(snapshot, target) != null) throw new SqlException("42809", "relation is not a table: " + target); if (!command.ifExists() && !snapshot.tables().containsKey(target)) throw new SqlException("42P01", "table does not exist: " + target); }
        java.util.Set<String> dropped = new java.util.HashSet<>();
        for (SqlParser.Name syntax : command.names()) {
            String name = name(syntax); if (!dropped.add(name)) continue;
            for (Table child : new ArrayList<>(snapshot.tables().values())) if (!command.names().stream().anyMatch(n -> name(n).equals(child.name()))) {
                List<ForeignKey> retained = child.foreignKeys().stream().filter(key -> !key.parent().equals(name)).toList();
                if (retained.size() != child.foreignKeys().size()) { if (!command.cascade()) throw new SqlException("2BP01", "table has foreign key dependents"); snapshot = snapshot.put(child.withForeignKeys(retained)); }
            }
            if (snapshot.views().containsKey(name) || snapshot.sequences().containsKey(name) || indexOwner(snapshot, name) != null) throw new SqlException("42809", "relation is not a table: " + name);
            List<String> dependents = snapshot.views().values().stream().filter(view -> view.dependencies().stream().anyMatch(dep -> dep.kind() == 0 && dep.name().equals(name))).map(View::name).toList();
            if (!dependents.isEmpty()) {
                if (!command.cascade()) throw new SqlException("2BP01", "table has dependent views");
                snapshot = dropView(new SqlParser.DropView(dependents.stream().map(n -> new SqlParser.Name(List.of(n))).toList(), false, true), snapshot);
            }
            if (!snapshot.tables.containsKey(name)) {
                if (!command.ifExists()) throw new SqlException("42P01", "table does not exist: " + name);
            } else {
                for (Sequence sequence : new ArrayList<>(snapshot.sequences().values())) if (name.equals(sequence.owner())) {
                    List<String> users = snapshot.views().values().stream().filter(view -> view.dependencies().stream().anyMatch(dep -> dep.kind() == 2 && dep.name().equals(sequence.name()))).map(View::name).toList();
                    if (!users.isEmpty()) { if (!command.cascade()) throw new SqlException("2BP01", "owned sequence has dependent views"); snapshot = dropView(new SqlParser.DropView(users.stream().map(n -> new SqlParser.Name(List.of(n))).toList(), false, true), snapshot); }
                    snapshot = snapshot.dropSequence(sequence.name());
                }
                snapshot = snapshot.drop(name);
            }
        }
        return snapshot;
    }

    private Snapshot insert(SqlParser.Insert command, Snapshot snapshot, Snapshot pending, SqlValue[] args, CostMeter meter, Returning returning, QueryEnvironment environment, ViewWrites.Checks viewChecks) throws IOException {
        Table table = table(snapshot, command.table());
        if (command.defaultValues() && (!command.columns().isEmpty() || command.overriding() != null)) throw new SqlException("42601", "DEFAULT VALUES cannot have a target column list or OVERRIDING");
        int[] targets = new int[command.columns().isEmpty() ? table.columns.size() : command.columns().size()];
        for (int i = 0; i < targets.length; i++) {
            targets[i] = command.columns().isEmpty() ? i : table.column(command.columns().get(i));
            for (int j = 0; j < i; j++) if (targets[j] == targets[i]) throw new SqlException("42701", "duplicate INSERT target");
        }
        ScalarPlan.Resolver resolver = environment.resolver();
        CheckEvaluator checks = new CheckEvaluator(table, environment);
        List<List<ScalarPlan.Expression>> inputs = new ArrayList<>();
        List<List<SqlParser.Expr>> syntaxRows = command.defaultValues() ? List.of(List.of()) : command.values();
        for (List<SqlParser.Expr> syntax : syntaxRows) {
            if (!command.defaultValues() && syntax.size() != targets.length) throw new SqlException("42601", "INSERT column count mismatch");
            var expressions = new ArrayList<ScalarPlan.Expression>(java.util.Collections.nCopies(table.columns.size(), null));
            for (int i = 0; i < syntax.size(); i++) {
                Column c = table.columns.get(targets[i]);
                boolean generated = syntax.get(i) instanceof SqlParser.Default || c.identity() != 0 && "user".equals(command.overriding());
                if (c.identity() == 2 && !generated && !"system".equals(command.overriding())) throw new SqlException("428C9", "cannot insert explicit value in GENERATED ALWAYS column");
                expressions.set(targets[i], assignment(resolver, generated ? defaultOf(c) : syntax.get(i), c.type));
            }
            for (int i = 0; i < expressions.size(); i++) if (expressions.get(i) == null) expressions.set(i, assignment(resolver, defaultOf(table.columns.get(i)), table.columns.get(i).type));
            inputs.add(expressions);
        }
        try (RowSpool candidates = new RowSpool(table.types(), 0, null, command.query() == null ? null : meter.memory()); IndexMutation indexes = new IndexMutation(table, environment, session.workMem(), storageDatabase().scratchDirectory()).insertOnly();
             ForeignKeys.Batch references = new ForeignKeys.Batch(table, storageDatabase(), environment, session.workMem(), pending);
             Upsert upsert = command.conflict() == null ? null : new Upsert(table(pending, command.table()), command, environment, storageDatabase(), session.workMem(), returning, references, viewChecks)) {
            if (command.query() != null) {
                try (QueryPlan source = read(command.query(), snapshot, args, environment)) {
                    if (source.types().size() != targets.length) throw new SqlException("42601", "INSERT source column count mismatch");
                    ScalarPlan.Frame frame = new ScalarPlan.Frame();
                    List<ScalarPlan.Binding> bindings = new ArrayList<>();
                    for (int c = 0; c < targets.length; c++) bindings.add(new ScalarPlan.Binding(List.of(), "column" + c, source.types().get(c)));
                    ScalarPlan.Resolver sourceResolver = environment.resolver(bindings, frame);
                    var projection = new ArrayList<ScalarPlan.Expression>(java.util.Collections.nCopies(table.columns.size(), null));
                    for (int c = 0; c < targets.length; c++) {
                        Column column = table.columns.get(targets[c]); boolean generated = column.identity() != 0 && "user".equals(command.overriding());
                        if (column.identity() == 2 && !generated && !"system".equals(command.overriding())) throw new SqlException("428C9", "cannot insert explicit value in GENERATED ALWAYS column");
                        SqlParser.Literal literal = generated ? null : AssignmentSources.stringLiteral(command.query(), c);
                        projection.set(targets[c], generated ? assignment(resolver, defaultOf(column), column.type())
                            : literal != null && AssignmentSources.stringNative(column.type()) ? ScalarPlan.constant(SqlCasts.literal(literal.text(), column.type(), meter))
                            : assignment(sourceResolver, new SqlParser.Name(List.of("column" + c)), column.type()));
                    }
                    for (int c = 0; c < projection.size(); c++) if (projection.get(c) == null) projection.set(c, assignment(resolver, defaultOf(table.columns.get(c)), table.columns.get(c).type));
                    Row row;
                    while ((row = source.next(meter)) != null) {
                        frame.values = row.values();
                        List<SqlValue> values = new ArrayList<>();
                        for (ScalarPlan.Expression expression : projection) values.add(expression.run(meter));
                        if (upsert == null) candidates.append(values); else upsert.accept(values);
                    }
                    if (meter.writeMetrics != null) meter.writeSourceActual = source.actualPlan(false);
                }
            } else { long beforeValues = meter.cost(); environment.initialize(meter); meter.writePhase("Values", beforeValues); for (List<ScalarPlan.Expression> input : inputs) {
                beforeValues = meter.cost(); meter.guard();
                List<SqlValue> values = new ArrayList<>(input.size());
                for (ScalarPlan.Expression expression : input) values.add(expression.run(meter));
                meter.writePhase("Values", beforeValues);
                if (upsert == null) candidates.append(values); else upsert.accept(values);
            } }
            Table writable = table(pending, command.table());
            if (upsert != null) { Table changed = upsert.finish(); mutationAffected += upsert.affectedRows(); return changed == writable ? pending : references.apply(writePhase(pending.put(changed), meter), deferredReferences); }
            PersistentTree tree = writable.rows; long rowId = writable.nextRowId;
            try (RowSpool.Cursor rows = candidates.cursor()) {
                List<SqlValue> values;
                while ((values = rows.next()) != null) {
                    meter.guard(); validate(table, tree, values, null, checks, meter);
                    byte[] key = key(table, values, rowId, storageDatabase().environment());
                    if (tree.getEntry(key) != null) throw new SqlException("23505", "duplicate primary key");
                    indexes.add(null, null, values, key);
                    viewChecks.accept(values);
                    references.record(null, values);
                    tree = tree.put(Encoding.entry(key, values, null, null, storageDatabase().pageLayout(), meter, table.primaryKey));
                    mutationAffected++; if (returning != null) returning.add(null, values);
                    if (table.primaryKey.length == 0) { if (rowId == Long.MAX_VALUE) throw SqlException.numericOverflow(); rowId++; }
                }
            }
            return candidates.size() == 0 ? pending : references.apply(writePhase(pending.put(indexes.checkExclusions(indexes.apply(writable.changed(tree, rowId)))), meter), deferredReferences);
        }
    }

    private MergePlan.Result merge(SqlParser.Merge command, Snapshot snapshot, Snapshot pending, SqlValue[] args, QueryEnvironment environment, ViewWrites.Checks viewChecks) throws IOException {
        Table target = table(snapshot, command.table());
        RelationPlan source = relation(command.source(), snapshot, args, environment);
        try { return new MergePlan(command, target, table(pending, command.table()), pending, source, environment, storageDatabase(), session.workMem(), args).execute(deferredReferences, viewChecks); }
        catch (IOException | RuntimeException | Error failure) { source.close(); throw failure; }
    }

    private Snapshot update(SqlParser.Update command, Snapshot snapshot, Snapshot pending, SqlValue[] args, CostMeter meter, Returning returning, QueryEnvironment environment, ViewWrites.Checks viewChecks) throws IOException {
        if (!command.from().isEmpty()) throw unsupported("UPDATE FROM");
        Table table = table(snapshot, command.table());
        CheckEvaluator checks = new CheckEvaluator(table, environment);
        ScalarPlan.Frame frame = new ScalarPlan.Frame();
        ScalarPlan.Resolver resolver = environment.resolver(table.bindings(command.alias()), frame);
        ScalarPlan.Expression predicate = predicate(resolver, command.where());
        List<ScalarPlan.Expression> assignments = new ArrayList<>(); List<Integer> targets = new ArrayList<>();
        for (SqlParser.Assignment assignment : command.assignments()) {
            int column = table.column(assignment.column());
            if (targets.contains(column)) throw new SqlException("42701", "multiple assignments to column");
            targets.add(column); Column c = table.columns.get(column);
            if (c.identity() == 2 && !(assignment.expression() instanceof SqlParser.Default)) throw new SqlException("428C9", "GENERATED ALWAYS column can only be updated to DEFAULT");
            assignments.add(assignment(resolver, assignment.expression() instanceof SqlParser.Default ? defaultOf(c) : assignment.expression(), c.type));
        }
        long beforeInitialize = meter.cost(); environment.initialize(meter); meter.writePhase("Filter", beforeInitialize);
        boolean[] changedColumns = new boolean[table.columns.size()];
        for (int target : targets) changedColumns[target] = true;
        boolean[] touched = resolver.touched();
        if (returning != null) { boolean[] extra = returning.readTouched(changedColumns); for (int c = 0; c < touched.length; c++) touched[c] |= extra[c]; }
        Table writable = table(pending, command.table());
        PersistentTree tree = writable.rows;
        PersistentTree physical = writable.rows;
        PersistentTree replacements = PersistentTree.empty(storageDatabase().pageSize(), new TableLeafCodec(table.types(), table.primaryKey()), storageDatabase().pageCodec());
        boolean primaryKeyAssigned = Arrays.stream(table.primaryKey()).anyMatch(targets::contains);
        long beforeScan = meter.cost();
        try (RowSpool checkedRows = viewChecks.present() ? new RowSpool(table.types(), session.workMem(), storageDatabase().scratchDirectory(), meter.memory()) : null;
             RowSpool replacementOrder = new RowSpool(List.of(SqlType.BYTEA), session.workMem(), storageDatabase().scratchDirectory(), meter.memory().physicalMetadata());
             IndexMutation indexes = new IndexMutation(table, environment, session.workMem(), storageDatabase().scratchDirectory()).update(primaryKeyAssigned); ForeignKeys.Batch references = new ForeignKeys.Batch(table, storageDatabase(), environment, session.workMem(), pending); var cursor = new TableScan(table, command.alias(), table.columns.stream().map(Column::name).toList(), (mutationDomain == null || mutationDomain.equals("main") || mutationDomain.equals("temp")) ? command.where() : null, args, meter, touched, environment)) {
            meter.writePhase("Scan", beforeScan);
            PersistentTree.Entry entry;
            while (true) {
                long beforeRead = meter.cost(); entry = cursor.next();
                if (entry == null) { meter.writePhase("Scan", beforeRead); break; }
                meter.charge(Costs.STORAGE_ROW_READ); meter.guard();
                frame.values = Encoding.row(entry, table.types());
                meter.writePhase("Scan", beforeRead);
                long beforeFilter = meter.cost(); boolean accepted = matches(predicate, meter); meter.writePhase("Filter", beforeFilter);
                if (!accepted) continue;
                List<SqlValue> oldValues = frame.values;
                SqlValue[] assigned = new SqlValue[table.columns.size()];
                for (int i = 0; i < assignments.size(); i++) assigned[targets.get(i)] = assignments.get(i).run(meter);
                List<SqlValue> values = new java.util.AbstractList<>() {
                    public int size() { return assigned.length; }
                    public SqlValue get(int index) { return changedColumns[index] ? assigned[index] : oldValues.get(index); }
                };
                byte[] key = table.primaryKey.length == 0 ? entry.key() : key(table, values, 0, storageDatabase().environment());
                validate(table, tree, values, entry.key(), checks, meter);
                if (replacements.getEntry(key) != null) throw new SqlException("23505", "duplicate primary key");
                indexes.add(oldValues, entry.key(), values, key);
                references.record(oldValues, values);
                if (checkedRows != null) checkedRows.append(values);
                tree = tree.remove(entry.key());
                if (primaryKeyAssigned) physical = physical.remove(entry.key());
                replacements = replacements.put(Encoding.entry(key, values, entry, changedColumns, storageDatabase().pageLayout(), meter, table.primaryKey));
                replacementOrder.append(List.of(SqlValue.ofBytes(key)));
                mutationAffected++; if (returning != null) returning.add(frame.values, values);
            }
            try (var changed = replacementOrder.cursor()) {
                List<SqlValue> ordered;
                while ((ordered = changed.next()) != null) {
                    entry = replacements.getEntry(ordered.getFirst().asBytes());
                    if (tree.getEntry(entry.key()) != null) throw new SqlException("23505", "duplicate primary key");
                    tree = tree.put(entry); physical = physical.put(entry);
                }
            }
            if (tree == writable.rows) return pending;
            Table changedTable = indexes.apply(writable.changed(physical, writable.nextRowId));
            if (checkedRows != null) try (var checked = checkedRows.cursor()) { List<SqlValue> values; while ((values = checked.next()) != null) viewChecks.accept(values); }
            return references.apply(writePhase(pending.put(indexes.checkExclusions(changedTable)), meter), deferredReferences);
        }
    }

    private Snapshot delete(SqlParser.Delete command, Snapshot snapshot, Snapshot pending, SqlValue[] args, CostMeter meter, Returning returning, QueryEnvironment environment) throws IOException {
        if (!command.using().isEmpty()) throw unsupported("DELETE USING");
        Table table = table(snapshot, command.table());
        ScalarPlan.Frame frame = new ScalarPlan.Frame();
        ScalarPlan.Resolver resolver = environment.resolver(table.bindings(command.alias()), frame);
        ScalarPlan.Expression predicate = predicate(resolver, command.where());
        long beforeInitialize = meter.cost(); environment.initialize(meter); meter.writePhase("Filter", beforeInitialize);
        boolean[] touched = resolver.touched();
        if (returning != null) { boolean[] extra = returning.readTouched(null); for (int c = 0; c < touched.length; c++) touched[c] |= extra[c]; }
        Table writable = table(pending, command.table());
        PersistentTree tree = writable.rows;
        long beforeScan = meter.cost();
        try (IndexMutation indexes = new IndexMutation(table, environment, session.workMem(), storageDatabase().scratchDirectory()); ForeignKeys.Batch references = new ForeignKeys.Batch(table, storageDatabase(), environment, session.workMem(), pending); var cursor = new TableScan(table, command.alias(), table.columns.stream().map(Column::name).toList(), (mutationDomain == null || mutationDomain.equals("main") || mutationDomain.equals("temp")) ? command.where() : null, args, meter, touched, environment)) {
            meter.writePhase("Scan", beforeScan);
            PersistentTree.Entry entry;
            while (true) {
                long beforeRead = meter.cost(); entry = cursor.next();
                if (entry == null) { meter.writePhase("Scan", beforeRead); break; }
                meter.charge(Costs.STORAGE_ROW_READ); meter.guard(); frame.values = Encoding.row(entry, table.types());
                meter.writePhase("Scan", beforeRead);
                long beforeFilter = meter.cost(); boolean accepted = matches(predicate, meter); meter.writePhase("Filter", beforeFilter);
                if (accepted) { mutationAffected++; indexes.add(frame.values, entry.key(), null, null); references.record(frame.values, null); tree = tree.remove(entry.key()); if (returning != null) returning.add(frame.values, null); }
            }
            return tree == writable.rows ? pending : references.apply(writePhase(pending.put(indexes.apply(writable.changed(tree, writable.nextRowId))), meter), deferredReferences);
        }
    }

    private Snapshot analyze(SqlParser.Analyze command, Snapshot snapshot, SqlValue[] args, QueryEnvironment environment) throws IOException {
        if (snapshot.views().containsKey(name(command.table()))) return snapshot;
        Table table = table(snapshot, command.table()); List<Integer> columns = new ArrayList<>();
        if (command.columns().isEmpty()) for (int i = 0; i < table.columns().size(); i++) columns.add(i);
        else for (String name : command.columns()) { int column = table.column(name); if (columns.contains(column)) throw new SqlException("42701", "duplicate ANALYZE column"); columns.add(column); }
        Map<Integer,ColumnStatistics> facts = new TreeMap<>(table.statistics());
        for (int column : columns) facts.put(column, ColumnStatistics.collect(table, column, args, environment));
        return snapshot.put(table.withStatistics(facts));
    }

    private QueryPlan select(SqlParser.Select query, Snapshot snapshot, SqlValue[] args, QueryEnvironment environment) {
        Snapshot roots = environment.readRoot == null ? snapshot : environment.readRoot;
        QueryPlan nested = NestedSelectPlan.resolve(query, environment, name -> DomainRouting.readDomain(roots, name).views().get(name(name)), (body, scope) -> read(body, roots, args, scope), database.scratchDirectory(), session.workMem());
        if (nested != null) return nested;
        boolean contradictory = Contradictions.proven(query.where(), args);
        if (WindowSyntax.needed(query)) {
            RelationPlan input;
            if (query.from().isEmpty()) input = new RelationPlan() {
                private boolean done;
                public List<ScalarPlan.Binding> bindings() { return List.of(); }
                public List<String> names() { return List.of(); }
                public List<SqlType> types() { return List.of(); }
                public void touch(boolean[] touched) { }
                public Row next(CostMeter meter) { if (done) return null; done = true; return new Row(List.of(), List.of()); }
                public void close() { done = true; }
            };
            else {
                SqlParser.From from = query.from().getFirst();
                for (int i = 1; i < query.from().size(); i++) from = new SqlParser.Join(from, query.from().get(i), "cross", null, List.of(), false);
                input = relation(from, snapshot, args, environment, query.where());
            }
            if (contradictory) input = Contradictions.empty(input);
            environment.captureStars(query.items(), input.bindings());
            try {
                if (query.where() != null) {
                    ScalarPlan.Expression validation = environment.resolver(input.bindings(), new ScalarPlan.Frame()).resolve(query.where(), SqlType.BOOLEAN);
                    if (validation.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("WHERE requires boolean");
                    if (AggregatePlan.needed(query)) {
                        SqlParser.Expr residual = null;
                        for (SqlParser.Expr term : FilterRules.terms(query.where())) if (!input.push(term)) residual = FilterRules.and(residual, term);
                        query = new SqlParser.Select(query.items(), residual, query.order(), query.limit(), query.offset(), query.distinct(), query.from(), query.group(), query.having(), query.distinctOn(), query.windows());
                    }


                }
                QueryPlan windowInput = input; List<ScalarPlan.Binding> windowBindings = input.bindings();
                if (AggregatePlan.needed(query) || query.windows().stream().anyMatch(w -> w.spec().partition().stream().anyMatch(AggregatePlan::contains) || w.spec().order().stream().anyMatch(o -> AggregatePlan.contains(o.expression())))) {
                    GroupedWindow.Prepared grouped = GroupedWindow.prepare(query, input, input.bindings(), args, environment, database.scratchDirectory(), session.workMem());
                    query = grouped.query(); windowInput = grouped.source(); windowBindings = grouped.bindings();
                }
                WindowPlan window = new WindowPlan(query, windowInput, windowBindings, environment);
                return new ProjectionPlan(window.rewritten(), window, window.bindings(), environment, database.scratchDirectory(), session.workMem());
            } catch (RuntimeException | Error failure) { input.close(); throw failure; }
        }
        if (query.from().isEmpty()) return AggregatePlan.needed(query) ? new AggregatePlan(query, (Table) null, null, args, database.scratchDirectory(), session.workMem(), environment) : ScalarPlan.resolve(query, args, environment);
        if (query.from().size() == 1 && query.from().getFirst() instanceof SqlParser.Table source
            && (source.name().parts().size() != 1 || environment.cte(source.name().parts().getFirst()) == null)
            && !CatalogRelation.recognized(source.name())
            && !DomainRouting.readDomain(snapshot, source.name()).views().containsKey(name(source.name()))) {
            Table table = readTable(snapshot, source.name());
            environment.viewTable(table);
            if (AggregatePlan.needed(query)) return AggregatePlan.fromTable(query, table, source, args, database.scratchDirectory(), session.workMem(), environment);
            return new TableQueryPlan(query, table, source, args, session.workMem(), database.scratchDirectory(), environment);
        }
        SqlParser.From syntax = query.from().getFirst();
        for (int i = 1; i < query.from().size(); i++) syntax = new SqlParser.Join(syntax, query.from().get(i), "cross", null, List.of(), false);
        RelationPlan source = relation(syntax, snapshot, args, environment, contradictory ? null : query.where(), query);
        if (contradictory) source = Contradictions.empty(source);
        try {
            if (query.where() != null) {
                ScalarPlan.Resolver validation = environment.resolver(source.bindings(), new ScalarPlan.Frame());
                ScalarPlan.Expression filter = validation.resolve(query.where(), SqlType.BOOLEAN);
                if (filter.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("WHERE requires boolean");
                SqlParser.Expr residual = null;
                for (SqlParser.Expr term : FilterRules.terms(query.where())) if (!source.push(term)) residual = FilterRules.and(residual, term);
                query = new SqlParser.Select(query.items(), residual, query.order(), query.limit(), query.offset(), query.distinct(), query.from(), query.group(), query.having(), query.distinctOn());
            }
            if (AggregatePlan.needed(query)) return new AggregatePlan(query, source, source.bindings(), args, database.scratchDirectory(), session.workMem(), environment);
            return new ProjectionPlan(query, source, source.bindings(), environment, database.scratchDirectory(), session.workMem());
        } catch (RuntimeException | Error failure) { source.close(); throw failure; }
    }

    private RelationPlan relation(SqlParser.From syntax, Snapshot snapshot, SqlValue[] args, QueryEnvironment environment) {
        return relation(syntax, snapshot, args, environment, null);
    }
    private RelationPlan relation(SqlParser.From syntax, Snapshot snapshot, SqlValue[] args, QueryEnvironment environment, SqlParser.Expr predicate) {
        return relation(syntax, snapshot, args, environment, predicate, null);
    }
    private RelationPlan relation(SqlParser.From syntax, Snapshot snapshot, SqlValue[] args, QueryEnvironment environment, SqlParser.Expr predicate, SqlParser.Select queryContext) {
        Snapshot roots = environment.readRoot == null ? snapshot : environment.readRoot;
        return new RelationPlanner(args, name -> readTable(roots, name),
            name -> DomainRouting.readDomain(roots, name).views().get(name(name)),
            (query, scope) -> read(query, roots, args, scope),
            (query, scope) -> values(query, args, scope, false), database.scratchDirectory(), session.workMem()).select(queryContext).relation(syntax, environment, predicate);
    }

    private static boolean writes(SqlParser.Statement statement) {
        if (statement instanceof SqlParser.Explain) return false;
        if (statement instanceof SqlParser.With with) return writes(with.body()) || with.ctes().stream().anyMatch(cte -> writes(cte.body()));
        return !(statement instanceof SqlParser.Select || statement instanceof SqlParser.Values || statement instanceof SqlParser.SetOperation || statement instanceof SqlParser.QueryTail);
    }
    private QueryEnvironment withScope(SqlParser.With with, Snapshot snapshot, SqlValue[] args, QueryEnvironment environment) {
        QueryEnvironment scope = environment.fork();
        java.util.Set<String> names = new java.util.HashSet<>();
        for (int i = 0; i < with.ctes().size(); i++) {
            SqlParser.Cte cte = with.ctes().get(i);
            if (!names.add(cte.name())) throw new SqlException("42712", "duplicate CTE name");
            SearchCycle.requireRecursive(cte, with.recursive());
            if (with.recursive() && CteReferences.count(cte.body(), cte.name()) != 0) {
                QueryEnvironment recursiveScope = scope.fork();
                scope.cte(cte.name(), new CtePlan(() -> new RecursivePlan(cte, recursiveScope), List.of(), true).correlated(recursiveScope.correlated));
                continue;
            }
            int references = CteReferences.count(with.body(), cte.name());
            boolean readReference = references != 0 && !writes(with.body());
            for (int j = i + 1; j < with.ctes().size(); j++) {
                SqlParser.Statement body = with.ctes().get(j).body(); int count = CteReferences.count(body, cte.name());
                references += count; if (count != 0 && !writes(body)) readReference = true;
            }
            boolean materialized = writes(cte.body()) || "materialized".equals(cte.materialization()) || references > 1 && !"not_materialized".equals(cte.materialization());
            QueryEnvironment bodyScope = scope.fork();
            scope.cte(cte.name(), new CtePlan(() -> read(cte.body(), snapshot, args, bodyScope), cte.columns(), materialized).correlated(bodyScope.correlated).inline(references == 1 && readReference ? cte.body() : null, bodyScope));
        }
        return scope;
    }

    private QueryPlan read(SqlParser.Statement query, Snapshot snapshot, SqlValue[] args, QueryEnvironment environment) {
        if (environment.readRoot != null) snapshot = environment.readRoot;
        if (query instanceof SqlParser.Explain explain) {
            boolean previous = environment.explainProbe(); environment.explainProbe(true); QueryPlan inner = null;
            CostMeter outcome = environment.meter(), execution = explain.analyze() ? outcome.fork() : null;
            if (execution != null) environment.meter(execution);
            try {
                if (explain.inner() instanceof SqlParser.CreateTable create) {
                    if (create.asQuery() == null) throw new SqlException("42601", "EXPLAIN requires CREATE TABLE AS query");
                    inner = read(create.asQuery(), snapshot, args, environment);
                    DomainRouting.Route route = DomainRouting.route(create, snapshot);
                    String target = create.name().parts().getLast();
                    if (target.startsWith("jed_")) throw new SqlException("42939", "reserved relation name");
                    if (relationTaken(snapshot.domain(route.domain()), target)) {
                        if (!create.ifNotExists()) throw new SqlException("42P07", "table already exists: " + target);
                        return new ExplainPlan(explain, inner, outcome, execution, true);
                    }
                    if (execution != null) inner = new AnalyzedCreatePlan(inner, () -> prepare(create, args, execution), execution, create.withData());
                } else if (explain.inner() instanceof SqlParser.CreateView) throw new SqlException("42601", "EXPLAIN requires a query or mutation");
                else inner = read(explain.inner(), snapshot, args, environment);
                if (execution != null && inner instanceof DmlExplanation metadata) inner = new AnalyzedWritePlan(metadata, () -> prepare(explain.inner(), args, execution), execution);
                else if (execution != null && (explain.inner() instanceof SqlParser.Merge || explain.inner() instanceof SqlParser.With with && writes(with))) inner = new AnalyzedStatementPlan(inner, () -> prepare(explain.inner(), args, execution), execution);
                return new ExplainPlan(explain, inner, outcome, execution);
            }
            catch (RuntimeException | Error failure) { if (inner != null) inner.close(); throw failure; }
            finally { environment.explainProbe(previous); }
        }
        if (environment.shapeOnly() && (query instanceof SqlParser.Insert || query instanceof SqlParser.Update || query instanceof SqlParser.Delete)) {
            SqlParser.Statement command = ViewWrites.expand(query, snapshot, environment).statement();
            SqlParser.Name target = DomainRouting.target(command); Table table = readTable(snapshot, target);
            String alias, oldAlias, newAlias; List<SqlParser.Item> items; boolean old;
            if (command instanceof SqlParser.Insert insert) { alias = insert.alias(); oldAlias = insert.oldAlias(); newAlias = insert.newAlias(); items = insert.returning(); old = false; }
            else if (command instanceof SqlParser.Update update) { alias = update.alias(); oldAlias = update.oldAlias(); newAlias = update.newAlias(); items = update.returning(); old = false; }
            else { SqlParser.Delete delete = (SqlParser.Delete)command; alias = delete.alias(); oldAlias = delete.oldAlias(); newAlias = delete.newAlias(); items = delete.returning(); old = true; }
            return new Returning(table, alias, items, oldAlias, newAlias, old, args, environment).finish(environment.meter());
        }
        if (environment.explainProbe() && (query instanceof SqlParser.Insert || query instanceof SqlParser.Update || query instanceof SqlParser.Delete)) {
            SqlParser.Statement command = ViewWrites.expand(query, snapshot, environment).statement();
            Table target = readTable(snapshot, DomainRouting.target(command)); Snapshot selected = snapshot;
            return new DmlExplanation(command, target, args, environment, body -> read(body, selected, args, environment));
        }
        if (query instanceof SqlParser.Merge merge) {
            if (!environment.explainProbe()) throw unsupported("MERGE as read query");
            ViewWrites.Expanded expanded = ViewWrites.expand(merge, snapshot, environment);
            SqlParser.Merge command = (SqlParser.Merge)expanded.statement(); Table target = table(snapshot, command.table());
            RelationPlan source = relation(command.source(), snapshot, args, environment);
            try { return new MergePlan(command, target, target, snapshot, source, environment, database, session.workMem(), args).metadata(); }
            catch (RuntimeException | Error failure) { source.close(); throw failure; }
        }
        if (query instanceof SqlParser.With with) { QueryEnvironment scope = withScope(with, snapshot, args, environment); return new WithPlan(with, scope, read(with.body(), snapshot, args, scope)); }
        if (query instanceof SqlParser.Select select) return environment.wrap(select(select, snapshot, args, environment));
        if (query instanceof SqlParser.Values values) return environment.wrap(values(values, args, environment));
        if (query instanceof SqlParser.SetOperation set) {
            QueryPlan left = read(set.left(), snapshot, args, environment), right = null;
            try { right = read(set.right(), snapshot, args, environment); return environment.wrap(new SetPlan(left, right, set.kind(), set.all(), database.scratchDirectory(), session.workMem())); }
            catch (RuntimeException | Error failure) { left.close(); if (right != null) right.close(); throw failure; }
        }
        if (query instanceof SqlParser.QueryTail tail) {
            if (tail.query() instanceof SqlParser.Select inner && (tail.order().isEmpty() || inner.order().isEmpty())
                && (tail.limit() == null || inner.limit() == null) && (tail.offset() == null || inner.offset() == null)) {
                return read(new SqlParser.Select(inner.items(), inner.where(), tail.order().isEmpty() ? inner.order() : tail.order(), tail.limit() == null ? inner.limit() : tail.limit(), tail.offset() == null ? inner.offset() : tail.offset(), inner.distinct(), inner.from(), inner.group(), inner.having(), inner.distinctOn(), inner.windows()), snapshot, args, environment);
            }
            if (tail.query() instanceof SqlParser.SetOperation) for (SqlParser.Order order : tail.order()) {
                SqlParser.Expr key = order.expression(); while (key instanceof SqlParser.Collate collate) key = collate.value();
                if (!(key instanceof SqlParser.Name || key instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER)) throw unsupported("set-operation ORDER BY expressions");
            }
            QueryPlan source = read(tail.query(), snapshot, args, environment);
            try {
                List<ScalarPlan.Binding> bindings = new ArrayList<>();
                for (int c = 0; c < source.types().size(); c++) bindings.add(new ScalarPlan.Binding(List.of(), source.names().get(c), source.types().get(c), false, false, source.collations().get(c).asColumn()));
                SqlParser.Select wrapper = new SqlParser.Select(List.of(new SqlParser.Item(new SqlParser.Star(List.of()), null)), null, tail.order(), tail.limit(), tail.offset(), false);
                return environment.wrap(new ProjectionPlan(wrapper, source, bindings, environment, database.scratchDirectory(), session.workMem(), false));
            } catch (RuntimeException | Error failure) { source.close(); throw failure; }
        }
        throw unsupported("query source");
    }

    private QueryPlan values(SqlParser.Values values, SqlValue[] args, QueryEnvironment environment) {
        return values(values, args, environment, true);
    }
    private QueryPlan values(SqlParser.Values values, SqlValue[] args, QueryEnvironment environment, boolean wrapperNeeded) {
        ScalarPlan.Resolver resolver = environment.resolver();
        int width = values.rows().getFirst().size();
        List<List<ScalarPlan.Expression>> rows = new ArrayList<>();
        List<SqlType> types = new ArrayList<>(); List<String> names = new ArrayList<>();
        boolean[] bareNulls = new boolean[width];
        List<CollationDerivation> collations = new ArrayList<>();
        for (int c = 0; c < width; c++) { types.add(null); names.add("column" + (c + 1)); collations.add(CollationDerivation.NONE); }
        for (List<SqlParser.Expr> row : values.rows()) {
            if (row.size() != width) throw new SqlException("42601", "VALUES rows have different widths");
            List<ScalarPlan.Expression> expressions = new ArrayList<>();
            for (int c = 0; c < width; c++) {
                if (row.get(c) instanceof SqlParser.Default) throw new SqlException("42601", "DEFAULT is only valid in INSERT VALUES");
                bareNulls[c] |= row.get(c) instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.WORD && literal.text().equals("null");
                ScalarPlan.Expression expression = resolver.resolve(row.get(c), types.get(c)); expressions.add(expression); collations.set(c, CollationDerivation.merge(collations.get(c), expression.collation()));
                try { types.set(c, types.get(c) == null ? expression.type() : ScalarPlan.common(types.get(c), expression.type())); }
                catch (SqlException failure) { if (failure.sqlState().equals("42883")) throw SqlException.typeMismatch("VALUES column types cannot be matched"); throw failure; }
            }
            rows.add(expressions);
        }
        for (int c = 0; c < width; c++) types.set(c, types.get(c) == SqlType.UNKNOWN ? SqlType.TEXT : bareNulls[c] ? types.get(c).withoutModifiers() : types.get(c));
        QueryPlan relation = new QueryPlan() {
            int index; long actual;
            public void requireAnalyze() {}
            public PlanNode actualPlan(boolean verbose) { PlanNode node = explain(verbose); PlanNode leaf = new PlanNode("Values", "rows=" + rows.size(), rows.size(), actual, List.of(), "buffered"); return wrapperNeeded ? new PlanNode(node.label(), node.detail(), node.rows(), actual, List.of(leaf), node.lane()) : leaf; }
            public List<String> names() { return List.copyOf(names); }
            public List<SqlType> types() { return List.copyOf(types); }
            public List<CollationDerivation> collations() { return List.copyOf(collations); }
            public PlanNode explain(boolean verbose) {
                long work = rows.size(); for (List<SqlParser.Expr> row : values.rows()) for (SqlParser.Expr cell : row) work = PlanEstimate.add(work, PlanEstimate.operations(cell));
                PlanNode leaf = new PlanNode("Values", "rows=" + rows.size(), rows.size(), work, List.of(), "buffered");
                return wrapperNeeded ? new PlanNode("Subquery *VALUES*", "", rows.size(), work, List.of(leaf), "buffered") : leaf;
            }
            public Row next(CostMeter meter) {
                long before = meter.cost(); try {
                if (index < rows.size()) {
                    List<ScalarPlan.Expression> expressions = rows.get(index++);
                    meter.charge(Costs.ROW_PRODUCED); meter.guard();
                    List<SqlValue> result = new ArrayList<>();
                    for (int c = 0; c < width; c++) result.add(SqlCasts.cast(expressions.get(c).run(meter), types.get(c)));
                    return new Row(names, result);
                }
                return null;
                } finally { actual += meter.cost() - before; }
            }
        };
        if (!wrapperNeeded) return relation;
        List<ScalarPlan.Binding> bindings = new ArrayList<>();
        for (int c = 0; c < width; c++) bindings.add(new ScalarPlan.Binding(List.of("*VALUES*"), names.get(c), types.get(c), false, false, collations.get(c).asColumn()));
        SqlParser.Select wrapper = new SqlParser.Select(List.of(new SqlParser.Item(new SqlParser.Star(List.of()), null)), null, values.order(), values.limit(), values.offset(), false);
        return new ProjectionPlan(wrapper, relation, bindings, environment, database.scratchDirectory(), session.workMem());
    }
    static ScalarPlan.Expression assignment(ScalarPlan.Resolver resolver, SqlParser.Expr syntax, SqlType target) {
        SqlParser.Expr unwrapped = DefaultExpression.unwrap(syntax);
        SqlType context = target.isComposite() && !(unwrapped instanceof SqlParser.RowConstructor || unwrapped instanceof SqlParser.Literal || unwrapped instanceof SqlParser.Parameter) ? null : target;
        if (unwrapped instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING && (target.isInteger() || target.kind() == SqlType.Kind.DECIMAL || target == SqlType.BOOLEAN))
            throw SqlException.typeMismatch("text literal cannot be assigned to " + target);
        ScalarPlan.Expression source = unwrapped instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING && target.isFloat()
            ? ScalarPlan.constant(SqlCasts.literal(literal.text(), target, null)) : resolver.resolve(syntax, context);
        if (target.isComposite() && source.type().kind() == SqlType.Kind.RECORD) throw CompositeValues.cannotCast();
        if ((target.kind() == SqlType.Kind.ARRAY && source.type().kind() == SqlType.Kind.ARRAY || target.kind() == SqlType.Kind.RANGE && source.type().kind() == SqlType.Kind.RANGE) && !source.type().equals(target)) throw SqlException.typeMismatch("array assignment requires the same element type");
        if (source.type() != SqlType.UNKNOWN && !source.type().equals(target) && !(source.type().isInteger() && target.isInteger())
                && !(source.type().isInteger() && target.kind() == SqlType.Kind.DECIMAL)
                && !(source.type() == SqlType.F32 && target == SqlType.F64)
                && !(source.type() == SqlType.JSONB && target == SqlType.JSON)
                && source.type().kind() != target.kind()) throw SqlException.typeMismatch("cannot assign " + source.type() + " to " + target);
        return new ScalarPlan.Expression(target, meter -> {
            SqlValue value = source.run(meter);
            if (!value.isNull() && target.kind() == SqlType.Kind.TEXT && target.maxLength() >= 0) {
                String text = value.asText();
                if (SqlText.length(text) > target.maxLength()) {
                    int cut = text.offsetByCodePoints(0, target.maxLength());
                    for (int i = cut; i < text.length(); i++) if (text.charAt(i) != ' ') throw new SqlException("22001", "value too long for " + target);
                    text = text.substring(0, cut);
                }
                return SqlValue.ofText(target, text);
            }
            return SqlCasts.cast(value, target);
        }, source.constant());
    }
    static SqlParser.Expr defaultOf(Column column) { return column.defaultValue == null ? new SqlParser.Literal(Lexer.Kind.WORD, "null") : column.defaultValue; }
    private static ScalarPlan.Expression predicate(ScalarPlan.Resolver resolver, SqlParser.Expr syntax) {
        if (syntax == null) return null;
        ScalarPlan.Expression expression = resolver.predicate(syntax);
        if (expression.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("predicate requires boolean");
        return expression;
    }
    private static boolean matches(ScalarPlan.Expression predicate, CostMeter meter) { if (predicate == null) return true; SqlValue value = predicate.run(meter); return !value.isNull() && value.asBoolean(); }
    private static long bound(ScalarPlan.Expression expression, CostMeter meter, long fallback, String state) {
        if (expression == null) return fallback;
        if (!expression.type().isInteger()) throw SqlException.typeMismatch("row bound requires integer");
        SqlValue value = expression.run(meter); if (value.isNull()) return fallback;
        if (value.asInteger() < 0) throw new SqlException(state, "negative row bound"); return value.asInteger();
    }
    static byte[] key(Table table, List<SqlValue> values, long rowId) { return key(table, values, rowId, Environment.empty()); }
    static byte[] key(Table table, List<SqlValue> values, long rowId, Environment environment) {
        Encoding.Sink out = new Encoding.Sink();
        if (table.primaryKey.length == 0) out.signed(rowId, 8);
        else for (int ordinal : table.primaryKey) out.raw(IndexCollations.key(values.get(ordinal), table.columns().get(ordinal).collation(), environment));
        return out.bytes();
    }
    static void validate(Table table, PersistentTree tree, List<SqlValue> values, byte[] excluded, CheckEvaluator checks, CostMeter meter) throws IOException {
        for (int c = 0; c < values.size(); c++) if (table.columns.get(c).notNull && values.get(c).isNull()) throw new SqlException("23502", "NULL in column " + table.columns.get(c).name);
        checks.evaluate(values, meter);
        for (int[] columns : table.unique) {
            if (table.indexes().stream().anyMatch(index -> index.constraint() && Arrays.equals(columns, index.keys().stream().mapToInt(IndexKey::column).toArray()))) continue;
            boolean hasNull = false; for (int c : columns) hasNull |= values.get(c).isNull(); if (hasNull) continue;
            try (var cursor = tree.cursor()) {
                PersistentTree.Entry entry;
                while ((entry = cursor.next()) != null) {
                    if (excluded != null && Arrays.equals(entry.key(), excluded)) continue;
                    List<SqlValue> other = Encoding.row(entry, table.types());
                    boolean same = true;
                    for (int c : columns) same &= !other.get(c).isNull() && Arrays.equals(Encoding.key(values.get(c)), Encoding.key(other.get(c)));
                    if (same) throw new SqlException("23505", "duplicate unique value");
                }
            }
        }
    }
    private static Table readTable(Snapshot snapshot, SqlParser.Name syntax) {
        return table(DomainRouting.readDomain(snapshot, syntax), DomainRouting.local(syntax));
    }
    private static Table table(Snapshot snapshot, SqlParser.Name syntax) {
        String name = name(syntax); Table table = snapshot.tables.get(name);
        if (table == null && snapshot.views().containsKey(name)) throw new SqlException("55000", "view is not updatable");
        if (table == null) throw new SqlException("42P01", "table does not exist: " + name); return table;
    }
    static String name(SqlParser.Name syntax) {
        if (syntax.parts().size() > 2) throw new SqlException("42P01", "database is not attached");
        return syntax.parts().getLast();
    }
    static QueryPlan emptyPlan() {
        return new QueryPlan() { public List<String> names() { return List.of(); } public List<SqlType> types() { return List.of(); } public Row next(CostMeter meter) { return null; } };
    }
    private static QueryPlan pinned(QueryPlan plan, Database.Pin pin, QueryEnvironment environment) {
        return new QueryPlan() {
            public List<String> names() { return plan.names(); }
            public List<SqlType> types() { return plan.types(); }
            public Row next(CostMeter meter) { pin.check(); return plan.next(meter); }
            public void close() { try { plan.close(); } finally { try { environment.close(); } finally { pin.close(); } } }
        };
    }
    static SqlException storageError(IOException error) {
        if (error instanceof org.jed.host.PageConnection.ConnectionLost) return new SqlException("08006", error.getMessage());
        if (error instanceof org.jed.host.PageConnection.RemoteFailure remote) return new SqlException(remote.sqlState(), error.getMessage());
        if (error instanceof org.jed.host.PageProtocol.Violation) return new SqlException("08P01", error.getMessage());
        return new SqlException(error instanceof org.jed.storage.CorruptStorageException ? "XX001" : error instanceof TreeCapacityException ? "0A000" : "58030", error.getMessage());
    }
    private static SqlException unsupported(String feature) { return new SqlException("0A000", feature + " is not implemented yet"); }
}
