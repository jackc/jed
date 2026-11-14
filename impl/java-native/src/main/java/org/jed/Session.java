package org.jed;

/** Serial session operations may move between threads; overlapping operations are rejected. */
public final class Session implements AutoCloseable {
    private Database tempDatabase;
    private final Engine.Snapshot emptyTemp = Engine.Snapshot.empty();
    private volatile long maxTempStorageBytes = 1_073_741_824L;
    private volatile long tempBuffers = 33_554_432L;
    public long maxTempStorageBytes() { return maxTempStorageBytes; }
    public long tempBuffers() { return tempBuffers; }
    public void setMaxTempStorageBytes(long value) { enter(); try { maxTempStorageBytes = Math.max(0, value); } finally { releaseSetup(); } }
    public void setTempBuffers(long value) { enter(); try { tempBuffers = Math.max(0, value); } finally { releaseSetup(); } }
    Database tempDatabase() { if (tempDatabase == null) tempDatabase = database.newTempDatabase(emptyTemp); return tempDatabase; }
    Engine.Snapshot tempSnapshot() { return tempDatabase == null ? emptyTemp : tempDatabase.snapshot(); }
    void checkTempStorage() { if (tempDatabase != null && maxTempStorageBytes > 0 && tempDatabase.storageBytes() > maxTempStorageBytes) throw new SqlException("54P03", "temporary storage exceeded the limit of " + maxTempStorageBytes + " bytes"); }
    private final Privileges privileges = new Privileges();
    public java.util.Set<Privilege> defaultPrivileges() { return privileges.defaults(); }
    public void setDefaultPrivileges(java.util.Set<Privilege> value) { enter(); try { privileges.defaults(value); } finally { releaseSetup(); } }
    public void grant(String object, Privilege... operations) { enter(); try { privileges.change(object, false, operations); } finally { releaseSetup(); } }
    public void revoke(String object, Privilege... operations) { enter(); try { privileges.change(object, true, operations); } finally { releaseSetup(); } }
    public void clearPrivilegeOverrides() { enter(); try { privileges.clear(); } finally { releaseSetup(); } }
    public void setAllowDdl(boolean value) { enter(); try { privileges.allowDdl = value; } finally { releaseSetup(); } }
    public void setAllowTempDdl(Boolean value) { enter(); try { privileges.allowTempDdl = value; } finally { releaseSetup(); } }
    Privileges privileges() { return privileges; }
    private final java.util.Map<String, String> variables = new java.util.HashMap<>();
    public void setVar(String name, String value) {
        enter(); try { String key = SessionVariables.name(name); if (!key.contains(".")) throw new SqlException("42704", "unrecognized configuration parameter: " + name); SqlText.validate(value); variables.put(key, value); } finally { releaseSetup(); }
    }
    public void resetVar(String name) {
        enter(); try { String key = SessionVariables.name(name); if (!key.contains(".")) throw new SqlException("42704", "unrecognized configuration parameter: " + name); variables.remove(key); } finally { releaseSetup(); }
    }
    public String var(String name) { enter(); try { return variables.get(SessionVariables.name(name)); } finally { releaseSetup(); } }
    public void clearVars() { enter(); try { variables.clear(); } finally { releaseSetup(); } }
    java.util.Map<String, String> variableSnapshot() { return java.util.Map.copyOf(variables); }
    private final Database database;
    private final Engine engine;
    private final boolean readHandle;
    boolean readHandle() { return readHandle; }
    boolean hasWriteTransaction() { return engine.hasWriteTransaction(); }
    private final Object gate = new Object();
    private boolean busy;
    private volatile boolean closed;
    private Rows rows;
    public static final long DEFAULT_MAX_SQL_LENGTH = 1_048_576;
    private volatile long maxSqlLength = DEFAULT_MAX_SQL_LENGTH;
    public long maxSqlLength() { return maxSqlLength; }
    public void setMaxSqlLength(long value) {
        enter(); try { if (value < 0) throw new SqlException("22023", "negative SQL length limit"); maxSqlLength = value; }
        finally { releaseSetup(); }
    }
    private PreparedStatement parse(String sql) {
        if (maxSqlLength > 0 && SqlText.encodedLength(sql) > maxSqlLength) throw new SqlException("54000", "SQL text exceeds input length limit");
        return new PreparedStatement(sql);
    }
    private volatile long maxCost;
    private volatile long lifetimeMaxCost;
    private volatile long lifetimeCost;
    private volatile long maxQueryMemoryBytes;
    private volatile long workMem = 268_435_456;
    private volatile long lockTimeoutMillis;
    public long lockTimeoutMillis() { return lockTimeoutMillis; }
    public void setLockTimeoutMillis(long value) {
        enter(); try { if (value < 0) throw new SqlException("22023", "negative lock timeout"); lockTimeoutMillis = value; }
        finally { releaseSetup(); }
    }
    public long maxQueryMemoryBytes() { return maxQueryMemoryBytes; }
    public long workMem() { return workMem; }
    public void setMaxQueryMemoryBytes(long value) {
        enter(); try { if (value < 0) throw new SqlException("22023", "negative memory limit"); maxQueryMemoryBytes = value; }
        finally { releaseSetup(); }
    }
    public void setWorkMem(long value) {
        enter(); try { if (value < 0) throw new SqlException("22023", "negative work memory"); workMem = value; }
        finally { releaseSetup(); }
    }
    private volatile long maxScalarBytes = LimitsSchedule.DEFAULT_SCALAR_BYTES;
    private volatile TimeZoneData.Zone timeZone = TimeZoneData.Zone.fixed("UTC", 0);
    public String timeZone() { return timeZone.name(); }
    public String render(SqlValue value) { return Temporal.renderValue(value, timeZone); }
    public void setTimeZone(String name) {
        enter(); try { timeZone = database.environment().zone(name); }
        finally { releaseSetup(); }
    }
    TimeZoneData.Zone zone() { return timeZone; }
    ScratchHost scratchHost() { return database.scratchHost(); }
    void scratchEntropy(byte[] bytes) { database.fillScratchEntropy(bytes); }
    boolean encrypted() { return database.protectedScratch(); }
    Environment environment() { return database.environment(); }

    Session(Database database) { this(database, false); }
    Session(Database database, boolean readHandle) { this.database = database; this.readHandle = readHandle; this.engine = new Engine(database, this); if (readHandle) engine.openReadHandle(); }
    public long maxCost() { return maxCost; }
    public long lifetimeMaxCost() { return lifetimeMaxCost; }
    public long lifetimeCost() { return lifetimeCost; }
    public long maxScalarBytes() { return maxScalarBytes; }
    public void setMaxScalarBytes(long value) {
        enter();
        try { maxScalarBytes = value > 0 ? value : LimitsSchedule.DEFAULT_SCALAR_BYTES; }
        finally { releaseSetup(); }
    }
    void restoreCost(long value) { lifetimeCost = value; }
    void addCost(long units) { lifetimeCost = CostMeter.saturatingAdd(lifetimeCost, units); }
    public void setMaxCost(long value) {
        enter();
        try { if (value < 0) throw new SqlException("22023", "negative cost limit"); maxCost = value; }
        finally { releaseSetup(); }
    }
    public void setLifetimeMaxCost(long value) {
        enter();
        try { if (value < 0) throw new SqlException("22023", "negative lifetime limit"); lifetimeMaxCost = value; }
        finally { releaseSetup(); }
    }
    public String defaultCollation() { enter(); try { return engine.defaultCollation(); } finally { releaseSetup(); } }
    public void setDefaultCollation(String name) { changeCollations(java.util.Objects.requireNonNull(name)); }
    public int upgradeCollations() { return changeCollations(null); }
    private int changeCollations(String name) {
        enter(); CostMeter meter = new CostMeter(this, null);
        try { meter.guard(); return engine.changeCollations(name, meter); }
        catch (SqlException error) { engine.fail(error); throw error.withCost(meter.cost()); }
        finally { releaseSetup(); }
    }
    public PreparedStatement prepare(String sql) {
        enter(); CostMeter meter = new CostMeter(this, null);
        try { meter.guard(); return parse(sql); }
        catch (SqlException error) { engine.fail(error); throw error.withCost(meter.cost()); }
        finally { releaseSetup(); }
    }
    public Rows query(String sql, SqlValue... arguments) { return query(sql, null, arguments); }
    public Rows query(String sql, Cancellation cancellation, SqlValue... arguments) { return run(() -> parse(sql), cancellation, arguments); }
    public Rows queryPrepared(PreparedStatement statement, SqlValue... arguments) { return queryPrepared(statement, null, arguments); }
    public Rows queryPrepared(PreparedStatement statement, Cancellation cancellation, SqlValue... arguments) { return run(() -> java.util.Objects.requireNonNull(statement), cancellation, arguments); }
    public long execute(String sql, SqlValue... arguments) { try (Rows rows = query(sql, arguments)) { while (rows.next() != null) { } return rows.affectedRows(); } }
    public long executePrepared(PreparedStatement statement, SqlValue... arguments) { try (Rows rows = queryPrepared(statement, arguments)) { while (rows.next() != null) { } return rows.affectedRows(); } }
    private Rows run(java.util.function.Supplier<PreparedStatement> statement, Cancellation cancellation, SqlValue[] arguments) {
        enter();
        CostMeter meter = new CostMeter(this, cancellation);
        try {
            meter.guard();
            PreparedStatement prepared = statement.get();
            SqlParser.Statement syntax = prepared.syntax(arguments);
            QueryPlan plan = engine.prepare(syntax, arguments.clone(), meter, prepared.parameterCount());
            Rows result = new Rows(this, plan, meter, syntax instanceof SqlParser.Explain);
            try { publish(result); }
            catch (RuntimeException | Error failure) { result.close(); throw failure; }
            return result;
        } catch (SqlException error) { engine.fail(error); releaseSetup(); throw error.withCost(meter.cost()); }
        catch (RuntimeException | Error error) { releaseSetup(); throw error; }
    }
    void enter() {
        synchronized (gate) {
            if (closed) throw new SqlException("55000", "session is closed");
            if (busy) throw new SqlException("55000", "session has an active operation");
            busy = true;
        }
    }
    void publish(Rows result) {
        synchronized (gate) {
            // Close may race parsing/resolution. Never publish after ownership ends.
            if (closed) throw new SqlException("55000", "session is closed");
            if (!busy || rows != null) throw new IllegalStateException("invalid cursor publication");
            rows = result;
        }
    }
    private void releaseSetup() {
        synchronized (gate) {
            if (rows == null) { busy = false; if (closed) finishClose(); }
        }
    }
    void release(Rows owner) {
        synchronized (gate) {
            // Delayed cleanup from an old cursor cannot unlock its replacement.
            if (rows != owner) return;
            rows = null;
            busy = false;
            if (closed) finishClose();
        }
    }
    boolean closed() { return closed; }
    void failed(SqlException error) { engine.fail(error); }
    @Override public void close() {
        Rows active;
        synchronized (gate) {
            if (closed) return;
            closed = true;
            active = rows;
            if (!busy) finishClose();
        }
        if (active != null) active.close();
    }
    private void finishClose() { try { engine.close(); } finally { try { if (tempDatabase != null) tempDatabase.close(); } finally { database.release(this); } } }
}
