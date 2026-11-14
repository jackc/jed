package org.jed;

import java.util.HashSet;
import java.util.Set;

/** Shared ownership boundary. Table and file APIs are being added through the pager seam. */
public final class Database implements AutoCloseable {
    private final Set<Session> sessions = new HashSet<>();
    private boolean closed;
    private long maxStorageBytes;
    private record Attachment(Database database, boolean readOnly, String realPath) {}
    private final java.util.TreeMap<String, Attachment> attachments = new java.util.TreeMap<>();
    private int snapshotPins;
    private final Object publicationLock = new Object();
    private Database scratchOwner;
    private Engine.Snapshot committed = Engine.Snapshot.empty();
    private Object writer;
    private Session transactionWriter;
    private java.util.List<Database> writerDomains = java.util.List.of();
    private PageService pageService;
    private FileOptions fileOptions = FileOptions.defaults();
    private PageTransport pageTransport;
    private String transportIdentity;
    private boolean flusherStarted;
    private synchronized void configureFile() {
        file.configure(fileOptions);
        if (file.client() || flusherStarted || !fileOptions.idleFlush || fileOptions.flushIntervalMillis() == 0) return;
        flusherStarted = true;
        Thread.ofVirtual().name("jed-idle-flush").start(() -> {
            while (true) {
                try { Thread.sleep(Math.min(1000, Math.max(1, fileOptions.flushIntervalMillis()))); }
                catch (InterruptedException interrupted) { Thread.currentThread().interrupt(); return; }
                synchronized (Database.this) {
                    if (closed) return;
                    if (writer != null || transactionWriter != null || maintenance != 0 || ownerDraining || !file.connected() || file.client()) continue;
                    try { file.flushDue(); } catch (java.io.IOException failed) { roleFailure = Engine.storageError(failed); return; }
                }
            }
        });
    }
    public synchronized void flush() {
        if (closed) throw new SqlException("55000", "database is closed");
        if (writer != null || transactionWriter != null) throw new SqlException("55006", "writer is active");
        if (file != null) try { file.flush(); } catch (java.io.IOException failure) { throw pageError(failure); }
    }
    FileOptions fileOptions() { return fileOptions; }
    private String writerIdentity() { return injectedRolePath != null ? injectedRolePath.toString() : pageTransport == null ? file.path().toString() : "remote:" + transportIdentity; }
    private volatile FileDatabase file;
    private final MemoryDomain memory;
    private org.jed.host.ExclusiveFileLock.Claim disconnectedClaim;
    private SqlException roleFailure;
    private boolean ownerDraining;
    private long promotionPausedUntil;
    private final Environment environment;
    private boolean fileClosed;
    private int maintenance;
    private java.nio.file.Path injectedRolePath;
    private java.util.function.Consumer<byte[]> scratchEntropy;
    synchronized void fillScratchEntropy(byte[] bytes) {
        if (scratchEntropy == null) scratchEntropy = environment.newKeyEntropyStream();
        scratchEntropy.accept(bytes);
    }
    private Database(Environment environment) { this(environment, 8192, 0); }
    private Database(Environment environment, int pageSize, long limit) { this(environment, pageSize, limit, true); }
    private Database(Environment environment, int pageSize, long limit, boolean mainDomain) {
        file = null; this.environment = java.util.Objects.requireNonNull(environment); maxStorageBytes = Math.max(0, limit);
        try { memory = new MemoryDomain(pageSize, mainDomain); } catch (java.io.IOException failure) { throw Engine.storageError(failure); }
    }
    private Database(FileDatabase file, Environment environment) { memory = null; this.file = file; this.environment = java.util.Objects.requireNonNull(environment); committed = file.snapshot();
        try { CollationCompatibility.validate(committed, environment); }
        catch (RuntimeException | Error failure) { try { file.close(); } catch (java.io.IOException close) { failure.addSuppressed(close); } throw failure; }
    }
    public synchronized long maxStorageBytes() { return maxStorageBytes; }
    public synchronized void setMaxStorageBytes(long value) { if (closed) throw new SqlException("55000", "database is closed"); maxStorageBytes = Math.max(0, value); }
    public synchronized void setMaxStorageBytes(String name, long value) { namedDatabase(name).setMaxStorageBytes(value); }
    public synchronized long storageBytes() { snapshot(); return file == null ? memory.storageBytes() : file.serviceMeta().livePages() * file.pageSize(); }
    public synchronized long storageBytes(String name) { return namedDatabase(name).storageBytes(); }
    public synchronized long maxStorageBytes(String name) { return namedDatabase(name).maxStorageBytes(); }
    private Database namedDatabase(String name) {
        if (name.equals("main")) return this;
        Attachment attachment = attachments.get(name);
        if (attachment == null) throw new SqlException("42704", "database is not attached: " + name);
        return attachment.database();
    }
    public void attach(String name, AttachSource source, boolean readOnly) {
        java.util.Objects.requireNonNull(name); java.util.Objects.requireNonNull(source);
        if (name.isEmpty()) throw new SqlException("42710", "attachment name must not be empty");
        Database opened = source.path() == null ? new Database(environment, pageSize(), source.maxStorageBytes(), false)
            : Database.open(source.path(), source.options().toBuilder().environment(environment).readOnly(readOnly)
                .maxStorageBytes(source.maxStorageBytes()).build());
        boolean adopted = false;
        try {
            synchronized (this) {
                if (closed) throw new SqlException("55000", "database is closed");
                if (writer != null || transactionWriter != null) throw new SqlException("55006", "cannot attach while a write transaction is open");
                if (name.equals("main") || name.equals("temp") || attachments.containsKey(name)) throw new SqlException("42710", "database already exists: " + name);
                if (opened.encrypted() && !protectedScratch()) throw new SqlException("55000", "an encrypted attachment requires encrypted scratch files");
                String path = source.path() == null ? null : source.options().storageHost != null ? source.path().normalize().toString() : source.path().toRealPath().toString();
                opened.scratchOwner = this;
                attachments.put(name, new Attachment(opened, readOnly, path)); adopted = true;
            }
        } catch (java.io.IOException failure) { throw Engine.storageError(failure); }
        finally { if (!adopted) opened.close(); }
    }
    public synchronized void detach(String name) {
        if (closed) throw new SqlException("55000", "database is closed");
        if (writer != null || transactionWriter != null) throw new SqlException("55006", "cannot detach while a write transaction is open");
        Attachment attachment = attachments.get(name);
        if (attachment == null) throw new SqlException("42704", "database is not attached: " + name);
        if (snapshotPins != 0) throw new SqlException("55006", "database is in use: " + name);
        attachments.remove(name); attachment.database().close();
    }
    synchronized boolean domainReadOnly(String name) {
        if (name.equals("temp")) return false;
        if (name.equals("main")) return fileOptions.readOnly();
        Attachment attachment = attachments.get(name);
        if (attachment == null) throw new SqlException("42P01", "database is not attached: " + name);
        return attachment.readOnly();
    }
    synchronized void requireWritable(String name) { if (domainReadOnly(name)) throw new SqlException("25006", "database is read-only: " + name); }
    synchronized Database domainDatabase(Session session, String name) { return name.equals("temp") ? session.tempDatabase() : namedDatabase(name); }
    Database newTempDatabase(Engine.Snapshot initial) { Database created = new Database(environment, pageSize(), 0, false); created.scratchOwner = this; created.committed = initial; return created; }
    boolean tempSpilled() { return memory != null && memory.spilled(); }
    long residentTempPages() { return memory == null ? 0 : memory.residentPages(); }
    synchronized int pageSize(String name) { return name.equals("temp") ? pageSize() : namedDatabase(name).pageSize(); }
    synchronized org.jed.storage.PageLayout pageLayout(String name) { return name.equals("temp") ? new org.jed.storage.PageLayout(pageSize(), false) : namedDatabase(name).pageLayout(); }
    synchronized org.jed.storage.PageCodec pageCodec(String name) { return name.equals("temp") ? org.jed.storage.PageCodec.PLAINTEXT : namedDatabase(name).pageCodec(); }
    public static Database memory() { return memory(Environment.empty()); }
    public static Database memory(Environment environment) { return new Database(environment); }
    public static Database memory(long maxStorageBytes) { return new Database(Environment.empty(), 8192, maxStorageBytes); }
    public static Database memory(Environment environment, int pageSize, long maxStorageBytes) { return new Database(environment, pageSize, maxStorageBytes); }
    Environment environment() { return environment; }
    public record CollationInfo(String name, String unicodeVersion, String cldrVersion, String description, boolean isDefault, boolean skewed) {}
    public java.util.List<String> loadedCollations() { return environment.loadedCollations(); }
    public synchronized String defaultCollation() { return CollationCompatibility.defaultName(snapshot()); }
    public void setDefaultCollation(String name) { try (Session session = session()) { session.setDefaultCollation(name); } }
    public int upgradeCollations() { try (Session session = session()) { return session.upgradeCollations(); } }
    public synchronized java.util.List<CollationInfo> collations() {
        Engine.Snapshot state = snapshot();
        return state.collations().values().stream().map(reference -> new CollationInfo(reference.name(), reference.unicodeVersion(), reference.cldrVersion(), reference.description(), reference.isDefault(), !CollationCompatibility.matches(reference.name(), state.collations(), environment))).toList();
    }
    public java.util.List<String> loadedTimeZones() { return environment.loadedTimeZones(); }
    public static Database createExclusive(java.nio.file.Path path, int pageSize, long cacheBytes) {
        return createExclusive(path, pageSize, cacheBytes, Environment.empty());
    }
    public static Database createExclusive(java.nio.file.Path path, int pageSize, long cacheBytes, Environment environment) {
        return createExclusive(path, pageSize, cacheBytes, null, environment);
    }
    public static Database createExclusive(java.nio.file.Path path, int pageSize, long cacheBytes, Credential credential) {
        return createExclusive(path, pageSize, cacheBytes, credential, Environment.empty());
    }
    public static Database createExclusive(java.nio.file.Path path, int pageSize, long cacheBytes, Credential credential, Environment environment) {
        try { return new Database(FileDatabase.createExclusive(path, pageSize, cacheBytes, credential, environment), environment); }
        catch (java.io.IOException failure) { throw Engine.storageError(failure); }
    }
    public static Database openExclusive(java.nio.file.Path path, long cacheBytes) {
        return openExclusive(path, cacheBytes, Environment.empty());
    }
    public static Database openExclusive(java.nio.file.Path path, long cacheBytes, Environment environment) {
        return openExclusive(path, cacheBytes, null, environment);
    }
    public static Database openExclusive(java.nio.file.Path path, long cacheBytes, Credential credential) {
        return openExclusive(path, cacheBytes, credential, Environment.empty());
    }
    public static Database openExclusive(java.nio.file.Path path, long cacheBytes, Credential credential, Environment environment) {
        try { return new Database(FileDatabase.openExclusive(path, cacheBytes, credential, environment), environment); }
        catch (java.io.IOException failure) { throw Engine.storageError(failure); }
    }
    static Database openPageClient(java.nio.file.Path endpoint, long cacheBytes, Credential credential, Environment environment) {
        org.jed.host.PageConnection connection = null;
        try {
            connection = org.jed.host.PageConnection.connect(endpoint, org.jed.storage.PageFormat.VERSION, false, notice -> { });
            return new Database(FileDatabase.openClient(null, connection, cacheBytes, credential, environment), environment);
        } catch (java.io.IOException failure) { if (connection != null) connection.close(); throw new SqlException("08006", failure.getMessage()); }
        catch (RuntimeException | Error failure) { if (connection != null) connection.close(); throw failure; }
    }
    public static Database create(java.nio.file.Path path) { return create(path, FileOptions.defaults()); }
    /** Opens a client over a host-provided authenticated duplex stream. The identity orders writer acquisition. */
    public static Database openRemote(String identity, PageTransport transport, FileOptions options) {
        java.util.Objects.requireNonNull(transport); java.util.Objects.requireNonNull(options);
        if (identity == null || identity.isEmpty()) throw new SqlException("22023", "remote identity must not be empty");
        org.jed.host.PageConnection connection = null;
        try {
            connection = connectTransport(transport, options);
            Database database = new Database(FileDatabase.openClient(null, connection, options.cacheBytes(), options.credential(), options.environment()), options.environment());
            database.pageTransport = transport; database.transportIdentity = identity;
            database.file.enableReadAhead();
            database.fileOptions = options.toBuilder().ownership(FileOptions.Ownership.NEVER).build();
            database.maxStorageBytes = options.maxStorageBytes(); database.configureFile(); return database;
        } catch (java.io.IOException failure) { if (connection != null) connection.close(); throw Engine.storageError(failure); }
        catch (RuntimeException | Error failure) { if (connection != null) connection.close(); throw failure; }
    }
    private static org.jed.host.PageConnection connectTransport(PageTransport transport, FileOptions options) throws java.io.IOException {
        var future = new java.util.concurrent.CompletableFuture<org.jed.host.PageConnection>();
        var opened = new java.util.concurrent.atomic.AtomicReference<PageTransport.Connection>();
        Thread connector = Thread.ofVirtual().name("jed-stream-connect").start(() -> {
            try {
                var streams = transport.open(); opened.set(streams);
                if (future.isCancelled()) { streams.close(); return; }
                Credential credential = options.credential();
                var wire = credential == null ? org.jed.host.PageProtocol.WireCredential.none()
                    : new org.jed.host.PageProtocol.WireCredential(credential.kind,
                        credential.kind == 1 ? new String(credential.bytes, java.nio.charset.StandardCharsets.UTF_8) : "",
                        credential.kind == 2 ? credential.bytes.clone() : new byte[0]);
                var hello = new org.jed.host.PageProtocol.Hello(1, 1, org.jed.storage.PageFormat.VERSION, options.readOnly() ? 1 : 0, "jed-java", wire);
                var connection = new org.jed.host.PageConnection(streams.input(), streams.output(), streams, hello, notice -> { });
                if (!future.complete(connection)) connection.close();
            } catch (Exception failure) {
                var streams = opened.get(); if (streams != null) try { streams.close(); } catch (java.io.IOException cleanup) { failure.addSuppressed(cleanup); }
                future.completeExceptionally(failure);
            }
        });
        try { return future.get(Math.max(1, options.lockTimeoutMillis()), java.util.concurrent.TimeUnit.MILLISECONDS); }
        catch (java.util.concurrent.ExecutionException failure) {
            if (failure.getCause() instanceof java.io.IOException io) throw io;
            if (failure.getCause() instanceof RuntimeException runtime) throw runtime;
            throw new java.io.IOException("stream connection failed", failure.getCause());
        } catch (InterruptedException | java.util.concurrent.TimeoutException failure) {
            future.cancel(false); connector.interrupt(); var streams = opened.get(); if (streams != null) streams.close();
            if (failure instanceof InterruptedException) Thread.currentThread().interrupt();
            throw new java.io.IOException("stream connection interrupted or timed out", failure);
        }
    }
    public static Database create(java.nio.file.Path path, FileOptions options) {
        java.util.Objects.requireNonNull(options);
        if (options.readOnly()) throw new SqlException("22023", "cannot create a read-only database");
        if (options.storageHost != null) return storageHostDatabase(path, options, true);
        Database database = createExclusive(path, options.pageSize(), options.cacheBytes(), options.credential(), options.environment());
        database.fileOptions = options; database.maxStorageBytes = options.maxStorageBytes(); database.configureFile();
        try { if (options.serve()) database.startPageService(); return database; }
        catch (RuntimeException | Error failure) { database.close(); throw failure; }
    }
    public static Database open(java.nio.file.Path path) { return open(path, FileOptions.defaults()); }
    public static Database open(java.nio.file.Path path, FileOptions options) {
        java.util.Objects.requireNonNull(options);
        if (options.storageHost != null) return storageHostDatabase(path, options, false);
        org.jed.host.ExclusiveFileLock.Claim claim = null;
        try {
            claim = org.jed.host.ExclusiveFileLock.claim(path, false);
            long started = System.nanoTime(); String detail = "database is owned by another process";
            do {
                if (options.ownership() != FileOptions.Ownership.NEVER) {
                    var ownership = claim.tryOwn(new java.security.SecureRandom()::nextBytes);
                    if (ownership != null) {
                        Database database = new Database(FileDatabase.openOwned(ownership, options.cacheBytes(), options.credential(), options.environment(), options.readOnly()), options.environment());
                        database.fileOptions = options; database.maxStorageBytes = options.maxStorageBytes(); database.configureFile(); claim = null;
                        try { if (options.serve()) database.startPageService(); return database; }
                        catch (RuntimeException | Error failure) { database.close(); throw failure; }
                    }
                }
                var owner = claim.ownerRecord();
                if (owner != null) {
                    if (owner.uid() != org.jed.host.ExclusiveFileLock.uid()) detail = "owned by another user's process";
                    else if (!owner.serving()) detail = "owned by a non-serving process";
                    else {
                        org.jed.host.PageConnection connection = null;
                        try {
                            connection = org.jed.host.PageConnection.connect(java.nio.file.Path.of(owner.endpoint()), org.jed.storage.PageFormat.VERSION, options.readOnly(), notice -> { }, Math.max(1, options.lockTimeoutMillis() - (System.nanoTime() - started) / 1_000_000));
                            Database database = new Database(FileDatabase.openClient(claim, connection, options.cacheBytes(), options.credential(), options.environment()), options.environment());
                            database.fileOptions = options; database.maxStorageBytes = options.maxStorageBytes(); database.configureFile(); database.file.configure(options); claim = null; return database;
                        } catch (org.jed.host.PageConnection.RemoteFailure failure) { if (connection != null) connection.close(); throw new SqlException(failure.sqlState(), failure.getMessage()); }
                        catch (java.io.IOException failure) { if (connection != null) connection.close(); }
                        catch (RuntimeException | Error failure) { if (connection != null) connection.close(); throw failure; }
                    }
                }
                if ((System.nanoTime() - started) / 1_000_000 >= options.lockTimeoutMillis()) break;
                try { Thread.sleep(Math.min(25, options.lockTimeoutMillis())); }
                catch (InterruptedException failure) { Thread.currentThread().interrupt(); throw new SqlException("57014", "file election interrupted"); }
            } while (true);
            throw new SqlException("55006", detail);
        } catch (org.jed.host.ExclusiveFileLock.BusyException failure) { throw new SqlException("55006", failure.getMessage()); }
        catch (org.jed.host.ExclusiveFileLock.ProtocolException failure) { throw new SqlException("0A000", failure.getMessage()); }
        catch (java.io.IOException failure) { throw Engine.storageError(failure); }
        finally { if (claim != null) try { claim.close(); } catch (java.io.IOException failure) { throw Engine.storageError(failure); } }
    }
    private static Database storageHostDatabase(java.nio.file.Path path, FileOptions options, boolean create) {
        if (options.serve() && options.roleHost == null) throw new IllegalArgumentException("custom file host requires an explicit role adapter");
        org.jed.host.PageConnection connection = null; Database database = null;
        try {
            if (options.roleHost != null) connection = options.roleHost.elect(path, options.readOnly());
            if (create && connection != null) throw new SqlException("55006", "database is owned by another process");
            FileDatabase file = connection != null ? FileDatabase.openClient(null, connection, options.cacheBytes(), options.credential(), options.environment())
                : create ? FileDatabase.createHost(null, options.storageHost, path, options.pageSize(), options.cacheBytes(), options.credential(), options.environment())
                : FileDatabase.openHost(null, options.storageHost, path, options.cacheBytes(), options.credential(), options.environment(), options.readOnly(), false, null);
            database = new Database(file, options.environment()); database.fileOptions = options; database.injectedRolePath = path;
            database.maxStorageBytes = options.maxStorageBytes(); database.configureFile();
            if (options.roleHost != null && connection == null) database.publishInjectedOwner();
            return database;
        } catch (java.io.IOException | RuntimeException | Error failure) {
            if (connection != null) connection.close();
            if (database != null) database.close(); else if (options.roleHost != null) options.roleHost.release(path);
            if (failure instanceof java.io.IOException io) throw Engine.storageError(io);
            if (failure instanceof RuntimeException runtime) throw runtime;
            throw (Error) failure;
        }
    }
    private void publishInjectedOwner() throws java.io.IOException {
        file.serviceIncarnation(fileOptions.roleHost.incarnation(injectedRolePath));
        pageService = new PageService(this, file, true);
        fileOptions.roleHost.publish(injectedRolePath, this, pageService);
    }
    /** Crash-clean host release: no flush, session rollback or graceful network drain. */
    synchronized void abortInjectedHost() {
        if (fileOptions.storageHost == null) throw new IllegalStateException("injected storage host required");
        closed = true; fileClosed = true; notifyAll();
        if (pageService != null) pageService.close();
        for (Attachment attachment : attachments.values()) attachment.database().abortInjectedHost();
        try { file.abortHost(); } catch (java.io.IOException failure) { throw Engine.storageError(failure); }
        finally { if (fileOptions.roleHost != null) fileOptions.roleHost.release(injectedRolePath); }
    }
    synchronized void beginOwnerDrain() { ownerDraining = true; }
    synchronized void cancelOwnerDrain() { ownerDraining = false; notifyAll(); }
    synchronized boolean ownerBusy() { return writer != null || transactionWriter != null || maintenance != 0 || file != null && file.hasLocalPins(); }
    synchronized void yieldOwner(PageService service, FileDatabase previous) throws java.io.IOException {
        if (closed || pageService != service || file != previous) return;
        service.close(); pageService = null;
        disconnectedClaim = previous.releaseOwnership();
        promotionPausedUntil = System.nanoTime() + 2_000_000_000L;
        writer = null; ownerDraining = false; notifyAll();
    }
    private void awaitOwnerDrain() {
        while (ownerDraining && !closed) {
            try { wait(25); } catch (InterruptedException failure) { Thread.currentThread().interrupt(); throw new SqlException("57014", "ownership handoff interrupted"); }
        }
    }
    /** Re-election occurs at new transaction boundaries; existing pins retain their failed role. */
    private synchronized void ensureRole() {
        if (roleFailure != null) throw roleFailure;
        if (file == null || file.connected()) return;
        if (closed) throw new SqlException("55000", "database is closed");
        var codec = file.retainedCodec();
        promotionPausedUntil = Math.max(promotionPausedUntil, file.promotionPausedUntil());
        try {
            if (fileOptions.roleHost != null) {
                file.close(); writer = null; notifyAll();
                var connection = fileOptions.roleHost.elect(injectedRolePath, fileOptions.readOnly());
                if (connection == null) {
                    try { file = FileDatabase.openHost(null, fileOptions.storageHost, injectedRolePath, fileOptions.cacheBytes(), null, environment, fileOptions.readOnly(), true, codec); }
                    catch (java.io.IOException | RuntimeException | Error failure) { fileOptions.roleHost.release(injectedRolePath); throw failure; }
                    committed = file.snapshot(); configureFile(); publishInjectedOwner();
                } else {
                    try { file = FileDatabase.openClient(null, connection, fileOptions.cacheBytes(), null, environment, true, codec); committed = file.snapshot(); file.configure(fileOptions); }
                    catch (java.io.IOException | RuntimeException | Error failure) { connection.close(); throw failure; }
                }
                return;
            }
            if (pageTransport != null) {
                file.close(); writer = null; notifyAll();
                var connection = connectTransport(pageTransport, fileOptions);
                try { file = FileDatabase.openClient(null, connection, fileOptions.cacheBytes(), null, environment, true, codec); file.enableReadAhead(); committed = file.snapshot(); file.configure(fileOptions); return; }
                catch (java.io.IOException | RuntimeException | Error failure) { connection.close(); throw failure; }
            }
            if (disconnectedClaim == null) {
                disconnectedClaim = file.detachClientClaim();
                if (disconnectedClaim == null) throw new SqlException("08006", "page connection requires reopen");
                file.close(); writer = null; notifyAll();
            }
            long started = System.nanoTime();
            do {
                if (fileOptions.ownership() != FileOptions.Ownership.NEVER && System.nanoTime() >= promotionPausedUntil) {
                    var ownership = disconnectedClaim.tryOwn(new java.security.SecureRandom()::nextBytes);
                    if (ownership != null) {
                        try { file = FileDatabase.openOwned(ownership, fileOptions.cacheBytes(), null, environment, fileOptions.readOnly(), true, codec); }
                        catch (java.io.IOException failure) { roleFailure = Engine.storageError(failure); throw roleFailure; }
                        catch (SqlException failure) { roleFailure = failure; throw failure; }
                        disconnectedClaim = null; committed = file.snapshot();
                        configureFile(); if (fileOptions.serve()) startPageService(); return;
                    }
                }
                var owner = disconnectedClaim.ownerRecord();
                if (owner != null && owner.uid() == org.jed.host.ExclusiveFileLock.uid() && owner.serving()) {
                    org.jed.host.PageConnection connection = null;
                    try {
                        connection = org.jed.host.PageConnection.connect(java.nio.file.Path.of(owner.endpoint()), org.jed.storage.PageFormat.VERSION, fileOptions.readOnly(), notice -> { }, Math.max(1, fileOptions.lockTimeoutMillis() - (System.nanoTime() - started) / 1_000_000));
                        FileDatabase replacement = FileDatabase.openClient(disconnectedClaim, connection, fileOptions.cacheBytes(), null, environment, true, codec);
                        file = replacement; disconnectedClaim = null; committed = file.snapshot(); file.configure(fileOptions); return;
                    } catch (org.jed.host.PageConnection.RemoteFailure failure) { if (connection != null) connection.close(); if (!failure.sqlState().equals("53300")) throw new SqlException(failure.sqlState(), failure.getMessage()); }
                    catch (java.io.IOException failure) { if (connection != null) connection.close(); }
                    catch (RuntimeException | Error failure) { if (connection != null) connection.close(); throw failure; }
                }
                if ((System.nanoTime() - started) / 1_000_000 >= fileOptions.lockTimeoutMillis()) break;
                try { Thread.sleep(Math.min(25, Math.max(1, fileOptions.lockTimeoutMillis()))); }
                catch (InterruptedException failure) { Thread.currentThread().interrupt(); throw new SqlException("57014", "file election interrupted"); }
                if (closed) throw new SqlException("55000", "database is closed");
            } while (true);
            throw new SqlException("08006", "page owner unavailable after connection loss");
        } catch (java.io.IOException failure) { throw pageError(failure); }
    }
    int pageSize() { return file == null ? memory.pageSize() : file.pageSize(); }
    org.jed.storage.PageLayout pageLayout() { return file == null ? new org.jed.storage.PageLayout(memory.pageSize(), false) : file.layout(); }
    org.jed.storage.PageCodec pageCodec() { return file == null ? org.jed.storage.PageCodec.PLAINTEXT : file.pageCodec(); }
    /** Experimental test seam; the shared backup API naming remains a product decision. */
    void backupForTesting(java.nio.file.Path destination, boolean pageIdentical) {
        FileDatabase retained;
        synchronized (this) { if (closed) throw new SqlException("55000", "database is closed"); ensureRole(); retained = file; maintenance++; }
        try {
            if (retained == null) {
                try (Pin pin = pinSnapshot()) { Backup.write(fileOptions.storageHost == null ? FileHost.NATIVE : fileOptions.storageHost, fileOptions.storageHost == null ? destination.toAbsolutePath().normalize() : destination, pin.snapshot(), null, null, pageSize(), org.jed.storage.PageCodec.PLAINTEXT, null, pageIdentical); }
            } else try (var lease = backupLease()) {
                Backup.write(fileOptions.storageHost == null ? FileHost.NATIVE : fileOptions.storageHost, fileOptions.storageHost == null ? destination.toAbsolutePath().normalize() : destination, lease.snapshot(), lease.pager(), lease.selected(), lease.pager().pageSize(), lease.pager().codec(), lease.keyBlock(), pageIdentical);
            }
        } catch (java.io.IOException failure) { throw Engine.storageError(failure); }
        finally { synchronized (this) { maintenance--; if (closed && sessions.isEmpty()) closeFile(); } }
    }
    private synchronized FileDatabase.BackupLease backupLease() throws java.io.IOException {
        long started = System.nanoTime();
        for (;;) {
            awaitOwnerDrain(); ensureRole();
            try { return file.backupLease(); }
            catch (java.io.IOException failure) {
                if (file.client() && !file.connected() && !(failure instanceof org.jed.host.PageConnection.RemoteFailure)
                    && (System.nanoTime() - started) / 1_000_000 < fileOptions.lockTimeoutMillis()) continue;
                throw failure;
            }
        }
    }
    public void compact(String name) {
        synchronized (publicationLock) { synchronized (this) {
            Database target = namedDatabase(name); requireWritable(name);
            if (writer != null || transactionWriter != null || snapshotPins != 0 || maintenance != 0) throw new SqlException("55006", "database is in use");
            synchronized (target) {
                if (target.writer != null || target.snapshotPins != 0 || target.maintenance != 0) throw new SqlException("55006", "database is in use");
                try {
                    if (target.file == null) target.committed = target.memory.compactImage();
                    else { target.file.compact(); target.committed = target.file.snapshot(); }
                } catch (java.io.IOException failure) { throw Engine.storageError(failure); }
            }
        } }
    }
    /** Rotate the data key, encrypt a plaintext file, or decrypt when credential is null. */
    public void rekey(Credential credential) { rekey("main", credential); }
    public void rekey(String name, Credential credential) {
        Database target; FileDatabase retained;
        synchronized (this) {
            if (closed) throw new SqlException("55000", "database is closed");
            target = namedDatabase(name); requireWritable(name);
            synchronized (target) {
                if (target.file == null) throw new SqlException("55000", "rekey requires a file database");
                if (credential != null && !protectedScratch()) throw new SqlException("55000", "encryption requires protected scratch files");
                retained = target.file; maintenance++; if (target != this) target.maintenance++;
            }
        }
        try {
            var prepared = retained.prepareRekey(credential, target.environment);
            synchronized (publicationLock) { synchronized (this) {
                if (closed) throw new SqlException("55000", "database is closed");
                if (namedDatabase(name) != target) throw new SqlException("42704", "database attachment changed during derivation");
                if (writer != null || transactionWriter != null || snapshotPins != 0 || maintenance != 1) throw new SqlException("55006", "database is in use");
                synchronized (target) {
                    if (target.writer != null || target.transactionWriter != null || target.snapshotPins != 0 || target.maintenance != 1) throw new SqlException("55006", "database is in use");
                    boolean protectedFiles = target.protectedScratch();
                    retained.rekey(prepared, target.environment, target.maxStorageBytes);
                    target.committed = retained.snapshot();
                    target.fileOptions = target.fileOptions.toBuilder().credential(credential).protectedScratch(protectedFiles).build();
                }
            } }
        } catch (java.io.IOException failure) { throw Engine.storageError(failure); }
        finally { releaseDerivation(target); }
    }
    private void releaseDerivation(Database target) {
        synchronized (this) {
            if (target != this) synchronized (target) { target.maintenance--; if (target.closed && target.sessions.isEmpty()) target.closeFile(); }
            maintenance--; if (closed && sessions.isEmpty()) closeFile();
        }
    }
    boolean protectedScratch() { return scratchOwner != null ? scratchOwner.protectedScratch() : encrypted() || fileOptions.protectedScratch(); }
    public boolean encrypted() { return file != null && file.layout().encrypted(); }
    public void changeCredential(Credential credential) { changeCredential("main", credential); }
    public void changeCredential(String name, Credential credential) {
        java.util.Objects.requireNonNull(credential, "credential");
        Database target; FileDatabase retained;
        synchronized (this) {
            if (closed) throw new SqlException("55000", "database is closed");
            target = namedDatabase(name); requireWritable(name);
            synchronized (target) {
                if (target.file == null) throw new SqlException("55000", "in-memory database has no credential");
                if (!target.encrypted()) throw new SqlException("28P03", "credential does not apply to plaintext database");
                retained = target.file; maintenance++; if (target != this) target.maintenance++;
            }
        }
        try {
            var change = retained.prepareCredential(credential);
            synchronized (publicationLock) { synchronized (this) {
                if (closed) throw new SqlException("55000", "database is closed");
                if (namedDatabase(name) != target) throw new SqlException("42704", "database attachment changed during derivation");
                if (writer != null || transactionWriter != null || maintenance != 1) throw new SqlException("55006", "database writer is busy");
                synchronized (target) {
                    if (target.writer != null || target.transactionWriter != null || target.maintenance != 1) throw new SqlException("55006", "database writer is busy");
                    retained.changeCredential(change);
                    target.fileOptions = target.fileOptions.toBuilder().credential(credential).build();
                }
            } }
        } catch (java.io.IOException failure) { throw Engine.storageError(failure); }
        finally { releaseDerivation(target); }
    }
    ScratchHost scratchHost() { return scratchOwner != null ? scratchOwner.scratchHost() : fileOptions.scratchHost; }
    java.nio.file.Path scratchDirectory() { return scratchOwner != null ? scratchOwner.scratchDirectory() : file == null ? null : java.nio.file.Path.of(System.getProperty("java.io.tmpdir")); }
    public PreparedStatement prepare(String sql) { try (Session session = session()) { return session.prepare(sql); } }
    public Rows query(String sql, SqlValue... arguments) {
        Session session = session();
        try { return session.query(sql, arguments).ownSession(); }
        catch (RuntimeException | Error failure) { session.close(); throw failure; }
    }
    public Rows queryPrepared(PreparedStatement statement, SqlValue... arguments) {
        Session session = session();
        try { return session.queryPrepared(statement, arguments).ownSession(); }
        catch (RuntimeException | Error failure) { session.close(); throw failure; }
    }
    public long execute(String sql, SqlValue... arguments) { try (Session session = session()) { return session.execute(sql, arguments); } }
    public long executePrepared(PreparedStatement statement, SqlValue... arguments) { try (Session session = session()) { return session.executePrepared(statement, arguments); } }
    public synchronized Session session() {
        if (closed) throw new SqlException("55000", "database is closed");
        Session session = new Session(this);
        sessions.add(session);
        return session;
    }
    /** A pinned read handle remains usable after statement errors and closes without publication. */
    public Session read() {
        synchronized (publicationLock) { synchronized (this) {
            if (closed) throw new SqlException("55000", "database is closed");
            Session session = new Session(this, true); sessions.add(session); return session;
        } }
    }
    synchronized long oldestLive() { return file == null ? memory.oldestLive() : file.oldestLive(); }
    synchronized void release(Session session) { sessions.remove(session); if (closed && sessions.isEmpty()) closeFile(); }
    final class Pin implements AutoCloseable {
        private final Engine.Snapshot snapshot;
        private final FileDatabase role = file;
        private final AutoCloseable lease;
        private final java.util.List<Pin> dependencies;
        private boolean released;
        private Pin(Engine.Snapshot snapshot, AutoCloseable lease) { this(snapshot, lease, java.util.List.of()); }
        private Pin(Engine.Snapshot snapshot, AutoCloseable lease, java.util.List<Pin> dependencies) { this.snapshot = snapshot; this.lease = lease; this.dependencies = java.util.List.copyOf(dependencies); snapshotPins++; }
        Engine.Snapshot snapshot() { check(); return snapshot; }
        void check() { synchronized (Database.this) { if (role != file || role != null && !role.connected()) throw new SqlException("08006", "transaction began before the page connection changed"); for (Pin pin : dependencies) pin.check(); } }
        public void close() {
            synchronized (Database.this) { if (released) return; released = true; snapshotPins--; }
            try { lease.close(); } catch (java.io.IOException failure) { throw Engine.storageError(failure); }
            catch (RuntimeException failure) { throw failure; } catch (Exception failure) { throw new IllegalStateException(failure); }
        }

    }
    synchronized Pin pinSnapshot() {
        long started = System.nanoTime();
        for (;;) {
            awaitOwnerDrain(); ensureRole();
            if (closed) throw new SqlException("55000", "database is closed");
            if (file == null) { var value = snapshot(); return new Pin(value, memory.pin(value)); }
            try { var pinned = file.readLease(); committed = pinned.snapshot();
                try { CollationCompatibility.validate(committed, environment); } catch (RuntimeException | Error failure) { try { pinned.release().close(); } catch (Exception close) { failure.addSuppressed(close); } throw failure; }
                return new Pin(pinned.snapshot(), pinned.release()); }
            catch (java.io.IOException failure) {
                if (file.client() && !file.connected() && !(failure instanceof org.jed.host.PageConnection.RemoteFailure) && (System.nanoTime()-started)/1_000_000 < fileOptions.lockTimeoutMillis()) continue;
                throw pageError(failure);
            }
        }
    }
    synchronized Pin pinWriterSnapshot() { return pin(snapshot()); }
    Pin pinWriterSnapshot(Session session) { synchronized (publicationLock) { return pin(session, snapshot(session)); } }
    Pin pinSnapshot(Session session) {
        synchronized (publicationLock) {
            java.util.Map<String, Attachment> attached;
            synchronized (this) { if (closed) throw new SqlException("55000", "database is closed"); attached = new java.util.TreeMap<>(attachments); snapshotPins++; }
            java.util.List<Pin> acquired = new java.util.ArrayList<>();
            try {
                Pin main = pinSnapshot(); acquired.add(main);
                var roots = new java.util.TreeMap<String, Engine.Snapshot>();
                for (var entry : attached.entrySet()) { Pin pin = entry.getValue().database().pinSnapshot(); acquired.add(pin); roots.put(entry.getKey(), pin.snapshot()); }
                if (session != null) { var temp = session.tempSnapshot(); roots.put("temp", temp); if (temp.version() != 0) acquired.add(session.tempDatabase().pin(temp)); }
                var value = main.snapshot().withDomains(roots);
                synchronized (this) { return new Pin(value, () -> closePins(acquired), acquired); }
            } catch (RuntimeException | Error failure) { try { closePins(acquired); } catch (RuntimeException cleanup) { failure.addSuppressed(cleanup); } throw failure; }
            finally { synchronized (this) { snapshotPins--; } }
        }
    }
    private static void closePins(java.util.List<Pin> pins) { RuntimeException failed = null;
        for (int i = pins.size() - 1; i >= 0; i--) try { pins.get(i).close(); } catch (RuntimeException failure) { if (failed == null) failed = failure; else failed.addSuppressed(failure); }
        if (failed != null) throw failed;
    }
    Pin pin(Session session, Engine.Snapshot value) {
        synchronized (publicationLock) {
            synchronized (this) { if (closed) throw new SqlException("55000", "database is closed"); snapshotPins++; }
            java.util.List<Pin> acquired = new java.util.ArrayList<>();
            try {
                acquired.add(pin(value.withoutDomains()));
                for (var entry : value.domains().entrySet()) if (!entry.getKey().equals("main")) acquired.add(domainDatabase(session, entry.getKey()).pin(entry.getValue()));
                synchronized (this) { return new Pin(value, () -> closePins(acquired), acquired); }
            } catch (RuntimeException | Error failure) { try { closePins(acquired); } catch (RuntimeException cleanup) { failure.addSuppressed(cleanup); } throw failure; }
            finally { synchronized (this) { snapshotPins--; } }
        }
    }
    synchronized Pin pin(Engine.Snapshot snapshot) {
        if (closed) throw new SqlException("55000", "database is closed");
        try { return new Pin(snapshot, file == null ? memory.pin(snapshot) : file.pinLease(snapshot)); }
        catch (java.io.IOException failure) { throw pageError(failure); }
    }
    synchronized Engine.Snapshot snapshot() {
        ensureRole();
        if (closed) throw new SqlException("55000", "database is closed");
        if (file != null) committed = file.snapshot();
        CollationCompatibility.validate(committed, environment);
        return committed;
    }
    Engine.Snapshot snapshot(Session session) {
        synchronized (publicationLock) {
            java.util.Map<String, Attachment> attached;
            synchronized (this) { attached = new java.util.TreeMap<>(attachments); snapshotPins++; }
            try {
                var roots = new java.util.TreeMap<String, Engine.Snapshot>();
                for (var entry : attached.entrySet()) roots.put(entry.getKey(), entry.getValue().database().snapshot());
                if (session != null) roots.put("temp", session.tempSnapshot());
                return snapshot().withDomains(roots);
            } finally { synchronized (this) { snapshotPins--; } }
        }
    }
    void acquireWriter(Session session, CostMeter meter) {
        long started = System.nanoTime();
        java.util.List<Database> domains = new java.util.ArrayList<>();
        synchronized (this) {
            while (transactionWriter != null && transactionWriter != session) {
                meter.guard();
                if (session.lockTimeoutMillis() != 0 && (System.nanoTime() - started) / 1_000_000 >= session.lockTimeoutMillis())
                    throw new SqlException("55P03", "database writer gate timeout");
                if (closed || session.closed()) throw new SqlException("55000", "database or session is closed");
                try { wait(25); } catch (InterruptedException failure) { Thread.currentThread().interrupt(); throw new SqlException("57014", "writer wait interrupted"); }
            }
            if (closed || session.closed()) throw new SqlException("55000", "database or session is closed");
            if (transactionWriter == session) return;
            transactionWriter = session;
            if (!fileOptions.readOnly()) domains.add(this);
            for (Attachment attachment : attachments.values()) if (!attachment.readOnly()) domains.add(attachment.database());
            domains.sort((left, right) -> {
                if (left.file == null) return right.file == null ? 0 : -1;
                if (right.file == null) return 1;
                return java.util.Arrays.compareUnsigned(left.writerIdentity().getBytes(java.nio.charset.StandardCharsets.UTF_8),
                    right.writerIdentity().getBytes(java.nio.charset.StandardCharsets.UTF_8));
            });
        }
        java.util.List<Database> acquired = new java.util.ArrayList<>();
        try {
            for (Database domain : domains) { domain.acquireSingleWriter(session, meter, started); acquired.add(domain); }
            synchronized (this) { writerDomains = java.util.List.copyOf(acquired); }
        } catch (RuntimeException | Error failure) {
            for (int i = acquired.size() - 1; i >= 0; i--) try { acquired.get(i).releaseSingleWriter(session); }
                catch (RuntimeException cleanup) { failure.addSuppressed(cleanup); }
            synchronized (this) { transactionWriter = null; notifyAll(); }
            throw failure;
        }
    }
    private void acquireSingleWriter(Session session, CostMeter meter, long started) {
        synchronized (this) {
            awaitOwnerDrain(); ensureRole();
            if (fileOptions.readOnly()) throw new SqlException("25006", "database is read-only");
            while (writer != null && writer != session) {
                meter.guard();
                if (session.lockTimeoutMillis() != 0 && (System.nanoTime() - started) / 1_000_000 >= session.lockTimeoutMillis()) throw new SqlException("55P03", "database writer gate timeout");
                if (closed || session.closed()) throw new SqlException("55000", "database or session is closed");
                try { wait(25); }
                catch (InterruptedException failure) { Thread.currentThread().interrupt(); throw new SqlException("57014", "writer wait interrupted"); }
            }
            if (closed || session.closed()) throw new SqlException("55000", "database or session is closed");
            writer = session;
        }
        // A remote gate wait must not hold the local publication monitor: pinned readers continue.
        try {
            if (file != null && file.ownerReadOnly()) {
                if (fileOptions.ownership() == FileOptions.Ownership.NEVER || !file.requestOwnership()) throw new SqlException("25006", "file owner is read-only");
                while (file.connected()) {
                    meter.guard();
                    if (session.lockTimeoutMillis() != 0 && (System.nanoTime() - started) / 1_000_000 >= session.lockTimeoutMillis()) throw new SqlException("55P03", "ownership handoff timeout");
                    try { Thread.sleep(10); } catch (InterruptedException failure) { Thread.currentThread().interrupt(); throw new SqlException("57014", "ownership handoff interrupted"); }
                }
                synchronized (this) { ensureRole(); writer = session; }
            }
            long remaining = session.lockTimeoutMillis();
            if (remaining != 0) { remaining -= (System.nanoTime() - started) / 1_000_000; if (remaining <= 0) throw new SqlException("55P03", "database writer gate timeout"); }
            if (file != null) file.acquireRemoteWriter(remaining); synchronized (this) { if (file != null) committed = file.snapshot(); }
        }
        catch (java.io.IOException failure) {
            boolean retry;
            synchronized (this) { if (writer == session) { writer = null; notifyAll(); }
                retry = file != null && file.client() && !file.connected() && !(failure instanceof org.jed.host.PageConnection.RemoteFailure) && (System.nanoTime()-started)/1_000_000 < fileOptions.lockTimeoutMillis(); }
            if (retry) { acquireSingleWriter(session, meter, started); return; }
            throw pageError(failure);
        }
        catch (RuntimeException | Error failure) { synchronized (this) { if (writer == session) { writer = null; notifyAll(); } } throw failure; }
    }
    private SqlException pageError(java.io.IOException failure) {
        if (failure instanceof org.jed.host.PageConnection.RemoteFailure remote) return new SqlException(remote.sqlState(), remote.getMessage());
        if (failure instanceof org.jed.host.PageProtocol.Violation) return new SqlException("08P01", failure.getMessage());
        return file != null && file.client() ? new SqlException("08006", failure.getMessage()) : Engine.storageError(failure);
    }
    synchronized String processRole(String name) {
        Database target = namedDatabase(name.isEmpty() ? "main" : name);
        try { target.ensureRole(); }
        catch (SqlException failure) {
            if (target.fileOptions.ownership() != FileOptions.Ownership.NEVER || !failure.sqlState().equals("08006")) throw failure;
        }
        return target.file == null ? "memory" : target.file.client() ? "client" : "owner";
    }
    synchronized long processTxid() { return snapshot().version(); }
    synchronized String cachedRoleForTesting() { return file == null ? "memory" : file.client() ? "client" : "owner"; }
    synchronized java.util.Set<String> attachmentNames() { return java.util.Set.copyOf(attachments.keySet()); }
    synchronized long publishedVersionForTesting() { return file == null ? committed.version() : file.serviceMeta().txid(); }
    synchronized FileDatabase.BackupLease storageLeaseForTesting() throws java.io.IOException { return file.backupLease(); }
    synchronized long durableGenerationForTesting() { return file.durableGeneration(); }
    synchronized boolean batchOpenForTesting(long now) { return file.batchOpen(now); }
    synchronized FileDatabase.Inspection inspectionForTesting() throws java.io.IOException { return file.inspectionForTesting(); }
    synchronized long processPageCount() { snapshot(); return file == null ? memory.storageBytes() / pageSize() : file.serviceMeta().pageCount(); }
    synchronized void interruptNextAcknowledgement(boolean crash) { if (pageService == null) throw new SqlException("55000", "not a serving owner"); pageService.interruptNextAcknowledgement(crash); }
    synchronized java.nio.file.Path relayEndpoint() throws java.io.IOException {
        ensureRole();
        return file.client() ? file.relayEndpoint() : startPageService();
    }
    synchronized java.nio.file.Path startPageService() {
        if (file == null || file.client()) throw new SqlException("55000", "only a file owner has a page service");
        if (closed) throw new SqlException("55000", "database is closed");
        try { if (pageService == null) pageService = new PageService(this, file); return pageService.endpoint(); }
        catch (java.io.IOException failure) { throw Engine.storageError(failure); }
    }
    synchronized void acquireServiceWriter(Object token, long timeoutMillis, java.util.function.BooleanSupplier active) {
        long started = System.nanoTime();
        while (writer != null && writer != token) {
            if (closed || !active.getAsBoolean()) throw new SqlException("08006", "page connection closed");
            if (timeoutMillis != 0 && Long.compareUnsigned((System.nanoTime() - started) / 1_000_000, timeoutMillis) >= 0)
                throw new SqlException("55P03", "page writer gate timeout");
            try { wait(25); } catch (InterruptedException failure) { Thread.currentThread().interrupt(); throw new SqlException("08006", "page writer wait interrupted"); }
        }
        if (closed || !active.getAsBoolean()) throw new SqlException("08006", "page connection closed");
        writer = token;
    }
    synchronized void releaseServiceWriter(Object token) { if (writer == token) { writer = null; notifyAll(); } }
    void releaseWriter(Session session) {
        java.util.List<Database> domains;
        synchronized (this) { if (transactionWriter != session) return; domains = writerDomains; }
        RuntimeException failure = null;
        for (int i = domains.size() - 1; i >= 0; i--) try { domains.get(i).releaseSingleWriter(session); }
            catch (RuntimeException cleanup) { if (failure == null) failure = cleanup; else failure.addSuppressed(cleanup); }
        synchronized (this) { writerDomains = java.util.List.of(); transactionWriter = null; notifyAll(); }
        if (failure != null) throw failure;
    }
    private synchronized void releaseSingleWriter(Session session) { if (writer == session) {
        try { if (file != null) file.releaseRemoteWriter(); }
        catch (java.io.IOException failure) { throw pageError(failure); }
        finally { writer = null; notifyAll(); }
    } }
    synchronized void publish(Engine.Snapshot base, Engine.Snapshot next) { publish(base, next, true); }
    private synchronized void publish(Engine.Snapshot base, Engine.Snapshot next, boolean reuse) { publish(base, next, reuse, false); }
    private synchronized void publish(Engine.Snapshot base, Engine.Snapshot next, boolean reuse, boolean writerPin) {
        if (closed) throw new SqlException("55000", "database is closed");
        if (base != committed) throw new IllegalStateException("writer snapshot is not current");
        try { committed = file == null ? memory.commit(next, maxStorageBytes) : file.commit(next, maxStorageBytes, reuse, writerPin); }
        catch (org.jed.host.PageConnection.CommitLost lost) {
            int status;
            try { ensureRole(); status = file.commitStatus(lost.txid(),lost.digest()); }
            catch (java.io.IOException | RuntimeException unresolved) { throw new SqlException("08007", "commit outcome could not be resolved after connection loss"); }
            if (status == 1) throw new SqlException("08006", "owner confirms the disconnected commit was not published");
            if (status != 0) throw new SqlException("08007", "commit is outside the owner's resolution window");
            committed = file.snapshot();
        }
        catch (java.io.IOException failure) { throw pageError(failure); }
    }
    void publish(Session session, Engine.Snapshot base, Engine.Snapshot next) {
        synchronized (publicationLock) { synchronized (this) { publishDomains(session, base, next); } }
    }
    private void publishDomains(Session session, Engine.Snapshot base, Engine.Snapshot next) {
        if (closed) throw new SqlException("55000", "database is closed");
        record Change(String name, Database database, Engine.Snapshot before, Engine.Snapshot after, boolean reuse) {}
        java.util.List<Change> changes = new java.util.ArrayList<>();
        var roots = new java.util.TreeMap<>(next.domains()); roots.put("main", next.withoutDomains());
        for (var entry : roots.entrySet()) {
            Engine.Snapshot previous = base.domain(entry.getKey()), changed = entry.getValue().withoutDomains();
            if (previous == changed) continue;
            Database target = domainDatabase(session, entry.getKey());
            requireWritable(entry.getKey());
            if (target.committed != previous) throw new IllegalStateException("writer snapshot is not current: " + entry.getKey());
            // Direct attachments share the frozen cores' conservative common-reader gate.
            // A serving attachment instead uses its independent page-service watermark.
            boolean reuse = entry.getKey().equals("main") || target.file == null || target.file.client() || target.pageService != null
                || target.snapshotPins <= (session.hasWriteTransaction() ? 1 : 0);
            changes.add(new Change(entry.getKey(), target, previous, changed, reuse));
        }
        if (changes.stream().filter(change -> change.database().file != null).count() > 1)
            throw new SqlException("0A000", "a transaction may modify at most one durable database");
        java.util.Map<Database, MemoryDomain.Staged> staged = new java.util.IdentityHashMap<>();
        try {
            if (changes.size() > 1) for (Change change : changes) if (change.database().file != null) {
                try { change.database().file.precheck(change.after(), change.database().maxStorageBytes, change.reuse()); }
                catch (SqlException failure) { throw domainLimit(failure, change.name()); }
            }
            for (Change change : changes) if (change.database().memory != null) {
                try { change.database().memory.precheck(change.after(), change.database().maxStorageBytes); }
                catch (SqlException failure) { throw domainLimit(failure, change.name()); }
            }
            for (Change change : changes) if (change.database().memory != null) {
                try { staged.put(change.database(), change.database().memory.stage(change.after(), change.database().maxStorageBytes)); }
                catch (SqlException failure) { throw domainLimit(failure, change.name()); }
            }
            for (Change change : changes) if (change.name().equals("temp"))
                change.database().memory.prepareTempSpill(staged.get(change.database()), scratchDirectory(), session.tempBuffers(),
                    protectedScratch() ? this::fillScratchEntropy : null, scratchHost());
            for (Change change : changes) if (change.database().file != null) {
                try { change.database().publish(change.before(), change.after(), change.reuse(), session.hasWriteTransaction()); }
                catch (SqlException failure) { throw domainLimit(failure, change.name()); }
            }
            for (Change change : changes) if (change.database().memory != null)
                change.database().committed = change.database().memory.adopt(staged.get(change.database()));
        } catch (java.io.IOException failure) { throw Engine.storageError(failure); }
    }
    private static SqlException domainLimit(SqlException failure, String name) {
        return failure.sqlState().equals("54P06") && !name.equals("main")
            ? new SqlException("54P06", failure.getMessage().replace("\"main\"", "\"" + name + "\"")) : failure;
    }
    @Override public void close() {
        Session[] owned;
        synchronized (this) {
            if (closed) return;
            closed = true;
            notifyAll();
            owned = sessions.toArray(Session[]::new);
        }
        if (pageService != null) pageService.shutdown();
        for (Session session : owned) session.close();
        synchronized (this) { if (sessions.isEmpty()) closeFile(); }
    }
    private void closeFile() {
        if (!fileClosed && maintenance == 0) { for (Attachment attachment : attachments.values()) attachment.database().close(); attachments.clear(); }
        if (memory != null && !fileClosed && maintenance == 0) { fileClosed = true; try { memory.close(); } catch (java.io.IOException failure) { throw Engine.storageError(failure); } }
        if (file != null && !fileClosed && maintenance == 0) {
            fileClosed = true;
            if (pageService != null) pageService.close();
            try { file.close(); if (fileOptions.roleHost != null) fileOptions.roleHost.release(injectedRolePath); if (disconnectedClaim != null) { disconnectedClaim.close(); disconnectedClaim = null; } } catch (java.io.IOException failure) { throw Engine.storageError(failure); }
        }
    }
}
