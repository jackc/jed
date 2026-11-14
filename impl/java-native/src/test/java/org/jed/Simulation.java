package org.jed;

import java.io.IOException;
import java.nio.file.*;
import java.util.*;

/** Executes complete frozen traces twice and compares transcripts and byte-device calls. */
public final class Simulation {
    private record Handle(Session session, boolean write, long version, Map<String,Long> attachments, SeededRandom random, int process) { }
    private final SimulationTrace trace;
    private final SimulationFileSystem fs;
    private final Map<String,Handle> sessions = new TreeMap<>();
    private record Process(Database database, FileOptions options) { }
    private final TreeMap<Integer,Process> processes = new TreeMap<>();
    private final SimulationNetwork network = new SimulationNetwork();
    private SimulationRoleRegistry roles = new SimulationRoleRegistry(network);
    private int selectedProcess, backups;
    private record Backup(int number, String path, long version, String digest, int key) { }
    private final List<Backup> successfulBackups = new ArrayList<>();
    private boolean ownerLost;
    private final Map<Long,Set<String>> history = new TreeMap<>();
    private final List<String> transcript = new ArrayList<>();
    private SeededRandom random;
    private Database database;
    private FileOptions processOptions;
    private final SimulationDomains domains;
    private long clock, openings, version, floor, attempted;
    private final SimulationGrowth growth;
    private int key;
    private final Credential[] credentials = new Credential[4];
    private SimulationFileSystem.Fault armed;
    private String committed, pending;
    private record KeyAttempt(int oldKey, int newKey, boolean entryPending, boolean acknowledged) { }
    private KeyAttempt keyAttempt;
    private final Map<String,KeyAttempt> attachmentKeyAttempts = new TreeMap<>();
    private Simulation(SimulationTrace trace) {
        this.trace = trace; fs = new SimulationFileSystem(trace.seed(), trace.pageSize(), trace.sector()); clock = trace.clock(); random = new SeededRandom(trace.seed()); key = trace.encrypted() ? 0 : -1;
        domains = new SimulationDomains(fs, trace.encrypted(), credentials);
        growth = new SimulationGrowth(fs, "sim.jed");
        for (int i = 0; i < credentials.length; i++) { byte[] bytes = new byte[32]; Arrays.fill(bytes, (byte)(0x4a + i)); credentials[i] = Credential.hostKey(bytes); }
    }
    public static void main(String[] args) throws Exception {
        if (args.length != 1) throw new IllegalArgumentException("simulation requires one complete .sim trace");
        var trace = SimulationTrace.read(Path.of(args[0])); var first = new Simulation(trace); var second = new Simulation(trace);
        first.execute(); second.execute(); require(first.transcript.equals(second.transcript), "transcript is not self-deterministic"); require(first.fs.operations.equals(second.fs.operations), "byte-device operations are not self-deterministic");
        for (String line : first.transcript) System.out.println(line);
    }
    private FileOptions options() {
        long entropySeed = SimulationFileSystem.decide(trace.seed(), 14, openings++, selectedProcess, 0);
        
        Environment environment = Environment.builder().clockSource(() -> clock).randomSource(bytes -> random.accept(bytes)).keyEntropy(() -> new SeededRandom(entropySeed)).build();
        return FileOptions.builder().storageHost(fs).scratchHost(fs.scratchHost()).roleHost(trace.service() ? roles.host(selectedProcess) : null).serve(trace.service()).shutdownGraceMillis(0).idleSessionMillis(0).idleTransactionMillis(0).idleFlush(false).ownership(FileOptions.Ownership.NEVER).pageSize(trace.pageSize())
            .cacheBytes(trace.cacheBytes() == 0 ? 268435456 : trace.cacheBytes()).synchronous(trace.synchronous()).flushIntervalMillis(trace.flushInterval())
            .environment(environment).credential(key < 0 ? null : credentials[key]).protectedScratch(trace.encrypted()).maxStorageBytes(trace.maxStorage()).build();
    }
    private void execute() throws Exception {
        // Explicit admission until each driver surface is implemented; never report a partial trace as passing.
        for (var step : trace.steps()) if (!Set.of("open", "on", "commit", "rollback", "close", "fault", "crash", "reopen", "compact", "check", "clock", "change_key", "rekey", "attach", "spawn", "backup", "exit", "kill", "disconnect", "corrupt", "restore", "replay").contains(step.operation())) throw new UnsupportedOperationException("simulation operation " + step.operation());
        database = Database.create(Path.of("sim.jed"), options()); database.close(); database = Database.open(Path.of("sim.jed"), processOptions = options());
        if (trace.service()) { processes.put(0, new Process(database, processOptions)); network.settle(); }
        version = floor = attempted = database.processTxid(); committed = digest(); remember(version, committed); growth.reset(storageDatabase());
        for (var step : trace.steps()) {
            try { step(step); }
            catch (Throwable failure) {
                Path path = Path.of("/tmp/java-simulation-" + trace.name()); Files.write(Path.of(path + ".img"), fs.image("sim.jed")); Files.write(Path.of(path + ".jsonl"), transcript);
                throw new IllegalStateException(trace.name() + ":" + step.line() + " step " + step.index() + " " + step.operation() + ": " + failure.getMessage(), failure);
            }
        }
        cleanClose();
    }
    private void configure(Session session) { session.setWorkMem(trace.workMem()); session.setTempBuffers(trace.tempBuffers() == 0 ? 33554432 : trace.tempBuffers()); session.setMaxQueryMemoryBytes(trace.queryMemory()); }
    private String digest() { try (Session session = database.session()) { return SimulationRows.digest(session, fs); } }
    private void remember(long generation, String value) { history.computeIfAbsent(generation, ignored -> new HashSet<>()).add(value); attempted = Math.max(attempted, generation); }
    private Database storageDatabase() { Database owner = trace.service() ? roles.database("sim.jed") : null; return owner == null ? database : owner; }
    private void cleanClose() throws IOException {
        for (Handle handle : sessions.values()) handle.session.close(); sessions.clear();
        long closing = version; var attachments = domains.cachedVersions();
        if (trace.service()) { for (Process process : processes.values()) process.database.close(); processes.clear(); network.settle(); }
        else database.close();
        domains.closed(attachments); if (!ownerLost && fs.entryDurable("sim.jed") && fs.contentDurable("sim.jed")) floor = Math.max(floor, closing);
    }
    private static int parameter(List<String> args, String name, int fallback) { for (String arg : args) if (arg.startsWith(name + "=")) return Integer.parseInt(arg.substring(name.length() + 1)); return fallback; }
    private void step(SimulationTrace.Step step) throws Exception {
        List<String> args = step.arguments(); String operation = step.operation();
        if (operation.equals("fault")) {
            String kind = args.getFirst(); if (!Set.of("power_loss", "process_crash", "io_error", "read_error", "enospc", "disconnect", "spill_error").contains(kind)) throw new UnsupportedOperationException("simulation fault " + kind);
            String op = args.stream().filter(a -> a.startsWith("op=")).map(a -> a.substring(3)).findFirst().orElse("");
            armed = new SimulationFileSystem.Fault(kind, parameter(args, kind.equals("power_loss") ? "survive" : kind.equals("process_crash") ? "issue" : "p", 0), parameter(args, "tear", 0), parameter(args, "nth", 1), op);
            emit(event(step).put("kind", kind)); return;
        }
        keyAttempt = null; attachmentKeyAttempts.clear();
        if (Set.of("corrupt", "restore", "replay").contains(operation)) { fs.begin(step.index(), null); if (operation.equals("corrupt")) corrupt(step); else if (operation.equals("restore")) restore(step); else replay(step); return; }
        if (trace.service() && Set.of("kill", "exit", "disconnect").contains(operation)) {
            int pid = processNumber(args.getFirst()); Event event = event(step).put("pid", pid); Process process = processes.get(pid);
            if (process == null) event.put("skip", true);
            else if (operation.equals("disconnect")) { if (!process.database.cachedRoleForTesting().equals("client")) event.put("skip", true); else network.disconnect(pid, "sim.jed"); }
            else {
                endProcess(pid, operation.equals("kill"));
                if (operation.equals("kill") && processes.isEmpty()) { fs.begin(step.index(), null); recover(step.index(), "none"); return; }
            }
            network.settle(); emit(event); return;
        }
        if (trace.service()) {
            if (processes.isEmpty() && !operation.equals("spawn") && !operation.equals("crash")) { spawn(0); Event spawned = new Event(step.index(), "spawn").put("pid", 0).put("version", database.processTxid()); domainVersions(spawned); emit(spawned); }
            int pid = operation.equals("open") ? args.size() > 2 ? processNumber(args.get(2)) : 0
                : operation.equals("backup") ? args.size() > 1 ? processNumber(args.get(1)) : processes.isEmpty() ? 0 : processes.firstKey()
                : Set.of("on", "commit", "rollback", "close").contains(operation) && sessions.containsKey(args.getFirst()) ? sessions.get(args.getFirst()).process
                : processes.isEmpty() ? 0 : processes.firstKey();
            Process process = processes.get(pid);
            if (process == null && (operation.equals("open") || operation.equals("backup"))) { emit(event(step).put(operation.equals("open") ? "sid" : "pid", operation.equals("open") ? args.getFirst() : pid).put("skip", true)); return; }
            if (process != null) { selectedProcess = pid; database = process.database; processOptions = process.options; domains.select(database, roles::database); }
            network.begin(armed != null && armed.kind().equals("disconnect") ? armed.nth() : 0);
        }
        var active = armed; armed = null; fs.begin(step.index(), active != null && active.kind().equals("disconnect") ? null : active); pending = null; domains.begin(clock); Map<String,Long> domainStarts = domains.versions();
        long start = version; boolean batchOpen = storageDatabase().batchOpenForTesting(clock), acknowledged = false, oracleUnavailable = false;
        if (operation.equals("crash")) { fs.crash(args.getFirst().equals("power"), parameter(args, "survive", 0), parameter(args, "tear", 0)); recover(step.index(), "none"); return; }
        Event event = event(step); if (Set.of("open", "on", "commit", "rollback", "close").contains(operation)) event.put("sid", args.getFirst());
        try {
            switch (operation) {
                case "spawn" -> {
                    int pid = processNumber(args.getFirst()); event.put("pid", pid);
                    if (!trace.service() || processes.containsKey(pid)) { event.put("skip", true); break; }
                    spawn(pid);
                    long observed = database.processTxid(); require(observed >= floor && observed <= attempted && history.getOrDefault(observed, Set.of()).contains(digest()), "spawn observed unrecorded contents");
                    event.put("version", observed); domainVersions(event);
                }
                case "backup" -> backup(step, event);
                case "open" -> {
                    String sid = args.getFirst(); boolean write = args.get(1).equals("write"); require(write || args.get(1).equals("read"), "invalid session mode");
                    require(!sessions.containsKey(sid) && (!write || sessions.values().stream().noneMatch(Handle::write)), "duplicate session/second writer");
                    random = new SeededRandom(SimulationFileSystem.decide(trace.seed(), 8, step.index(), 0, 0)); Session session = write ? database.session() : database.read(); configure(session); if (write) session.execute("BEGIN");
                    observe();
                    sessions.put(sid, new Handle(session, write, database.processTxid(), domains.versions(), random, selectedProcess)); event.put("version", database.processTxid()); domainVersions(event);
                }
                case "attach" -> { require(sessions.values().stream().noneMatch(Handle::write), "attachment opened while a writer is held"); String name = args.getFirst(); event.put("db", name).put("version", domains.attach(database, name, this::options, processOptions)); if (trace.service()) for (var process : processes.values()) domains.bindExisting(process.database, process.options); }
                case "on" -> { Handle handle = sessions.get(args.getFirst()); if (handle == null) event.put("skip", true); else { random = handle.random; var result = SimulationRows.query(handle.session, step.sql()); event.put("rows", result.rendered().size()).put("affected", result.affected()).put("hash", result.hash()).put("cost", result.cost()); } }
                case "commit" -> {
                    Handle handle = sessions.remove(args.getFirst()); if (handle == null) { event.put("skip", true); break; }
                    try {
                        random = handle.random;
                        try { if (handle.write) { pending = SimulationRows.digest(handle.session, fs); domains.pending(handle.session); } } catch (SqlException failure) { if (failure.sqlState().equals("XX001")) throw failure; }
                        long before = database.publishedVersionForTesting(); var beforeStorage = trace.service() ? null : inspection();
                        try { if (handle.write) handle.session.execute("COMMIT"); } catch (RuntimeException failure) { require(database.publishedVersionForTesting() == before, "failed commit published a generation"); require(domains.versions().equals(domainStarts), "failed commit published an attachment"); throw failure; }
                        acknowledged = true; version = database.publishedVersionForTesting(); require(version == start || version == start + 1, "invalid commit generation transition");
                        String result = version == start ? committed : pending; require(result != null, "published commit missing in-flight digest");
                        if (!fs.dead && version != start) {
                            try { String read = digest(); require(result.equals(read), "acknowledged commit differs from pending values"); }
                            catch (SqlException failure) {
                                if (trace.service() && failure.sqlState().equals("08006") && (fs.dead || fs.fired.equals("write"))) oracleUnavailable = true;
                                else throw failure;
                            }
                        }
                        committed = result; remember(version, result); if (version != start && fs.entryDurable("sim.jed")) floor = Math.max(floor, storageDatabase().durableGenerationForTesting());
                        if (!fs.dead && !oracleUnavailable) { oracle(); if (version != start && !trace.service()) growth.committed(storageDatabase(), beforeStorage, active == null && sessions.isEmpty()); } event.put("version", version); domains.committed(database, active == null && sessions.isEmpty(), !trace.service(), !oracleUnavailable); domainVersions(event); if (trace.service()) event.put("role", database.cachedRoleForTesting());
                    } finally { handle.session.close(); }
                }
                case "rollback", "close" -> {
                    Handle handle = sessions.remove(args.getFirst()); if (handle == null) { event.put("skip", true); break; }
                    try {
                        random = handle.random;
                        try { if (!handle.write && operation.equals("close")) require(history.get(handle.version).contains(SimulationRows.digest(handle.session, fs)), "pinned reader changed contents"); if (!handle.write) domains.reader(handle.session, handle.attachments); }
                        catch (SqlException failure) { if (trace.service() && failure.sqlState().equals("08006")) event.put("lost", true); else throw failure; }
                        if (handle.write && operation.equals("rollback")) handle.session.execute("ROLLBACK");
                    } finally { handle.session.close(); }
                }
                case "clock" -> clock = Math.addExact(clock, Long.parseLong(args.getFirst()));
                case "reopen" -> { cleanClose(); selectedProcess = 0; database = Database.open(Path.of("sim.jed"), processOptions = options()); if (trace.service()) { processes.put(0, new Process(database, processOptions)); network.settle(); } domains.reattach(database, processOptions); version = database.processTxid(); resetGrowth(); committed = digest(); require(history.get(version).contains(committed), "clean reopen changed contents"); event.put("version", version).put("digest", committed); domainFields(event, false, true); }
                case "check" -> { if (trace.service()) observe(); fields(event, false, fs.image("sim.jed")); domainFields(event, false, false); }
                case "change_key" -> {
                    if (args.size() > 1 && !args.get(1).equals("main")) { attachmentMaintenance(step, event, args.get(1)); break; }
                    if (!trace.encrypted()) { event.put("skip", true); break; }
                    int target = Integer.parseInt(args.getFirst()); keyAttempt = new KeyAttempt(key, target, !fs.entryDurable("sim.jed"), false);
                    database.changeCredential(credentials[target]); keyAttempt = new KeyAttempt(key, target, keyAttempt.entryPending, true); key = target; event.put("key", key);
                }
                case "compact", "rekey" -> {
                    String targetName = operation.equals("compact") ? args.isEmpty() ? "main" : args.getFirst() : args.size() > 1 ? args.get(1) : "main";
                    if (!targetName.equals("main")) { attachmentMaintenance(step, event, targetName); break; }
                    if (operation.equals("rekey") && !trace.encrypted()) { event.put("skip", true); break; }
                    pending = committed; remember(start + 1, pending);
                    if (operation.equals("compact")) database.compact("main"); else { int target = args.getFirst().equals("plain") ? -1 : Integer.parseInt(args.getFirst()); keyAttempt = new KeyAttempt(key, target, !fs.entryDurable("sim.jed"), false); database.rekey(target < 0 ? null : credentials[target]); keyAttempt = new KeyAttempt(key, target, keyAttempt.entryPending, true); key = target; }
                    version = database.processTxid(); require(version == start + 1 && digest().equals(pending), "maintenance changed contents or generation incorrectly"); remember(version, pending); floor = version; resetGrowth(); event.put("version", version);
                }
                default -> throw new UnsupportedOperationException(operation);
            }
        } catch (SqlException failure) {
            if (acknowledged) throw new AssertionError("acknowledged commit failed its post-publication oracle", failure);
            if (failure.sqlState().equals("XX001")) throw new AssertionError("unexpected corruption outside corruption step", failure);
            event.put("error", failure.sqlState()); recordAttempt(start);
        } catch (IOException failure) {
            if (acknowledged) throw new AssertionError("acknowledged commit failed its post-publication oracle", failure);
            event.put("error", "58030"); recordAttempt(start);
        }
        if (trace.service()) network.settle();
        emit(event);
        if (trace.service() && operation.equals("commit") && acknowledged && !fs.dead && !oracleUnavailable) {
            Database owner = storageDatabase(); long current = owner.processTxid();
            if (current > version) {
                require(current == version + 1, "owner maintenance advanced more than one generation");
                try (Session read = owner.session()) { require(committed.equals(SimulationRows.digest(read, fs)), "owner maintenance changed logical contents"); }
                version = current; remember(current, committed); if (fs.entryDurable("sim.jed")) floor = Math.max(floor, owner.durableGenerationForTesting());
                emit(new Event(step.index(), "maintenance").put("version", current));
            }
        }
        domains.barriers(domainStarts);
        if (fs.durableBarriers.contains("sim.jed") && !batchOpen) floor = Math.max(floor, start);
        if (active != null && active.kind().equals("disconnect")) { emit(new Event(step.index(), "disconnected").put("fired", network.disconnected ? "frame" : "end")); return; }
        if (active != null) {
            fs.finishFault();
            if (trace.service() && !active.kind().equals("power_loss")) {
                var owners = new TreeSet<Integer>(); Integer mainOwner = roles.owner("sim.jed"); if (mainOwner != null) owners.add(mainOwner);
                for (var state : domains.states.values()) { Integer owner = roles.owner(state.path()); if (owner != null) owners.add(owner); }
                if (owners.isEmpty()) owners.add(selectedProcess);
                if (owners.size() < processes.size()) {
                    String fired = fs.fired; for (int pid : owners) endProcess(pid, true); fs.restart();
                    for (int pid : owners) emit(new Event(step.index(), "killed").put("pid", pid).put("fired", fired));
                    if (keyAttempt != null) settleKey(step.index());
                    settleAttachmentKeys(step.index(), attachmentImages());
                    return;
                }
            }
            recover(step.index(), fs.fired);
        }
    }
    private void settleKey(int step) throws IOException {
        byte[] image = fs.image("sim.jed"); boolean oldWorks = unlocks(image, keyAttempt.oldKey), newWorks = unlocks(image, keyAttempt.newKey);
        require(keyAttempt.oldKey == keyAttempt.newKey ? oldWorks : oldWorks && !newWorks && !keyAttempt.acknowledged || !oldWorks && newWorks || keyAttempt.entryPending && !oldWorks && !newWorks,
            "credential recovery violates atomic replacement or acknowledged durability");
        key = newWorks ? keyAttempt.newKey : oldWorks ? keyAttempt.oldKey : key;
        emit(new Event(step, "unlocked").put("key", key));
    }
    private void recordAttempt(long start) { if (pending != null && !fs.written.getOrDefault("sim.jed", Set.of()).isEmpty()) remember(start + 1, pending); domains.attempts(); }
    private void recover(int step, String fired) throws Exception {
        if (trace.service()) { for (int pid : List.copyOf(processes.keySet())) endProcess(pid, true); roles = new SimulationRoleRegistry(network); selectedProcess = 0; }
        sessions.clear(); fs.restart(); var images = attachmentImages(); byte[] image = fs.image("sim.jed"); int oldKey = key; boolean opened = false; FileOptions opening = processOptions = options();
        if (keyAttempt != null) {
            boolean oldWorks = unlocks(image, keyAttempt.oldKey), newWorks = unlocks(image, keyAttempt.newKey);
            require(keyAttempt.oldKey == keyAttempt.newKey ? oldWorks : oldWorks && !newWorks && !keyAttempt.acknowledged || !oldWorks && newWorks || keyAttempt.entryPending && !oldWorks && !newWorks,
                "credential recovery violates atomic replacement or acknowledged durability");
        }
        for (int candidate : new LinkedHashSet<>(List.of(key, -1, 0, 1, 2, 3))) {
            if (!trace.encrypted() && candidate != -1) continue; key = candidate;
            try { database = Database.open(Path.of("sim.jed"), opening.toBuilder().credential(key < 0 ? null : credentials[key]).build()); opened = true; break; }
            catch (SqlException failure) { if (!Set.of("28P01", "28P02", "28P03").contains(failure.sqlState())) throw failure; }
        }
        if (trace.service()) { processes.put(0, new Process(database, processOptions)); network.settle(); }
        require(opened, "no authorized credential unlocks recovery"); if (key != oldKey || keyAttempt != null) emit(new Event(step, "unlocked").put("key", key));
        settleAttachmentKeys(step, images);
        domains.reattach(database, processOptions); version = database.processTxid(); resetGrowth(); require(version >= floor && version <= attempted, "recovered version " + version + " outside durability window " + floor + ".." + attempted);
        committed = digest(); require(history.getOrDefault(version, Set.of()).contains(committed), "recovered unrecorded contents");
        Event event = new Event(step, "crash").put("fired", fired); fields(event, true, image); domainFields(event, true, false, images); emit(event);
    }
    private FileDatabase.Inspection inspection() throws IOException { return growth.inspect(storageDatabase()); }
    private void resetGrowth() throws IOException { growth.reset(storageDatabase()); }
    private void oracle() throws IOException { boolean was = fs.oracleReads; fs.oracleReads = true; try { SimulationStorageChecks.live(storageDatabase()); } finally { fs.oracleReads = was; } }
    private void fields(Event event, boolean recovered, byte[] image) throws Exception {
        String digest = digest(); oracle(); var facts = SimulationStorageChecks.live(storageDatabase());
        // An independent reopened copy checks both selected-meta recovery and persisted references.
        var copy = new SimulationFileSystem(trace.seed(), trace.pageSize(), trace.sector()); copy.install("sim.jed", image);
        try (var reopened = FileDatabase.openHost(null, copy, Path.of("sim.jed"), trace.cacheBytes(), key < 0 ? null : credentials[key], Environment.empty(), true, false, null); var lease = reopened.backupLease()) {
            var disk = SimulationStorageChecks.inspect(lease.snapshot(), lease.pager(), lease.selected()); require(disk.version() == version && facts.reachable().equals(disk.reachable()), "live roots differ from independently reopened image: expected=" + version + " live=" + facts.version() + ":" + facts.reachable() + " disk=" + disk.version() + ":" + disk.reachable());
        }
        event.put(recovered ? "recovered" : "version", version).put("digest", digest).put("live", facts.live()).put("pages", facts.pages()).put("free", facts.free()).put("image", SimulationRows.fold(Arrays.copyOf(image, Math.toIntExact(facts.pages() * trace.pageSize()))));
        String output = System.getenv("JED_SIM_OUT"); if (output != null && !output.isEmpty()) { Files.createDirectories(Path.of(output)); Files.write(Path.of(output, trace.name().replace(".sim", "") + ".java-native.step" + fs.step + ".img"), image); }
    }
    private void spawn(int pid) throws IOException {
        selectedProcess = pid; processOptions = options(); database = Database.open(Path.of("sim.jed"), processOptions);
        processes.put(pid, new Process(database, processOptions)); domains.bindExisting(database, processOptions); network.settle(); observe();
    }
    private void observe() {
        if (!trace.service()) return;
        String value = digest(); long observed = database.processTxid();
        require(observed >= (ownerLost ? floor : version) && observed <= attempted && history.getOrDefault(observed, Set.of()).contains(value), "process observed an invalid generation or digest: observed=" + observed + " floor=" + floor + " version=" + version + " attempted=" + attempted + " ownerLost=" + ownerLost + " digest=" + value + " history=" + history.get(observed));
        if (ownerLost) { try { growth.reset(storageDatabase()); } catch (IOException failure) { throw Engine.storageError(failure); } }
        version = observed; committed = value; ownerLost = false;
    }
    private void endProcess(int pid, boolean kill) throws IOException {
        Process process = processes.remove(pid); if (process == null) return;
        for (String sid : List.copyOf(sessions.keySet())) if (sessions.get(sid).process == pid) { Handle handle = sessions.remove(sid); if (!kill) handle.session.close(); }
        if (kill) { if (Objects.equals(roles.owner("sim.jed"), pid)) ownerLost = true; roles.kill(pid); process.database.abortInjectedHost(); }
        else { process.database.close(); network.kill(pid); }
        network.settle();
    }
    private static int processNumber(String text) { if (!text.matches("p[0-9]+")) throw new IllegalArgumentException("invalid simulation process"); return Integer.parseInt(text.substring(1)); }
    private void backup(SimulationTrace.Step step, Event event) throws IOException {
        String form = step.arguments().getFirst(); boolean identical = form.equals("page_identical");
        require(identical || form.equals("compacted"), "invalid backup form"); event.put("form", form);
        Path path = Path.of("sim.jed.backup" + ++backups);
        try { database.backupForTesting(path, identical); }
        catch (RuntimeException failure) { require(!fs.exists(path), "failed backup left its destination"); throw failure; }
        byte[] image = fs.image(path.toString()); var copy = new SimulationFileSystem(trace.seed(), trace.pageSize(), trace.sector()); copy.install(path.toString(), image);
        FileOptions options = processOptions.toBuilder().storageHost(copy).roleHost(null).serve(false).readOnly(true).credential(key < 0 ? null : credentials[key]).build();
        try (Database opened = Database.open(path, options); Session session = opened.session()) {
            String value = SimulationRows.digest(session, copy); long copied = opened.processTxid(), source = identical ? copied : copied - 1;
            require(source >= (ownerLost ? floor : version) && source <= attempted && history.getOrDefault(source, Set.of()).contains(value), "backup copied unrecorded contents");
            if (ownerLost) { version = source; committed = value; ownerLost = false; growth.reset(storageDatabase()); }
            successfulBackups.add(new Backup(backups, path.toString(), copied, value, key));
            var facts = SimulationStorageChecks.live(opened);
            event.put("version", copied).put("digest", value).put("live", facts.live()).put("pages", facts.pages()).put("free", facts.free()).put("image", SimulationRows.fold(Arrays.copyOf(image, Math.toIntExact(facts.pages() * trace.pageSize()))));
        }
        String output = System.getenv("JED_SIM_OUT"); if (output != null && !output.isEmpty()) Files.write(Path.of(output, trace.name().replace(".sim", "") + ".java-native.step" + step.index() + ".backup.img"), image);
    }
    private void reopenAll() throws IOException {
        selectedProcess = 0; database = Database.open(Path.of("sim.jed"), processOptions = options());
        if (trace.service()) { processes.put(0, new Process(database, processOptions)); network.settle(); }
        domains.reattach(database, processOptions); resetGrowth();
    }
    private void abandonOpened() throws IOException {
        if (trace.service()) { for (int pid : List.copyOf(processes.keySet())) endProcess(pid, true); }
        else database.abortInjectedHost();
    }
    private void corrupt(SimulationTrace.Step step) throws Exception {
        long pages = inspection().meta().pageCount(); cleanClose(); fs.crash(false, 0, 0); fs.restart();
        boolean reseal = parameter(step.arguments(), "reseal", 0) == 1;
        var changed = fs.corrupt("sim.jed", parameter(step.arguments(), "flips", 0), pages, reseal);
        String opened = "ok", read = "-"; boolean active = false;
        try {
            try { selectedProcess = 0; database = Database.open(Path.of("sim.jed"), processOptions = options()); active = true; if (trace.service()) processes.put(0, new Process(database, processOptions)); }
            catch (SqlException error) { require(reseal || error.sqlState().equals("XX001"), "corrupt open returned " + error.sqlState()); opened = error.sqlState(); }
            if (active) {
                try { String value = digest(); long recovered = database.processTxid(); read = "ok";
                    require(reseal || recovered <= version && recovered >= version - 1 && history.getOrDefault(recovered, Set.of()).contains(value), "corruption produced unrecorded rows"); }
                catch (SqlException error) { require(reseal || error.sqlState().equals("XX001"), "corrupt read returned " + error.sqlState()); read = error.sqlState(); }
            }
        } finally { if (active) abandonOpened(); fs.restart(); fs.repair("sim.jed", changed); }
        reopenAll(); require(database.processTxid() == version && digest().equals(committed), "corruption repair changed database");
        emit(event(step).put("flipped", changed.size()).put("open", opened).put("read", read));
    }
    private void replay(SimulationTrace.Step step) throws Exception {
        String kind = step.arguments().getFirst(); cleanClose(); fs.crash(false, 0, 0); fs.restart();
        if (key < 0) { reopenAll(); emit(event(step).put("kind", kind).put("skip", true)); return; }
        SimulationPointers.Facts pointers;
        try (var opened = FileDatabase.openHost(null, fs, Path.of("sim.jed"), trace.cacheBytes(), credentials[key], Environment.empty(), true, false, null)) { pointers = SimulationPointers.inspect(opened); }
        record Candidate(long page, List<byte[]> versions) { }
        List<Candidate> candidates = new ArrayList<>();
        for (long page : pointers.kinds().getOrDefault(kind, List.of())) { var versions = fs.replayVersions("sim.jed", page); if (!versions.isEmpty()) candidates.add(new Candidate(page, versions)); }
        if (candidates.isEmpty()) { reopenAll(); emit(event(step).put("kind", kind).put("skip", true)); return; }
        var candidate = candidates.get((int)Long.remainderUnsigned(SimulationFileSystem.decide(trace.seed(), 15, step.index(), 0, 0), candidates.size()));
        byte[] older = candidate.versions.get((int)Long.remainderUnsigned(SimulationFileSystem.decide(trace.seed(), 16, step.index(), 0, 0), candidate.versions.size()));
        var replaced = fs.replace("sim.jed", candidate.page, older); String outcome = "XX001"; long recovered = 0; boolean active = false;
        try {
            try {
                selectedProcess = 0; database = Database.open(Path.of("sim.jed"), processOptions = options()); active = true;
                if (trace.service()) processes.put(0, new Process(database, processOptions));
                recovered = database.processTxid(); String value = digest();
                if (recovered == pointers.otherVersion() && recovered < version && pointers.dependencies().contains(candidate.page) && history.getOrDefault(recovered, Set.of()).contains(value)) {
                    SimulationStorageChecks.live(database); outcome = "fallback";
                } else {
                    require(recovered == version && value.equals(committed) && !pointers.dependencies().contains(candidate.page), "replay produced unexpected generation or rows");
                    SimulationStorageChecks.live(database); throw new AssertionError("encrypted pointer replay went undetected: " + kind + " " + candidate.page);
                }
            } catch (SqlException error) { require(error.sqlState().equals("XX001"), "replay returned " + error.sqlState()); }
              catch (org.jed.storage.CorruptStorageException expected) { }
        } finally { if (active) abandonOpened(); fs.restart(); fs.repair("sim.jed", List.of(replaced)); }
        reopenAll(); require(database.processTxid() == version && digest().equals(committed), "replay repair changed database");
        Event event = event(step).put("kind", kind).put("page", candidate.page).put("outcome", outcome); if (outcome.equals("fallback")) event.put("version", recovered); emit(event);
    }
    private void restore(SimulationTrace.Step step) throws Exception {
        cleanClose(); fs.crash(false, 0, 0); fs.restart();
        int back = Integer.parseInt(step.arguments().getFirst()); require(back > 0, "invalid backup ordinal");
        Backup backup = successfulBackups.isEmpty() ? null : successfulBackups.get(Math.max(successfulBackups.size() - back, 0));
        if (backup == null || !fs.exists(Path.of(backup.path))) { reopenAll(); emit(event(step).put("skip", true)); return; }
        fs.restore(backup.path, "sim.jed"); history.keySet().removeIf(value -> value >= backup.version); version = floor = attempted = backup.version;
        committed = backup.digest; key = backup.key; ownerLost = false; remember(version, committed); reopenAll();
        require(database.processTxid() == version && digest().equals(committed), "restored backup changed contents");
        emit(event(step).put("backup", backup.number).put("version", version).put("digest", committed));
    }
    private void domainVersions(Event event) { if (!domains.states.isEmpty()) event.put("dbs", domains.versions()); }
    private boolean unlocks(byte[] image, int candidate) {
        if (image[6] == 0) return candidate < 0;
        if (candidate < 0) return false;
        int page = trace.pageSize();
        try { byte[] secret = KeyBlock.unlock(KeyBlock.select(Arrays.copyOfRange(image, 2 * page, 3 * page), Arrays.copyOfRange(image, 3 * page, 4 * page)), credentials[candidate]); Arrays.fill(secret, (byte)0); return true; }
        catch (SqlException failure) { if (failure.sqlState().equals("28P01")) return false; throw failure; }
    }
    private void attachmentMaintenance(SimulationTrace.Step step, Event event, String name) throws IOException {
        event.put("db", name); var state = domains.states.get(name);
        if (state == null || !step.operation().equals("compact") && !trace.encrypted()) { event.put("skip", true); return; }
        if (step.operation().equals("change_key")) { int target = Integer.parseInt(step.arguments().getFirst()); attachmentKeyAttempts.put(name, new KeyAttempt(state.key, target, !fs.entryDurable(state.path()), false)); database.changeCredential(name, credentials[target]); attachmentKeyAttempts.put(name, new KeyAttempt(state.key, target, !fs.entryDurable(state.path()), true)); state.key = target; event.put("key", target); return; }
        state.pending = state.committed; state.remember(state.version + 1, state.committed); long old = state.version;
        if (step.operation().equals("compact")) database.compact(name);
        else { int target = step.arguments().getFirst().equals("plain") ? -1 : Integer.parseInt(step.arguments().getFirst()); attachmentKeyAttempts.put(name, new KeyAttempt(state.key, target, !fs.entryDurable(state.path()), false)); database.rekey(name, target < 0 ? null : credentials[target]); attachmentKeyAttempts.put(name, new KeyAttempt(state.key, target, !fs.entryDurable(state.path()), true)); state.key = target; }
        state.version = state.database.processTxid(); require(state.version == old + 1, "attachment maintenance generation");
        try (Session session = database.session()) { require(state.committed.equals(SimulationRows.digest(session, fs, name)), "attachment maintenance changed contents"); }
        state.floor = state.version; event.put("version", state.version);
    }
    private Map<String,byte[]> attachmentImages() throws IOException {
        Map<String,byte[]> images = new TreeMap<>();
        for (var state : domains.states.values()) if (fs.exists(Path.of(state.path()))) images.put(state.name, fs.image(state.path()));
        return images;
    }
    private void settleAttachmentKeys(int step, Map<String,byte[]> images) {
        for (var state : domains.states.values()) {
            byte[] image = images.get(state.name); if (image == null) continue;
            int previous = state.key; KeyAttempt change = attachmentKeyAttempts.get(state.name);
            if (change != null) {
                boolean oldWorks = unlocks(image, change.oldKey), newWorks = unlocks(image, change.newKey);
                require(change.oldKey == change.newKey ? oldWorks : oldWorks && !newWorks && !change.acknowledged || !oldWorks && newWorks || change.entryPending && !oldWorks && !newWorks,
                    "attachment credential recovery violates atomic replacement or acknowledged durability");
            }
            boolean found = false;
            for (int candidate : new LinkedHashSet<>(List.of(previous, -1, 0, 1, 2, 3))) if (unlocks(image, candidate)) { state.key = candidate; found = true; break; }
            require(found, "attachment has no authorized recovery credential");
            if (change != null || state.key != previous) emit(new Event(step, "unlocked").put("db", state.name).put("key", state.key));
        }
    }
    private void domainFields(Event event, boolean recovered, boolean summary) throws IOException { domainFields(event, recovered, summary, null); }
    private void domainFields(Event event, boolean recovered, boolean summary, Map<String,byte[]> images) throws IOException {
        if (!domains.states.isEmpty()) event.put("dbs", domains.fields(database, recovered, summary, trace.pageSize(), images));
        String output = System.getenv("JED_SIM_OUT");
        if (!summary && output != null && !output.isEmpty()) for (var state : domains.states.values())
            Files.write(Path.of(output, trace.name().replace(".sim", "") + ".java-native.step" + fs.step + "." + state.name + ".img"), images == null ? fs.image(state.path()) : images.get(state.name));
    }
    private Event event(SimulationTrace.Step step) { return new Event(step.index(), step.operation()); }
    private void emit(Event event) { transcript.add(event.json()); }
    private static void require(boolean valid, String message) { SimulationStorageChecks.require(valid, message); }
    private static final class Event {
        private final LinkedHashMap<String,Object> fields = new LinkedHashMap<>();
        Event(int step, String operation) { put("i", step); put("op", operation); }
        Event put(String name, Object value) { fields.put(name, value); return this; }
        String json() { StringBuilder out = new StringBuilder(); value(out, fields); return out.toString(); }
        private static void value(StringBuilder out, Object value) { if (value instanceof String text) string(out, text); else if (value instanceof Map<?,?> map) { out.append('{'); boolean first = true; for (var entry : map.entrySet()) { if (!first) out.append(','); first = false; string(out, entry.getKey().toString()); out.append(':'); value(out, entry.getValue()); } out.append('}'); } else out.append(value); }
        private static void string(StringBuilder out, String text) { out.append('"'); for (int i = 0; i < text.length(); i++) { char c = text.charAt(i); if (c == '"' || c == '\\') out.append('\\').append(c); else if (c < 32) out.append(String.format(Locale.ROOT, "\\u%04x", (int)c)); else out.append(c); } out.append('"'); }
    }
}
