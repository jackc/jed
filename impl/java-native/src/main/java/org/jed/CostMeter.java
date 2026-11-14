package org.jed;

final class CostMeter {
    private final Session session;
    private final Cancellation cancellation;
    private final long limit;
    private long accrued;
    PlanMetrics writeMetrics;
    PlanNode writeSourceActual;
    long writeAffected;
    void writePhase(String phase, long before) { if (writeMetrics != null) writeMetrics.add(phase, before, this); }
    private int ceilingHit;
    private int unmetered;
    private boolean immutableInput;
    boolean immutableInput() { return immutableInput; }
    <T> T immutableInput(java.util.function.Supplier<T> action) {
        boolean saved = immutableInput; immutableInput = true;
        try { return unmetered(action); } finally { immutableInput = saved; }
    }
    private java.util.Map<String, Engine.CollationReference> collations = java.util.Map.of();
    void collations(java.util.Map<String, Engine.CollationReference> references) { collations = references; }
    java.util.Map<String, Engine.CollationReference> collations() { return collations; }
    Environment environment() { return environment; }
    <T> T unmetered(java.util.function.Supplier<T> operation) {
        unmetered++; try { return operation.get(); } finally { unmetered--; }
    }
    private final MemoryAccount memory;
    private final long scalarLimit;
    private long scalarUsed;
    private final java.util.Map<String, String> variables;
    String variable(String name) { return variables.get(SessionVariables.name(name)); }
    private final TimeZoneData.Zone zone;
    private final Environment environment;
    private Long statementTime;
    private int uuidCounter;
    private final java.util.IdentityHashMap<Object, Boolean> regexCompiles = new java.util.IdentityHashMap<>();
    void regexCompile(Object node, RegexProgram program) { if (regexCompiles.put(node, Boolean.TRUE) == null) program.chargeCompile(this); }

    CostMeter(Session session, Cancellation cancellation) {
        this.session = session;
        this.cancellation = cancellation;
        this.limit = session.maxCost();
        this.scalarLimit = session.maxScalarBytes();
        this.memory = new MemoryAccount(session.maxQueryMemoryBytes());
        this.memory.onRejected(this::guard);
        this.memory.encrypted(session.encrypted());
        this.memory.scratchHost(session.scratchHost());
        if (session.encrypted()) this.memory.scratchEntropy(session::scratchEntropy);
        this.variables = session.variableSnapshot();
        this.zone = session.zone();
        this.environment = session.environment();
    }
    record Checkpoint(long cost, long lifetime, int ceiling, long scalar, long memory) {}
    Checkpoint checkpoint() { return new Checkpoint(accrued, session.lifetimeCost(), ceilingHit, scalarUsed, memory.used()); }
    void restore(Checkpoint saved) {
        if (memory.used() != saved.memory()) throw new IllegalStateException("discarded lane retained query memory");
        accrued = saved.cost(); session.restoreCost(saved.lifetime()); ceilingHit = saved.ceiling(); scalarUsed = saved.scalar();
    }
    CostMeter fork() { CostMeter child = new CostMeter(session, cancellation); child.collations(collations); child.pendingBytes(pendingBytes); return child; }
    TimeZoneData.Zone zone() { return zone; }
    TimeZoneData.Zone zone(String name) { return environment.zone(name); }
    TimeZoneData.Zone constructorZone(String name) { return environment.zone(name, true); }
    long statementTime() { if (statementTime == null) statementTime = environment.clock(); return statementTime; }
    long clockTime() { return environment.clock(); }
    void random(byte[] bytes) { environment.random(bytes); }
    int nextUuidCounter() { return uuidCounter++ & 4095; }
    long cost() { return accrued; }
    long speculationRows(long unit) {
        if (unit == 0) return Long.MAX_VALUE;
        long headroom = Math.min(limit > 0 ? Math.max(0, limit - accrued) : Long.MAX_VALUE, session.lifetimeMaxCost() > 0 ? Math.max(0, session.lifetimeMaxCost() - session.lifetimeCost()) : Long.MAX_VALUE);
        return headroom == Long.MAX_VALUE ? Long.MAX_VALUE : headroom / unit + 1;
    }
    private long pendingBytes;
    void pendingBytes(long value) {
        guard();
        if (value > pendingBytes) memory.reserve(value - pendingBytes);
        else if (value < pendingBytes) memory.release(pendingBytes - value);
        pendingBytes = value;
    }
    MemoryAccount memory() { return memory; }
    void charge(long units) {
        if (units < 0) throw new IllegalArgumentException("negative cost");
        if (unmetered != 0) return;
        if (ceilingHit == 0) {
            long statementRemaining = limit > 0 ? limit - accrued : Long.MAX_VALUE;
            long lifetimeRemaining = session.lifetimeMaxCost() > 0 ? session.lifetimeMaxCost() - session.lifetimeCost() : Long.MAX_VALUE;
            boolean statement = limit > 0 && units >= statementRemaining;
            boolean lifetime = session.lifetimeMaxCost() > 0 && units >= lifetimeRemaining;
            if (statement) ceilingHit = 1;
            if (lifetime && (!statement || lifetimeRemaining < statementRemaining)) ceilingHit = 2;
        }
        accrued = saturatingAdd(accrued, units);
        session.addCost(units);
    }
    void cancellationGuard() { if (cancellation != null && cancellation.isCancelled()) throw new SqlException("57014", "query cancelled"); }
    void guard() {
        cancellationGuard();
        if (ceilingHit == 1 || limit > 0 && accrued >= limit) throw new SqlException("54P01", "statement cost limit reached");
        if (ceilingHit == 2 || session.lifetimeMaxCost() > 0 && session.lifetimeCost() >= session.lifetimeMaxCost()) {
            throw new SqlException("54P02", "session lifetime cost limit reached");
        }
    }
    void operation() { charge(Costs.OPERATOR_EVAL); guard(); }
    void scalarInput(long bytes) { charge(saturatingMultiply(bytes, Costs.SCALAR_BYTE)); guard(); }
    void scalarOutput(long bytes) {
        charge(saturatingMultiply(bytes, Costs.SCALAR_BYTE)); guard();
        scalarReserve(bytes);
    }
    void scalarReserve(long bytes) {
        if (bytes < 0) throw new IllegalArgumentException("negative scalar reservation");
        if (bytes > scalarLimit - scalarUsed) throw new SqlException("54P04", "scalar memory limit reached");
        scalarUsed += bytes;
    }
    static long saturatingAdd(long a, long b) { return a > Long.MAX_VALUE - b ? Long.MAX_VALUE : a + b; }
    static long saturatingMultiply(long a, long b) { return b != 0 && a > Long.MAX_VALUE / b ? Long.MAX_VALUE : a * b; }
}
