package org.jed;

import java.io.IOException;
import java.nio.file.Path;
import java.util.*;

/** Per-domain histories retain attachment durability independently of main. */
final class SimulationDomains {
    static final class State {
        final String name;
        Database database;
        int key;
        long version = 1, floor, attempted = 1;
        String committed, pending;
        boolean batchOpen;
        SimulationGrowth growth;
        FileDatabase.Inspection before;
        final Map<Long,Set<String>> history = new TreeMap<>();
        State(String name, int key) { this.name = name; this.key = key; }
        String path() { return name + ".jed"; }
        void remember(long version, String digest) { history.computeIfAbsent(version, ignored -> new HashSet<>()).add(digest); attempted = Math.max(attempted, version); }
        void verify(String digest) { require(version >= floor && version <= attempted, "attachment " + name + " outside durability window"); require(history.getOrDefault(version, Set.of()).contains(digest), "attachment " + name + " recovered unrecorded contents"); }
    }
    final TreeMap<String,State> states = new TreeMap<>(SqlText::compare);
    private final SimulationFileSystem fs;
    private final boolean encrypted;
    private final Credential[] keys;
    SimulationDomains(SimulationFileSystem fs, boolean encrypted, Credential[] keys) { this.fs = fs; this.encrypted = encrypted; this.keys = keys; }
    private String digest(Database main, State state) { try (Session session = main.session()) { return SimulationRows.digest(session, fs, state.name); } }
    long attach(Database main, String name, java.util.function.Supplier<FileOptions> createOptions, FileOptions processOptions) throws IOException {
        State state = states.get(name);
        if (state != null && state.database != null) return state.version;
        if (state == null) {
            state = new State(name, encrypted ? 0 : -1); state.growth = new SimulationGrowth(fs, state.path()); states.put(name, state);
            StringBuilder empty = new StringBuilder(); for (String relation : List.of("jed_tables", "jed_columns", "jed_indexes", "jed_constraints", "jed_sequences")) empty.append("SELECT * FROM ").append(name).append('.').append(relation).append("\n\n\n"); state.remember(1, SimulationRows.fold(empty.toString()));
        }
        if (!fs.exists(Path.of(state.path()))) { try (Database created = Database.create(Path.of(state.path()), createOptions.get().toBuilder().credential(state.key < 0 ? null : keys[state.key]).build())) { } state.floor = 1; }
        bind(main, state, processOptions); state.version = state.database.processTxid(); state.committed = digest(main, state); state.verify(state.committed); state.growth.reset(state.database); return state.version;
    }
    private void bind(Database main, State state, FileOptions options) { main.attach(state.name, AttachSource.file(Path.of(state.path()), options.toBuilder().credential(state.key < 0 ? null : keys[state.key]).build()), false); state.database = main.domainDatabase(null, state.name); }
    void reattach(Database main, FileOptions options) throws IOException {
        for (State state : List.copyOf(states.values())) {
            if (!fs.exists(Path.of(state.path()))) { require(state.floor == 0, "durable attachment disappeared"); states.remove(state.name); continue; }
            boolean opened = false;
            for (int candidate : new LinkedHashSet<>(List.of(state.key, -1, 0, 1, 2, 3))) {
                if (!encrypted && candidate != -1) continue; state.key = candidate;
                try { bind(main, state, options); opened = true; break; } catch (SqlException failure) { if (!Set.of("28P01", "28P02", "28P03").contains(failure.sqlState())) throw failure; }
            }
            require(opened, "attachment cannot unlock"); state.version = state.database.processTxid(); state.committed = digest(main, state); state.verify(state.committed); state.growth.reset(state.database);
        }
    }
    void bindExisting(Database main, FileOptions options) { for (State state : states.values()) {
        if (!main.attachmentNames().contains(state.name)) bind(main, state, options);
    } }
    void select(Database main, java.util.function.Function<String,Database> owner) { for (State state : states.values()) {
        Database serving = owner.apply(state.path()); state.database = serving == null ? main.domainDatabase(null, state.name) : serving;
    } }
    Map<String,Long> cachedVersions() { Map<String,Long> result = new TreeMap<>(SqlText::compare); for (State state : states.values()) if (state.database != null) result.put(state.name, state.database.publishedVersionForTesting()); return result; }
    Map<String,Long> versions() { Map<String,Long> result = new TreeMap<>(SqlText::compare); for (State state : states.values()) if (state.database != null) result.put(state.name, state.database.publishedVersionForTesting()); return result; }
    void begin(long clock) throws IOException { for (State state : states.values()) { state.pending = null; state.before = state.database == null || state.database.cachedRoleForTesting().equals("client") ? null : state.growth.inspect(state.database); state.batchOpen = state.database != null && state.database.batchOpenForTesting(clock); } }
    void pending(Session session) { for (State state : states.values()) state.pending = SimulationRows.digest(session, fs, state.name); }
    void committed(Database main, boolean quiet, boolean localGrowth, boolean verify) throws IOException {
        for (State state : states.values()) {
            long old = state.version; state.version = state.database.publishedVersionForTesting(); require(state.version == old || state.version == old + 1, "attachment invalid generation transition");
            String value = state.version == old ? state.committed : state.pending; require(value != null, "attachment published without pending digest");
            if (state.version == old) require(state.pending == null || value.equals(state.pending), "unchanged attachment commit discarded pending contents");
            else if (!fs.dead && verify) { String actual = digest(main, state); require(value.equals(actual), "attachment " + state.name + " differs from pending contents: generation " + old + " -> " + state.version + ", expected " + value + ", actual " + actual + ", pending " + state.pending); }
            state.committed = value; state.remember(state.version, value);
            if (state.version != old && fs.entryDurable(state.path())) state.floor = Math.max(state.floor, state.database.durableGenerationForTesting());
            if (!fs.dead && verify) { SimulationStorageChecks.live(state.database); if (state.version != old && localGrowth) state.growth.committed(state.database, state.before, quiet); }
        }
    }
    void attempts() { for (State state : states.values()) if (state.pending != null && !fs.written.getOrDefault(state.path(), Set.of()).isEmpty()) state.remember(state.version + 1, state.pending); }
    void barriers(Map<String,Long> starts) { for (State state : states.values()) if (fs.durableBarriers.contains(state.path()) && !state.batchOpen) state.floor = Math.max(state.floor, starts.getOrDefault(state.name, 0L)); }
    void closed(Map<String,Long> versions) throws IOException { for (State state : states.values()) if (fs.entryDurable(state.path()) && fs.contentDurable(state.path())) state.floor = Math.max(state.floor, versions.getOrDefault(state.name, 0L)); }
    void reader(Session session, Map<String,Long> versions) { for (var entry : versions.entrySet()) require(states.get(entry.getKey()).history.get(entry.getValue()).contains(SimulationRows.digest(session, fs, entry.getKey())), "attachment pinned reader changed"); }
    Map<String,Object> fields(Database main, boolean recovered, boolean summary, int pageSize, Map<String,byte[]> images) throws IOException {
        Map<String,Object> result = new TreeMap<>(SqlText::compare);
        for (State state : states.values()) {
            state.version = state.database.processTxid(); String digest = digest(main, state); state.verify(digest);
            Map<String,Object> data = new LinkedHashMap<>(); data.put(recovered ? "recovered" : "version", state.version); data.put("digest", digest);
            if (!summary) { var facts = SimulationStorageChecks.live(state.database); byte[] bytes = images == null ? fs.image(state.path()) : images.get(state.name);
                var copy = new SimulationFileSystem(1, pageSize, 64); copy.install(state.path(), bytes);
                try (var reopened = FileDatabase.openHost(null, copy, Path.of(state.path()), 0, state.key < 0 ? null : keys[state.key], Environment.empty(), true, false, null); var lease = reopened.backupLease()) {
                    var disk = SimulationStorageChecks.inspect(lease.snapshot(), lease.pager(), lease.selected()); require(disk.version() == state.version && facts.reachable().equals(disk.reachable()), "attachment live roots differ from independently reopened image");
                }
                data.put("live", facts.live()); data.put("pages", facts.pages()); data.put("free", facts.free()); data.put("image", SimulationRows.fold(Arrays.copyOf(bytes, Math.toIntExact(facts.pages() * pageSize)))); }
            result.put(state.name, data);
        }
        return result;
    }
    private static void require(boolean condition, String message) { SimulationStorageChecks.require(condition, message); }
}
