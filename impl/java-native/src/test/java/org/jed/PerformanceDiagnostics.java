package org.jed;

import java.lang.management.ManagementFactory;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;

/** Matched public-API diagnostics. Timing includes draining and canonical result rendering. */
public final class PerformanceDiagnostics {
    @jdk.jfr.Name("org.jed.BenchmarkPhase")
    @jdk.jfr.Label("Jed benchmark phase")
    private static final class BenchmarkPhase extends jdk.jfr.Event {
        public String workload;
        public String phase;
        BenchmarkPhase(String workload, String phase) { this.workload = workload; this.phase = phase; }
    }
    private static final class Checksum {
        long value = 0xcbf29ce484222325L, cost, workerAllocated;
        void text(String text) { for (byte b : text.getBytes(StandardCharsets.UTF_8)) value = (value ^ (b & 255)) * 0x100000001b3L; }
    }
    private static final FileOptions OPTIONS = FileOptions.builder().cacheBytes(256L*1024*1024).build();
    private static long parallel(Database database, String sql, Checksum checksum) throws InterruptedException {
        Checksum[] sums = new Checksum[4]; long[] counts = new long[4]; Throwable[] failures = new Throwable[4];
        Thread[] workers = new Thread[4];
        var bean = (com.sun.management.ThreadMXBean)ManagementFactory.getThreadMXBean();
        for (int i=0;i<workers.length;i++) {
            final int index=i;
            workers[i]=Thread.ofPlatform().unstarted(() -> {
                long id=Thread.currentThread().threadId(), before=bean.getThreadAllocatedBytes(id);
                Checksum sum=new Checksum(); sums[index]=sum;
                try (Session session=database.session()) {
                    PreparedStatement statement=session.prepare(sql);
                    for (int n=0;n<64;n++) counts[index]+=drain(session,session.queryPrepared(statement),sum);
                } catch (Throwable failure) { failures[index]=failure; }
                sum.workerAllocated=bean.getThreadAllocatedBytes(id)-before;
            });
        }
        for (Thread worker:workers) worker.start();
        for (Thread worker:workers) worker.join();
        long count=0;
        for (int i=0;i<workers.length;i++) {
            if (failures[i]!=null) throw new IllegalStateException("parallel read failed",failures[i]);
            checksum.text(String.format("%016x",sums[i].value)); checksum.text("\u001d");
            checksum.cost=Math.addExact(checksum.cost,sums[i].cost); checksum.workerAllocated+=sums[i].workerAllocated; count+=counts[i];
        }
        return count;
    }
    private static long retainedHeap() { System.gc(); return ManagementFactory.getMemoryMXBean().getHeapMemoryUsage().getUsed(); }
    private record Workload(String name, String mode, int warmup, int iterations, String sql) {}
    private static long drain(Session session, Rows rows, Checksum checksum) {
        try (rows) {
            long count = 0; Row row;
            while ((row = rows.next()) != null) {
                for (SqlValue value : row.values()) { checksum.text(session.render(value)); checksum.text("\u001f"); }
                checksum.text("\u001e"); count++;
            }
            checksum.cost = Math.addExact(checksum.cost, rows.cost()); return count;
        }
    }
    private static long gcCount() { return ManagementFactory.getGarbageCollectorMXBeans().stream().mapToLong(bean -> Math.max(0, bean.getCollectionCount())).sum(); }
    private static long gcMillis() { return ManagementFactory.getGarbageCollectorMXBeans().stream().mapToLong(bean -> Math.max(0, bean.getCollectionTime())).sum(); }
    private static JsonNode number(long value) { return JsonNode.number(Decimal.parse(Long.toString(value))); }
    private static JsonNode array(long[] values) { List<JsonNode> nodes = new ArrayList<>(); for (long value : values) nodes.add(number(value)); return JsonNode.array(nodes); }
    public static void main(String[] arguments) throws Exception {
        if (arguments.length == 1 && arguments[0].equals("ready")) {
            try (Database database = Database.memory(); Session session = database.session()) { drain(session, session.query("SELECT 1"), new Checksum()); }
            System.out.println("ready"); return;
        }
        boolean retained = arguments.length == 3 && arguments[2].equals("retained");
        if (arguments.length != 2 && !retained) throw new IllegalArgumentException("performance INPUT DIRECTORY | ready");
        Path directory = Path.of(arguments[1]); Files.createDirectories(directory);
        List<String> setup = new ArrayList<>(); List<Workload> workloads = new ArrayList<>();
        for (String line : Files.readAllLines(Path.of(arguments[0]), StandardCharsets.UTF_8)) {
            String[] fields = line.split("\t", 6);
            if (fields[0].equals("setup") && fields.length == 2) setup.add(fields[1]);
            else if (fields[0].equals("case") && fields.length == 6) {
                if (!fields[1].matches("[a-z0-9_]+") || !List.of("query", "rollback", "durable", "cold_open", "parallel").contains(fields[2])) throw new IllegalArgumentException("invalid workload");
                int warmup = Integer.parseInt(fields[3]), iterations = Integer.parseInt(fields[4]);
                if (warmup < 0 || iterations <= 0) throw new IllegalArgumentException("invalid sample count");
                workloads.add(new Workload(fields[1], fields[2], warmup, iterations, fields[5]));
            } else throw new IllegalArgumentException("invalid benchmark input");
        }
        var thread = (com.sun.management.ThreadMXBean)ManagementFactory.getThreadMXBean();
        if (!thread.isThreadAllocatedMemorySupported()) throw new IllegalStateException("thread allocation measurement unavailable");
        thread.setThreadAllocatedMemoryEnabled(true); long threadId = Thread.currentThread().threadId();
        for (Workload workload : workloads) {
            BenchmarkPhase setupPhase = new BenchmarkPhase(workload.name(), "setup"); setupPhase.begin();
            Path path = directory.resolve(workload.name() + ".jed");
            try (Database database = Database.create(path, OPTIONS); Session session = database.session()) {
                for (String sql : setup) drain(session, session.query(sql), new Checksum());
            }
            Database database = workload.mode().equals("cold_open") ? null : Database.open(path, OPTIONS);
            Session session = database == null ? null : database.session();
            PreparedStatement prepared = session == null || workload.mode().equals("parallel") ? null : session.prepare(workload.sql());
            setupPhase.commit();
            long retainedBefore = retained ? retainedHeap() : -1;
            BenchmarkPhase warmupPhase = new BenchmarkPhase(workload.name(), "warmup");
            BenchmarkPhase measuredPhase = new BenchmarkPhase(workload.name(), "measured");
            warmupPhase.begin();
            Checksum checksum = new Checksum(); long count = 0;
            long[] samples = new long[workload.iterations()], warmups = new long[workload.warmup()];
            long allocatedBefore = 0, gcBefore = 0, gcTimeBefore = 0, heapBefore = 0, compilationBefore = 0;
            try {
                for (int i = 0; i < workload.warmup() + workload.iterations(); i++) {
                    if (i == workload.warmup()) {
                        warmupPhase.commit(); measuredPhase.begin();
                        allocatedBefore = thread.getThreadAllocatedBytes(threadId); gcBefore = gcCount(); gcTimeBefore = gcMillis();
                        heapBefore = ManagementFactory.getMemoryMXBean().getHeapMemoryUsage().getUsed();
                        compilationBefore = ManagementFactory.getCompilationMXBean().getTotalCompilationTime();
                    }
                    Checksum sink = i < workload.warmup() ? new Checksum() : checksum;
                    long start = System.nanoTime(), rows;
                    if (workload.mode().equals("cold_open")) {
                        try (Database opened = Database.open(path, OPTIONS); Session reader = opened.session()) {
                            rows = drain(reader, reader.queryPrepared(reader.prepare(workload.sql())), sink);
                        }
                    } else if (workload.mode().equals("parallel")) {
                        rows = parallel(database, workload.sql(), sink);
                    } else if (workload.mode().equals("rollback")) {
                        drain(session, session.query("BEGIN"), new Checksum());
                        rows = drain(session, session.queryPrepared(prepared), sink);
                        drain(session, session.query("ROLLBACK"), new Checksum());
                    } else rows = drain(session, session.queryPrepared(prepared), sink);
                    long elapsed = System.nanoTime() - start;
                    if (i < workload.warmup()) warmups[i] = elapsed;
                    else { samples[i - workload.warmup()] = elapsed; count += rows; }
                }
                measuredPhase.end();
                long allocated = thread.getThreadAllocatedBytes(threadId) - allocatedBefore;
                measuredPhase.commit();
                long collections = gcCount() - gcBefore, gcMillis = gcMillis() - gcTimeBefore;
                long heapAfter = ManagementFactory.getMemoryMXBean().getHeapMemoryUsage().getUsed();
                if (workload.mode().equals("rollback") || workload.mode().equals("durable")) count += drain(session, session.query("SELECT count(*), sum(v) FROM t"), checksum);
                long retainedLive=retained ? retainedHeap() : -1, retainedClosed=-1;
                if (retained) { if (session!=null) session.close(); if (database!=null) database.close(); prepared=null; session=null; database=null; retainedClosed=retainedHeap(); }
                var record = List.of(
                    new JsonNode.Member("name", JsonNode.string(workload.name())), new JsonNode.Member("mode", JsonNode.string(workload.mode())),
                    new JsonNode.Member("runtime", JsonNode.string(Runtime.version().toString())), new JsonNode.Member("warmup", number(workload.warmup())),
                    new JsonNode.Member("iterations", number(workload.iterations())), new JsonNode.Member("rows", number(count)),
                    new JsonNode.Member("checksum", JsonNode.string(String.format("%016x", checksum.value))), new JsonNode.Member("cost", number(checksum.cost)),
                    new JsonNode.Member("samples_ns", array(samples)), new JsonNode.Member("warmup_ns", array(warmups)),
                    new JsonNode.Member("thread_allocated_bytes", number(allocated)), new JsonNode.Member("worker_allocated_bytes",number(checksum.workerAllocated)),
                    new JsonNode.Member("retained_before",number(retainedBefore)), new JsonNode.Member("retained_live_database",number(retainedLive)), new JsonNode.Member("retained_after_close",number(retainedClosed)), new JsonNode.Member("gc_collections", number(collections)),
                    new JsonNode.Member("gc_millis", number(gcMillis)), new JsonNode.Member("compilation_millis",number(ManagementFactory.getCompilationMXBean().getTotalCompilationTime()-compilationBefore)), new JsonNode.Member("heap_before", number(heapBefore)), new JsonNode.Member("heap_after", number(heapAfter)));
                System.out.println(JsonNode.object(record).render());
            } finally { if (session != null) session.close(); if (database != null) database.close(); }
        }
    }
}
