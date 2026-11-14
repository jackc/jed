package org.jed;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeSet;
import java.util.UUID;
import org.jed.host.ExclusiveFileLock;
import org.jed.host.LinuxNative;
import org.jed.storage.*;

/** Exclusive file domain. Immutable readers pin generations independently of writes. */
final class FileDatabase implements AutoCloseable {
    private final ExclusiveFileLock lock;
    private final FileHost host;
    private final Path storagePath;
    private Pager pager;
    private DurableCommitter committer;
    private RemoteBlockStore remote;
    private ExclusiveFileLock.Claim clientClaim;
    private EncryptedPageCodec encryption;
    private KeyBlock.Selected keyBlock;
    private boolean keyStabilized;
    private Engine.Snapshot snapshot;
    private Map<Long, Long> retiredAt = new HashMap<>();
    private Set<Long> held = new HashSet<>();
    private final IdentityHashMap<Engine.Snapshot, Integer> pins = new IdentityHashMap<>();
    private volatile boolean closed;
    private boolean reclamationKnown;
    private boolean catalogRetained;
    private boolean foreignWholesale;
    private FileOptions options = FileOptions.defaults();
    private long batchDeadline;
    private long batchReading;
    private final Map<Long, Integer> servicePins = new HashMap<>();
    private org.jed.host.PageWindows serviceWindows;
    private byte[] injectedIncarnation;
    synchronized void serviceIncarnation(byte[] value) { if (value.length != 16 || serviceWindows != null) throw new IllegalArgumentException("service incarnation"); injectedIncarnation = value.clone(); }
    byte[] serviceIncarnation() { return lock == null ? injectedIncarnation.clone() : lock.incarnation(); }


    private FileDatabase(ExclusiveFileLock lock, Pager pager, PlaintextRecovery.Selected selected) throws IOException {
        this(lock, pager, selected, FileHost.NATIVE, lock == null ? null : lock.path());
    }
    private FileDatabase(ExclusiveFileLock lock, Pager pager, PlaintextRecovery.Selected selected, FileHost host, Path path) throws IOException {
        this.host = host; this.storagePath = path; this.lock = lock; this.pager = pager; encryption = pager.codec() instanceof EncryptedPageCodec codec ? codec : null;
        committer = new DurableCommitter(pager, selected, storagePath == null ? null : () -> host.syncDirectory(storagePath.getParent()), encryption == null ? null : encryption::authenticateMeta);
        snapshot = CatalogCodec.read(pager, selected.meta());
        for (long page : committer.freePages()) retiredAt.put(page, selected.meta().txid());
    }
    static FileDatabase openExclusive(Path path, long cacheBytes) throws IOException {
        return openExclusive(path, cacheBytes, null, Environment.empty());
    }
    static FileDatabase openExclusive(Path path, long cacheBytes, Credential credential, Environment environment) throws IOException {
        return openOwned(acquire(path, false), cacheBytes, credential, environment);
    }
    static FileDatabase openOwned(ExclusiveFileLock lock, long cacheBytes, Credential credential, Environment environment) throws IOException {
        return openOwned(lock, cacheBytes, credential, environment, false);
    }
    static FileDatabase openOwned(ExclusiveFileLock lock, long cacheBytes, Credential credential, Environment environment, boolean readOnly) throws IOException {
        return openOwned(lock, cacheBytes, credential, environment, readOnly, false, null);
    }
    static FileDatabase openOwned(ExclusiveFileLock lock, long cacheBytes, Credential credential, Environment environment, boolean readOnly, boolean retained, EncryptedPageCodec heldCodec) throws IOException {
        return openHost(lock, FileHost.NATIVE, lock.path(), cacheBytes, credential, environment, readOnly, retained, heldCodec);
    }
    static FileDatabase openHost(ExclusiveFileLock lock, FileHost host, Path path, long cacheBytes, Credential credential, Environment environment, boolean readOnly, boolean retained, EncryptedPageCodec heldCodec) throws IOException {
        Pager pager = null;
        BlockStore store = null;
        try {
            store = host.open(path, !readOnly);
            var selected = PlaintextRecovery.recover(store);
            PageCodec codec = PageCodec.PLAINTEXT;
            KeyBlock.Selected keyBlock = null;
            if (selected.meta().encrypted()) {
                int size = selected.meta().pageSize();
                keyBlock = KeyBlock.select(store.readAt(2L * size, size), store.readAt(3L * size, size));
                if (retained && heldCodec == null) throw new SqlException("28P02", "database was encrypted; reopen with a credential");
                EncryptedPageCodec encrypted = heldCodec;
                if (encrypted == null) { byte[] dataKey = KeyBlock.unlock(keyBlock, credential);
                    encrypted = new EncryptedPageCodec(dataKey, environment.newKeyEntropyStream()); java.util.Arrays.fill(dataKey, (byte) 0); }
                try { encrypted.authenticateMeta(selected.slot(), selected.meta().encoded(), manifests(store, selected.meta())); }
                catch (SqlException failure) { if (retained) throw new SqlException("28P04", "database encryption key changed"); throw failure; }
                codec = encrypted;
            } else if (retained && heldCodec != null) throw new SqlException("28P04", "database encryption variant changed");
            else if (!retained && credential != null) throw new SqlException("28P03", "credential does not apply to plaintext database");
            pager = new Pager(store, selected.meta().pageSize(), cacheBytes, false, codec);
            FileDatabase database = new FileDatabase(lock, pager, selected, host, path);
            // A promoted page client retains the DEK, not the credential that
            // authenticates a key block. It adopts the selected pair without
            // repairing it (encrypted.md §14); only credential-open may repair.
            if (!readOnly && !retained && keyBlock != null && keyBlock.repair()) {
                pager.sync(); host.syncDirectory(path.getParent());
                pager.writePage(keyBlock.slot() == 2 ? 3 : 2, keyBlock.bytes()); pager.sync();
                keyBlock = KeyBlock.select(keyBlock.bytes(), keyBlock.bytes()); database.keyStabilized = true;
            }
            database.keyBlock = keyBlock;
            database.reclamationKnown = true; // Reopen seeds the committed free/live accounting; it is not a fresh-file sentinel.
            return database;
        } catch (IOException | RuntimeException | Error failure) {
            try { if (pager != null) pager.close(); else if (store != null) store.close(); } catch (IOException close) { failure.addSuppressed(close); }
            try { if (lock != null) lock.close(); } catch (IOException close) { failure.addSuppressed(close); }
            throw failure;
        }
    }
    static FileDatabase openClient(ExclusiveFileLock.Claim claim, org.jed.host.PageConnection connection, long cacheBytes, Credential credential, Environment environment) throws IOException {
        return openClient(claim, connection, cacheBytes, credential, environment, false, null);
    }
    static FileDatabase openClient(ExclusiveFileLock.Claim claim, org.jed.host.PageConnection connection, long cacheBytes, Credential credential, Environment environment, boolean retained, EncryptedPageCodec heldCodec) throws IOException {
        RemoteBlockStore remote = new RemoteBlockStore(connection); Pager pager = null; AutoCloseable lease = null;
        try {
            var begun = remote.beginRead(); lease = begun.release(); remote.adopt(begun.begun().incarnation(), begun.begun().meta());
            var meta = remote.meta(); PageCodec codec = PageCodec.PLAINTEXT; KeyBlock.Selected keyBlock = null;
            if (meta.encrypted()) {
                keyBlock = KeyBlock.select(remote.readAt(2L * meta.pageSize(), meta.pageSize()), remote.readAt(3L * meta.pageSize(), meta.pageSize()));
                if (retained && heldCodec == null) throw new SqlException("28P02", "database was encrypted; reopen with a credential");
                EncryptedPageCodec encrypted = heldCodec;
                if (encrypted == null) { byte[] key = KeyBlock.unlock(keyBlock, credential); encrypted = new EncryptedPageCodec(key, environment.newKeyEntropyStream()); java.util.Arrays.fill(key, (byte) 0); }
                authenticateServed(encrypted, meta.encoded(), manifests(remote, meta), retained); codec = encrypted;
            } else if (retained && heldCodec != null) throw new SqlException("28P04", "database encryption variant changed");
            else if (!retained && credential != null) throw new SqlException("28P03", "credential does not apply to plaintext database");
            var dependencies = PlaintextRecovery.validateManifest(remote, meta, null);
            var selected = new PlaintextRecovery.Selected(meta, 0, 0, dependencies);
            pager = new Pager(remote, meta.pageSize(), cacheBytes, false, codec);
            FileDatabase file = new FileDatabase(null, pager, selected); file.remote = remote; file.clientClaim = claim; file.keyBlock = keyBlock;
            closeLease(lease); lease = null; return file;
        } catch (IOException | RuntimeException | Error failure) {
            try { if (lease != null) closeLease(lease); } catch (IOException cleanup) { failure.addSuppressed(cleanup); }
            try { if (pager != null) pager.close(); else remote.close(); } catch (IOException cleanup) { failure.addSuppressed(cleanup); }
            throw failure;
        }
    }
    private static void authenticateServed(EncryptedPageCodec codec, byte[] meta, List<ManifestEncoder.Write> manifests, boolean heldKey) {
        try { codec.authenticateMeta(0, meta, manifests); }
        catch (SqlException first) { try { codec.authenticateMeta(1, meta, manifests); }
            catch (SqlException second) { if (heldKey) throw new SqlException("28P04", "database encryption key changed"); throw second; } }
    }
    private synchronized void adoptRemote(byte[] incarnation, byte[] encoded, int invalidate, long[] indices) throws IOException {
        var next = PlaintextRecovery.parseMeta(encoded);
        if (next.encrypted() != layout().encrypted()) throw new SqlException(encryption == null ? "28P02" : "28P04", "database encryption variant changed");
        if (invalidate == 1) pager.invalidateAll(); else if (invalidate == 0) { for (long index : indices) pager.invalidate(index); }
        else throw new org.jed.host.PageProtocol.Violation("invalid cache invalidation mode");
        remote.adopt(incarnation, encoded);
        if (encryption != null) authenticateServed(encryption, encoded, manifests(remote, next), true);
        if (next.txid() != committer.current().meta().txid() || invalidate == 1) {
            var selected = new PlaintextRecovery.Selected(next, 0, 0, PlaintextRecovery.validateManifest(remote, next, null));
            committer = new DurableCommitter(pager, selected, null, encryption == null ? null : encryption::authenticateMeta);
            snapshot = CatalogCodec.read(pager, next); catalogRetained = false;
        }
    }
    boolean client() { return remote != null; }
    void enableReadAhead() { if (remote != null) remote.enableReadAhead(); }
    boolean connected() { return !closed && (remote == null || remote.connected()); }
    int commitStatus(long txid,long digest) throws IOException { return remote == null ? serviceWindows().status(txid,digest) : remote.commitStatus(txid,digest); }
    ExclusiveFileLock.Claim detachClientClaim() { var result = clientClaim; clientClaim = null; return result; }
    EncryptedPageCodec retainedCodec() { return encryption; }
    long promotionPausedUntil() { return remote == null ? 0 : remote.promotionPausedUntil(); }
    void configure(FileOptions options) { this.options = options; if (remote != null && options.ownership() == FileOptions.Ownership.PREFER) remote.preferOwnership(); }
    synchronized void flush() throws IOException { if (remote == null) committer.flush(true); }
    synchronized void flushDue() throws IOException {
        if (remote == null && committer.hasPending() && options.flushIntervalMillis() != 0 && options.environment().clock() >= batchDeadline) committer.flush(true);
    }
    private void batchBefore(boolean relaxed, boolean client) throws IOException {
        if (remote != null) return;
        if (client && !relaxed) { committer.flush(false); return; }
        if (options.flushIntervalMillis() != 0 && (relaxed || committer.hasPending())) {
            batchReading = options.environment().clock();
            if (committer.hasPending() && batchReading >= batchDeadline) committer.flush(!client);
        }
    }
    private void batchAfter(boolean wasPending) {
        if (!wasPending && committer.hasPending()) {
            long interval = options.flushIntervalMillis() > Long.MAX_VALUE / 1000 ? Long.MAX_VALUE : options.flushIntervalMillis() * 1000;
            batchDeadline = batchReading > Long.MAX_VALUE - interval ? Long.MAX_VALUE : batchReading + interval;
        }
    }
    Path path() { return storagePath != null ? storagePath : clientClaim.path(); }
    Path relayEndpoint() throws IOException {
        var owner = clientClaim == null ? null : clientClaim.ownerRecord();
        if (owner == null || !owner.serving()) throw new IOException("page owner unavailable");
        return Path.of(owner.endpoint());
    }
    boolean ownerReadOnly() { return remote != null && remote.ownerReadOnly(); }
    boolean requestOwnership() throws IOException {
        if (clientClaim == null) return false;
        try (FileBlockStore probe = FileBlockStore.open(clientClaim.path(), true)) { /* Permission probe opens no bytes. */ }
        catch (java.nio.file.AccessDeniedException denied) { return false; }
        return remote.requestOwnership();
    }
    synchronized boolean hasLocalPins() { return !pins.isEmpty(); }
    synchronized long oldestLive() { long oldest = serviceMeta().txid(); for (var pin : pins.keySet()) oldest = Math.min(oldest, pin.version()); for (long pin : servicePins.keySet()) oldest = Math.min(oldest, pin); return oldest; }
    synchronized ExclusiveFileLock.Claim releaseOwnership() throws IOException {
        if (remote != null || closed) throw new IOException("not an active file owner");
        committer.flush(true);
        closed = true;
        // No file descriptor may survive releasing presence EX.
        pager.close(); lock.releaseOwnership(); return lock.claim();
    }
    record SnapshotLease(Engine.Snapshot snapshot, AutoCloseable release) { }
    record BackupLease(Engine.Snapshot snapshot, Pager pager, PlaintextRecovery.Selected selected, byte[] keyBlock, AutoCloseable release) implements AutoCloseable {
        public void close() throws IOException { closeLease(release); }
    }
    synchronized BackupLease backupLease() throws IOException {
        var lease = readLease();
        try { return new BackupLease(lease.snapshot(), pager, committer.current(), currentKeyBlock(), lease.release()); }
        catch (IOException | RuntimeException | Error failure) { try { closeLease(lease.release()); } catch (IOException cleanup) { failure.addSuppressed(cleanup); } throw failure; }
    }
    synchronized SnapshotLease readLease() throws IOException {
        if (remote == null) { Engine.Snapshot value = snapshot(); return new SnapshotLease(value, pinLease(value)); }
        var begun = remote.beginRead();
        try { adoptRemote(begun.begun().incarnation(), begun.begun().meta(), begun.begun().invalidate(), begun.begun().indices());
            Engine.Snapshot value = snapshot(); pin(value);
            return new SnapshotLease(value, () -> { synchronized (FileDatabase.this) { unpin(value); closeLease(begun.release()); } });
        } catch (IOException | RuntimeException | Error failure) { try { closeLease(begun.release()); } catch (IOException cleanup) { failure.addSuppressed(cleanup); } throw failure; }
    }
    synchronized AutoCloseable pinLease(Engine.Snapshot value) throws IOException {
        AutoCloseable ownerPin = remote == null ? () -> { } : remote.retain(value.version()); pin(value);
        return () -> { synchronized (FileDatabase.this) { unpin(value); closeLease(ownerPin); } };
    }
    void acquireRemoteWriter(long timeoutMillis) throws IOException {
        if (remote == null) return;
        var begun = remote.beginWrite(timeoutMillis, !options.synchronous());
        try { adoptRemote(begun.incarnation(), begun.meta(), begun.invalidate(), begun.indices()); committer.adoptBatch(begun.pending(), begun.reuse()); }
        catch (IOException | RuntimeException | Error failure) { try { remote.abort(); } catch (IOException cleanup) { failure.addSuppressed(cleanup); } throw failure; }
    }
    void releaseRemoteWriter() throws IOException { if (remote != null) remote.abort(); }
    private static void closeLease(AutoCloseable lease) throws IOException {
        try { lease.close(); } catch (IOException failure) { throw failure; } catch (Exception failure) { throw new IOException("snapshot pin release failed", failure); }
    }
    static FileDatabase createExclusive(Path path, int pageSize, long cacheBytes) throws IOException {
        return createExclusive(path, pageSize, cacheBytes, null, Environment.empty());
    }
    static FileDatabase createExclusive(Path path, int pageSize, long cacheBytes, Credential credential, Environment environment) throws IOException {
        if (!PageFormat.validPageSize(pageSize)) throw new SqlException("22023", "invalid database page size");
        if (credential != null && pageSize < 1024) throw new SqlException("0A000", "encrypted databases require pages of at least 1024 bytes");
        ExclusiveFileLock lock = acquire(path, true);
        return createHost(lock, FileHost.NATIVE, lock.path(), pageSize, cacheBytes, credential, environment);
    }
    static FileDatabase createHost(ExclusiveFileLock lock, FileHost host, Path path, int pageSize, long cacheBytes, Credential credential, Environment environment) throws IOException {
        if (!PageFormat.validPageSize(pageSize)) throw new SqlException("22023", "invalid database page size");
        if (credential != null && pageSize < 1024) throw new SqlException("0A000", "encrypted databases require pages of at least 1024 bytes");
        Path temporary = Path.of(path + ".jedtmp");
        BlockStore store = null;
        Pager pager = null;
        try {
            if (host.exists(path)) throw new SqlException("58P02", "database already exists");
            host.delete(temporary); store = host.create(temporary);
            PageCodec codec = PageCodec.PLAINTEXT; byte[] keyBlock = null;
            if (credential != null) {
                var entropy = environment.newKeyEntropyStream(); byte[] dataKey = new byte[32]; entropy.accept(dataKey);
                keyBlock = KeyBlock.wrap(pageSize, 1, dataKey, credential, entropy);
                codec = new EncryptedPageCodec(dataKey, entropy); java.util.Arrays.fill(dataKey, (byte) 0);
            }
            PageLayout layout = new PageLayout(pageSize, codec.encrypted()); long root = layout.firstBody();
            byte[] page = new byte[pageSize]; page[0] = 1; PageFormat.put16(page, 2, 1); page = codec.sealBody(root, page);
            byte[] meta = ManifestEncoder.encode(layout, 1, root, codec.bodyTag(page), root + 1, 0, new byte[layout.tagBytes()], 1, List.of(), new long[0], List.of()).meta();
            byte[] first = meta, second = meta;
            if (codec instanceof EncryptedPageCodec encrypted) { first = encrypted.sealMeta(0, meta, List.of()); second = encrypted.sealMeta(1, meta, List.of()); }
            store.setSize((root + 1) * pageSize); store.writeAt(0, first); store.writeAt(pageSize, second);
            if (keyBlock != null) { store.writeAt(2L * pageSize, keyBlock); store.writeAt(3L * pageSize, keyBlock); }
            store.writeAt(root * pageSize, page); store.sync();
            host.rename(temporary, path, false); host.syncDirectory(path.getParent());
            var selected = PlaintextRecovery.recover(store, pageSize);
            pager = new Pager(store, pageSize, cacheBytes, false, codec);
            FileDatabase database = new FileDatabase(lock, pager, selected, host, path);
            if (keyBlock != null) database.keyBlock = KeyBlock.select(keyBlock, keyBlock);
            return database;
        } catch (IOException | RuntimeException | Error failure) {
            try { if (pager != null) pager.close(); else if (store != null) store.close(); } catch (IOException close) { failure.addSuppressed(close); }
            try { if (lock != null) lock.close(); } catch (IOException close) { failure.addSuppressed(close); }
            throw failure;
        } finally { host.delete(temporary); }
    }
    private static List<ManifestEncoder.Write> manifests(BlockStore store, PlaintextRecovery.Meta meta) throws IOException {
        List<ManifestEncoder.Write> result = new ArrayList<>();
        for (long id = meta.manifestHead(); id != 0;) {
            byte[] bytes = store.readAt(id * meta.pageSize(), meta.pageSize()); result.add(new ManifestEncoder.Write(id, bytes)); id = PageFormat.u32(bytes, 8);
        }
        return result;
    }
    private static ExclusiveFileLock acquire(Path path, boolean create) throws IOException {
        try { return ExclusiveFileLock.acquire(path, create); }
        catch (ExclusiveFileLock.BusyException failure) { throw new SqlException("55006", failure.getMessage()); }
        catch (ExclusiveFileLock.ProtocolException failure) { throw new SqlException("0A000", failure.getMessage()); }
    }
    int pageSize() { return pager.pageSize(); }
    PageLayout layout() { return pager.layout(); }
    PageCodec pageCodec() { return pager.codec(); }
    synchronized byte[] currentKeyBlock() throws IOException {
        return encryption == null ? null : KeyBlock.select(pager.readRaw(2), pager.readRaw(3)).bytes();
    }
    synchronized void compact() throws IOException { rewriteImage(snapshot(), pager.codec(), currentKeyBlock(), 0); }
    record RekeyTarget(PageCodec codec, byte[] keyBlock) { }
    RekeyTarget prepareRekey(Credential credential, Environment environment) throws IOException {
        if (credential != null && pageSize() < 1024) throw new SqlException("0A000", "encrypted pages require at least 1024 bytes");
        java.util.function.Consumer<byte[]> entropy;
        synchronized (this) {
            if (closed || pager.poisoned()) throw new IOException("file database requires reopen");
            entropy = encryption == null ? environment.newKeyEntropyStream() : encryption.entropyStream();
        }
        PageCodec target = PageCodec.PLAINTEXT; byte[] block = null;
        if (credential != null) {
            byte[] key = new byte[32]; entropy.accept(key);
            try { block = KeyBlock.wrap(pageSize(), 1, key, credential, entropy); target = new EncryptedPageCodec(key, entropy); }
            finally { java.util.Arrays.fill(key, (byte) 0); }
        }
        return new RekeyTarget(target, block);
    }
    synchronized void rekey(RekeyTarget target, Environment environment, long limit) throws IOException {
        checkMaintenance();
        try (var scratch = new RekeyScratch(pageSize(), target.codec().encrypted(), pager.cacheBytes(), environment.newKeyEntropyStream())) {
            rewriteImage(scratch.rebuild(snapshot()), target.codec(), target.keyBlock(), limit);
        }
    }
    private void checkMaintenance() throws IOException {
        if (remote != null || !pins.isEmpty() || !servicePins.isEmpty()) throw new SqlException("55006", "database is in use");
        if (closed || pager.poisoned()) throw new IOException("file database requires reopen");
    }
    private void rewriteImage(Engine.Snapshot before, PageCodec target, byte[] targetKeyBlock, long limit) throws IOException {
        checkMaintenance();
        Path temporary = Path.of(storagePath + ".jedtmp");
        var selected = committer.current(); long cacheBytes = pager.cacheBytes();
        boolean replaced = false, reopenedStore = false;
        try {
            try (BlockStore output = host.rewrite(temporary)) {
                host.permissions(storagePath, temporary);
                var image = ImageWriter.write(before, pageSize(), before.version() + 1, target, targetKeyBlock,
                    (page, bytes) -> output.writeAt(page * pageSize(), bytes));
                if (limit > 0 && image.pageCount() - (target.encrypted() ? 4 : 2) > limit / pageSize()) throw StorageLimit.exceeded(limit);
                output.setSize(image.pageCount() * pageSize()); output.sync();
            }
            int size = pageSize(); PageCodec codec = pager.codec(); pager.close();
            try { host.rename(temporary, storagePath, true); replaced = true; }
            finally {
                BlockStore reopened = host.open(storagePath, true);
                try { pager = new Pager(reopened, size, cacheBytes, false, replaced ? target : codec); reopenedStore = true; }
                catch (IOException | RuntimeException | Error failure) { reopened.close(); throw failure; }
            }
            host.syncDirectory(storagePath.getParent());
            encryption = target instanceof EncryptedPageCodec encrypted ? encrypted : null;
            keyBlock = targetKeyBlock == null ? null : KeyBlock.select(targetKeyBlock, targetKeyBlock);
            keyStabilized = true;
            byte[] first = pager.readRaw(0), second = pager.readRaw(1);
            var recovered = PlaintextRecovery.parseMeta(first);
            committer = new DurableCommitter(pager, new PlaintextRecovery.Selected(recovered, (int) (recovered.txid() & 1), (int) (recovered.txid() & 1), Set.of()),
                () -> host.syncDirectory(storagePath.getParent()), encryption == null ? null : encryption::authenticateMeta);
            snapshot = CatalogCodec.read(pager, recovered); catalogRetained = false; retiredAt.clear(); held.clear(); reclamationKnown = true;
            if (serviceWindows != null) serviceWindows = new org.jed.host.PageWindows(serviceIncarnation(), first, second);
        } catch (IOException | RuntimeException | Error failure) {
            if (replaced) pager.poison();
            else if (reopenedStore) {
                try {
                    committer = new DurableCommitter(pager, selected, () -> host.syncDirectory(storagePath.getParent()), encryption == null ? null : encryption::authenticateMeta);
                    snapshot = CatalogCodec.read(pager, selected.meta());
                } catch (IOException | RuntimeException recovery) { pager.poison(); failure.addSuppressed(recovery); }
            }
            throw failure;
        } finally { host.delete(temporary); }
    }
    record CredentialChange(long previousGeneration, byte[] keyBlock) { }
    CredentialChange prepareCredential(Credential credential) throws IOException {
        long generation;
        synchronized (this) {
            if (remote != null) throw new SqlException("55000", "credential changes require the file owner");
            if (closed || pager.poisoned()) throw new IOException("file database requires reopen");
            if (encryption == null) throw new SqlException("28P03", "credential does not apply to plaintext database");
            generation = keyBlock.generation();
            if (generation == -1L) throw new SqlException("54000", "key-block generation exhausted");
        }
        // Derivation is deliberately outside the file and database publication gates.
        return new CredentialChange(generation, encryption.wrapKey(pageSize(), generation + 1, credential));
    }
    synchronized void changeCredential(CredentialChange change) throws IOException {
        if (closed || pager.poisoned()) throw new IOException("file database requires reopen");
        if (keyBlock.generation() != change.previousGeneration()) throw new SqlException("55006", "database credential changed concurrently");
        // Finish the relaxed data batch before beginning the independent key-block
        // publication. Its durable floor must advance even if credential I/O fails.
        committer.flush(true);
        keyBlock = KeyBlock.publishChange(pager, keyBlock, change.keyBlock(), keyStabilized ? null : () -> host.syncDirectory(storagePath.getParent()));
        committer.coveredByBarrier();
        keyStabilized = true;
    }
    synchronized Engine.Snapshot snapshot() {
        if (pager.revoked()) throw new SqlException("58030", "database storage was revoked after a failed barrier");
        if (snapshot.version() != committer.current().meta().txid()) {
            try { snapshot = CatalogCodec.read(pager, committer.current().meta()); catalogRetained = false; }
            catch (IOException failure) { throw Engine.storageError(failure); }
        }
        return snapshot;
    }
    ExclusiveFileLock ownership() { return lock; }
    synchronized PlaintextRecovery.Meta serviceMeta() { return committer.current().meta(); }
    synchronized long durableGeneration() { return committer.durableGeneration(); }
    synchronized boolean batchOpen(long now) { return committer.hasPending() && (options.flushIntervalMillis() == 0 || now < batchDeadline); }
    record Inspection(PlaintextRecovery.Meta meta, long[] free, long[] reusable, boolean pending, boolean reclaimDue, long leaked) { }
    synchronized Inspection inspectionForTesting() throws IOException {
        var selected = committer.current(); var meta = selected.meta(); FreeList free = FreeList.read(pager, selected);
        long leaked = meta.pageCount() - layout().firstBody() - free.size() - meta.livePages()
            - free.chain().length - meta.manifestPages() - held.size();
        boolean due = !reclamationKnown || leaked > ReclamationSchedule.COMPACT_MIN_PAGES && leaked * ReclamationSchedule.LEAK_DEN > meta.livePages();
        return new Inspection(meta, committer.freePages(), committer.allocatablePages(), committer.hasPending(), due, leaked);
    }
    synchronized org.jed.host.PageWindows serviceWindows() throws IOException {
        if (serviceWindows == null) {
            byte[] previous = null;
            try { previous = PlaintextRecovery.parseMeta(pager.readRaw(committer.current().slot() ^ 1)).encoded(); }
            catch (CorruptStorageException ignored) { }
            serviceWindows = new org.jed.host.PageWindows(serviceIncarnation(), serviceMeta().encoded(), previous);
        }
        return serviceWindows;
    }
    synchronized void servicePin(long txid) { servicePins.merge(txid, 1, Integer::sum); }
    synchronized void serviceUnpin(long txid) {
        Integer count = servicePins.get(txid); if (count == null) throw new IllegalStateException("unbalanced remote pin");
        if (count == 1) servicePins.remove(txid); else servicePins.put(txid, count - 1);
    }
    synchronized boolean serviceReuse() {
        long generation = 0; for (long retired : retiredAt.values()) if (Long.compareUnsigned(retired, generation) > 0) generation = retired;
        for (Engine.Snapshot pin : pins.keySet()) if (Long.compareUnsigned(pin.version(), generation) < 0) return false;
        for (long pin : servicePins.keySet()) if (Long.compareUnsigned(pin, generation) < 0) return false;
        return true;
    }
    synchronized long[] serviceFree() { return committer.allocatablePages(); }
    synchronized void serviceBeginWrite(boolean relaxed) throws IOException { batchBefore(relaxed, true); }
    synchronized List<org.jed.host.PageProtocol.Digest> servicePending() { return committer.pendingEntries().stream().map(e -> new org.jed.host.PageProtocol.Digest(e.page(), e.crc())).toList(); }
    synchronized long[] serviceFloorFree() { return committer.hasPending() ? committer.floorFreePages() : new long[0]; }
    byte[] serviceRead(long index) throws IOException { return pager.readRaw(index); }
    synchronized PageCommitDelta serviceBegin(org.jed.host.PageProtocol.CommitBegin begin, long[] granted) throws IOException {
        PageCommitDelta delta = new PageCommitDelta(serviceMeta(), begin, id -> java.util.Arrays.binarySearch(granted, id) >= 0, committer.pendingEntries());
        if (encryption != null) {
            try { encryption.authenticateMeta(0, begin.meta(), delta.manifests()); }
            catch (SqlException first) { try { encryption.authenticateMeta(1, begin.meta(), delta.manifests()); }
                catch (SqlException second) { throw new org.jed.host.PageProtocol.Violation("client meta authentication failed"); } }
        }
        committer.prepareExternalWrites(); pager.reserve(delta.meta().pageCount());
        for (var page : delta.manifests()) pager.writePage(page.page(), page.bytes());
        return delta;
    }
    void serviceWrite(PageCommitDelta delta, org.jed.host.PageProtocol.IndexedPage page) throws IOException {
        delta.accept(page.index(), page.bytes()); pager.writePage(page.index(), page.bytes());
    }
    synchronized long serviceCommit(PageCommitDelta delta, boolean relaxed) throws IOException {
        delta.complete(); byte[] meta = delta.meta().encoded();
        boolean pending = committer.hasPending();
        if (!pending && relaxed && options.flushIntervalMillis() != 0) batchReading = options.environment().clock();
        if (encryption != null) meta = encryption.sealMeta(committer.current().floorSlot() ^ 1, meta, delta.manifests());
        var published = committer.publishExternal(meta, delta.dependencies(), delta.identities(), !relaxed);
        batchAfter(pending);
        // Retired client pages remain unlisted until an owner maintenance commit.
        // A client may only retain a subset of the owner's free list. Those pages
        // keep their original reuse generation; the client did not retire them.
        Map<Long, Long> retained = new HashMap<>();
        for (long page : committer.freePages()) retained.put(page, retiredAt.getOrDefault(page, published.meta().txid() - 1));
        retiredAt = retained;
        held.clear(); foreignWholesale = true;
        if (serviceWindows != null) serviceWindows.published(meta, delta.dependencies().stream().mapToLong(Long::longValue).toArray());
        return published.meta().txid();
    }

    synchronized void pin(Engine.Snapshot value) { pins.merge(value, 1, Integer::sum); }
    synchronized void unpin(Engine.Snapshot value) {
        Integer count = pins.get(value);
        if (count == null) throw new IllegalStateException("unbalanced file snapshot pin");
        if (count == 1) pins.remove(value); else pins.put(value, count - 1);
    }
    synchronized void serviceMaintenance() throws IOException {
        long next = serviceMeta().txid() + 1;
        if (next == 0) return;
        for (Engine.Snapshot pin : pins.keySet()) if (Long.compareUnsigned(pin.version(), next) < 0) return;
        for (long pin : servicePins.keySet()) if (Long.compareUnsigned(pin, next) < 0) return;
        var before = committer.current(); var free = FreeList.read(pager, before);
        long leaked = before.meta().pageCount() - layout().firstBody() - free.size() - before.meta().livePages()
            - free.chain().length - before.meta().manifestPages() - held.size();
        boolean trailing = free.contains(before.meta().pageCount() - 1);
        if (!reclamationKnown || trailing || leaked > ReclamationSchedule.COMPACT_MIN_PAGES && leaked * ReclamationSchedule.LEAK_DEN > before.meta().livePages())
            commit(snapshot(), true);
    }
    synchronized Engine.Snapshot commit(Engine.Snapshot next) throws IOException { return commit(next, false, 0); }
    synchronized Engine.Snapshot commit(Engine.Snapshot next, long limit) throws IOException { return commit(next, false, limit); }
    synchronized Engine.Snapshot commit(Engine.Snapshot next, long limit, boolean reuse) throws IOException { return commit(next, false, limit, reuse); }
    synchronized Engine.Snapshot commit(Engine.Snapshot next, long limit, boolean reuse, boolean writerPin) throws IOException { return commit(next, false, limit, reuse, writerPin); }
    /** Pure admission for a multi-domain publication, before another domain writes scratch pages. */
    synchronized void precheck(Engine.Snapshot next, long limit) throws IOException { precheck(next, limit, true); }
    synchronized void precheck(Engine.Snapshot next, long limit, boolean reuse) throws IOException {
        if (next == snapshot) return;
        var before = committer.current(); long txid = before.meta().txid() + 1;
        if (txid == 0) throw new TreeCapacityException("transaction id exhausted");
        long watermark = txid;
        for (var pin : pins.keySet()) if (Long.compareUnsigned(pin.version(), watermark) < 0) watermark = pin.version();
        for (long pin : servicePins.keySet()) if (Long.compareUnsigned(pin, watermark) < 0) watermark = pin;
        final long safeWatermark = watermark;
        long[] safe = !reuse || remote != null && !remote.reuse() ? new long[0] : java.util.Arrays.stream(committer.allocatablePages())
            .filter(id -> remote != null || Long.compareUnsigned(retiredAt.getOrDefault(id, 0L), safeWatermark) <= 0).toArray();
        var allocator = new PageAllocator(before.meta().pageCount(), safe);
        PageCodec codec = encryption == null ? pager.codec() : DRAFT_CODEC;
        Plan plan = trees(next, allocator, txid, false, codec);
        if (plan.remaps.size() > layout().remapCapacity()) plan = trees(next, allocator, txid, true, codec);
        List<byte[]> entries = new ArrayList<>();
        for (var type : CatalogCodec.orderedTypes(next)) entries.add(CatalogCodec.definition(type));
        for (var sequence : CatalogCodec.orderedSequences(next)) entries.add(CatalogCodec.sequence(sequence));
        for (var collation : CatalogCodec.orderedCollations(next)) entries.add(CatalogCodec.collation(collation));
        for (var table : CatalogCodec.ordered(next)) entries.add(CatalogCodec.table(table, plan.roots.get(table.name()).page(), plan.roots.get(table.name()).tag(), plan.roots));
        entries.addAll(CatalogCodec.statistics(next));
        for (var view : CatalogCodec.orderedViews(next)) entries.add(CatalogCodec.view(view));
        var catalog = CatalogChain.prepare(pageSize(), entries, plan.allocator, txid, pager, before.meta().root(), before.meta().pageCount(), codec,
            before.meta().rootTag(), !catalogRetained || before.meta().root() == migrateHint() || remote != null && (!plan.writes.isEmpty() || !plan.remaps.equals(before.meta().remaps())));
        long live = before.meta().livePages() + plan.writes.size() - plan.retired.size() + catalog.writes().size() - catalog.retiredPages().size();
        if (live < 0) throw new CorruptStorageException("negative planned live-page count");
        boolean repair = catalog.writes().size() <= catalog.retiredPages().size() && next.tables().values().stream().noneMatch(table ->
            table.rows().hasStagedRecords() || table.indexes().stream().anyMatch(index -> index.entries().hasStagedRecords()));
        if (limit > 0 && live > before.meta().livePages() && live > limit / pageSize() && !repair) throw StorageLimit.exceeded(limit);
    }
    private Engine.Snapshot commit(Engine.Snapshot next, boolean maintenance) throws IOException { return commit(next, maintenance, 0); }
    private Engine.Snapshot commit(Engine.Snapshot next, boolean maintenance, long limit) throws IOException { return commit(next, maintenance, limit, true); }
    private Engine.Snapshot commit(Engine.Snapshot next, boolean maintenance, long limit, boolean reuse) throws IOException { return commit(next, maintenance, limit, reuse, false); }
    private Engine.Snapshot commit(Engine.Snapshot next, boolean maintenance, long limit, boolean reuse, boolean writerPin) throws IOException {
        if (closed) throw new IOException("file database is closed");
        if (!maintenance && next == snapshot) return snapshot;
        batchBefore(!options.synchronous(), false);
        boolean wasPending = committer.hasPending();
        var before = committer.current();
        long txid = before.meta().txid() + 1;
        if (txid == 0) throw new TreeCapacityException("transaction id exhausted");
        long watermark = txid;
        boolean ignoredWriter = false;
        for (var pinned : pins.entrySet()) {
            Engine.Snapshot pin = pinned.getKey(); int count = pinned.getValue();
            if (writerPin && !ignoredWriter && pin.version() == before.meta().txid()) { count--; ignoredWriter = true; }
            if (count != 0 && Long.compareUnsigned(pin.version(), watermark) < 0) watermark = pin.version();
        }
        for (long pin : servicePins.keySet()) if (Long.compareUnsigned(pin, watermark) < 0) watermark = pin;
        final long safeWatermark = watermark;
        long[] safe = !reuse || remote != null && !remote.reuse() ? new long[0] : java.util.Arrays.stream(committer.allocatablePages()).filter(id -> remote != null || Long.compareUnsigned(retiredAt.getOrDefault(id, 0L), safeWatermark) <= 0).toArray();
        PageAllocator allocator = new PageAllocator(before.meta().pageCount(), safe);
        // Decide a remap fold without drawing key entropy. Only the final post-order
        // serialization seals pages; speculative plans must not perturb canonical nonce order.
        PageCodec planner = encryption == null ? pager.codec() : DRAFT_CODEC;
        Plan plan = trees(next, allocator, txid, false, planner);
        boolean fold = plan.remaps.size() > layout().remapCapacity();
        if (fold || encryption != null) plan = trees(next, allocator, txid, fold, pager.codec());
        if (plan.remaps.size() > layout().remapCapacity()) throw new IllegalStateException("remap fold left excess entries");
        List<byte[]> entries = new ArrayList<>();
        for (SqlType type : CatalogCodec.orderedTypes(next)) entries.add(CatalogCodec.definition(type));
        for (Engine.Sequence sequence : CatalogCodec.orderedSequences(next)) entries.add(CatalogCodec.sequence(sequence));
        for (Engine.CollationReference collation : CatalogCodec.orderedCollations(next)) entries.add(CatalogCodec.collation(collation));
        for (Engine.Table table : CatalogCodec.ordered(next)) entries.add(CatalogCodec.table(table, plan.roots.get(table.name()).page(), plan.roots.get(table.name()).tag(), plan.roots));
        entries.addAll(CatalogCodec.statistics(next));
        for (Engine.View view : CatalogCodec.orderedViews(next)) entries.add(CatalogCodec.view(view));
        var catalog = CatalogChain.prepare(pageSize(), entries, plan.allocator, txid, pager, before.meta().root(), before.meta().pageCount(), pager.codec(), before.meta().rootTag(), !catalogRetained || maintenance || before.meta().root() == migrateHint() || remote != null && (!plan.writes.isEmpty() || !plan.remaps.equals(before.meta().remaps())));
        List<ManifestEncoder.Write> body = new ArrayList<>(plan.writes); body.addAll(catalog.writes());
        // Logical no-ops were rejected by snapshot identity above. A metadata
        // write such as repeated ANALYZE still publishes a new generation when
        // its catalog bytes happen to match and the catalog chain is retained.
        Set<Long> retired = new HashSet<>(plan.retired); retired.addAll(catalog.retiredPages());
        long live = before.meta().livePages() + plan.writes.size() - plan.retired.size() + catalog.writes().size() - catalog.retiredPages().size();
        if (live < 0) throw new CorruptStorageException("negative planned live-page count");
        boolean repair = catalog.writes().size() <= catalog.retiredPages().size() && next.tables().values().stream().noneMatch(table -> table.rows().hasStagedRecords() || table.indexes().stream().anyMatch(index -> index.entries().hasStagedRecords()));
        if (limit > 0 && live > before.meta().livePages() && live > limit / pageSize() && !repair) throw StorageLimit.exceeded(limit);
        FreeList oldFree = FreeList.read(pager, before);
        long leaked = before.meta().pageCount() - layout().firstBody() - oldFree.size() - before.meta().livePages()
            - oldFree.chain().length - before.meta().manifestPages() - held.size();
        boolean rebuild = remote == null && reuse && watermark == txid
            && (!reclamationKnown || leaked > ReclamationSchedule.COMPACT_MIN_PAGES
                && leaked * ReclamationSchedule.LEAK_DEN > before.meta().livePages());
        if (rebuild) retired.addAll(unlistedDead(before, oldFree));
        if (foreignWholesale && !rebuild) {
            // A foreign commit invalidates the owner's incremental wholesale inventory.
            // These pages become bounded leaks until the canonical backstop walk.
            retired.removeAll(catalog.retiredPages());
            for (var table : snapshot.tables().values()) for (var index : table.indexes())
                if (index.kind() == 2) retired.removeAll(index.physicalPages());
        }
        for (long id : oldFree.chain()) retired.add(id);
        for (long id = foreignWholesale && !rebuild ? 0 : before.meta().manifestHead(); id != 0;) {
            retired.add(id); id = PageFormat.parseBody(pager.readRaw(id)).next();
        }
        retired.addAll(held);
        Map<Long, Long> nextRetiredAt = new HashMap<>(retiredAt);
        for (long id : retired) nextRetiredAt.putIfAbsent(id, txid);
        TreeSet<Long> remaining = new TreeSet<>();
        for (long id : committer.freePages()) remaining.add(id);
        Set<Long> nextHeld = new HashSet<>();
        // Fully durable commits list new orphans immediately, but the allocator's
        // input remains the previous free list, so none can be reused this commit.
        if (remote == null) remaining.addAll(retired);
        for (var entry : committer.pendingEntries()) { if (remaining.remove(entry.page())) nextHeld.add(entry.page()); }
        // A batch-held orphan becomes reusable from the generation that first
        // lists it, not the earlier generation that orphaned it. The one-page
        // resident free residue is already listed logically and keeps its age.
        long[] previouslyFree = committer.freePages();
        for (long id : held) if (remaining.contains(id) && java.util.Arrays.binarySearch(previouslyFree, id) < 0)
            nextRetiredAt.put(id, txid);
        for (var write : body) { remaining.remove(write.page()); nextRetiredAt.remove(write.page()); }
        FinalPlan publication = finish(catalog.allocator(), remaining, body, txid, catalog.root(), catalog.rootTag(), live, plan.remaps);
        for (var write : publication.writes) nextRetiredAt.remove(write.page());
        // The canonical single-page residue remains allocatable in RAM without paying for a free-list page.
        if (publication.freeHead == 0) nextHeld.addAll(publication.remaining);
        var plannedMeta = PlaintextRecovery.parseMeta(publication.meta);
        Map<String, Engine.Table> tables = new HashMap<>();
        // Build the committed skeleton while the writer pin still protects the base.
        // After a client's commit_end the owner may reclaim immediately, so adoption
        // must perform no further page reads under the released writer generation.
        try (Pager plannedPager = plannedPager(publication.writes, plannedMeta.pageCount())) {
        for (Engine.Table table : next.tables().values()) {
            var tree = PersistentTree.open(plannedPager, plan.roots.get(table.name()).page(), table.rows().size(), plannedMeta.pageCount(), plannedMeta.remaps(),
                new TableLeafCodec(table.types(), table.primaryKey(), pager, plannedMeta.pageCount()), plan.roots.get(table.name()).tag()).retainCommittedRoot(table.rows()).publishedOn(pager);
            List<Engine.Index> indexes = new ArrayList<>();
            for (Engine.Index index : table.indexes()) {
                var pointer = plan.roots.get(index.name());
                if (index.kind() == 2) {
                    var loaded = GistTree.open(plannedPager, pointer, plannedMeta.pageCount(), index.keys().stream().map(Engine.IndexKey::type).toList());
                    indexes.add(new Engine.Index(index.name(), index.keys(), index.unique(), index.constraint(), index.predicate(), 2, loaded.entries(), index.timezoneDependencies(), loaded.tree(), index.hostDependencies()));
                } else indexes.add(index.changed(PersistentTree.openIndex(plannedPager, pointer.page(), plannedMeta.pageCount(), plannedMeta.remaps(), TableLeafCodec.index(), pointer.tag()).publishedOn(pager)));
            }
            tables.put(table.name(), table.changed(tree, table.nextRowId()).withIndexes(indexes).withStatistics(table.statistics()));
        }
        }
        var published = committer.commit(publication.meta, publication.writes,
            id -> remote != null ? remote.reuse() : Long.compareUnsigned(retiredAt.getOrDefault(id, 0L), safeWatermark) <= 0, remote != null || options.synchronous());
        batchAfter(wasPending);
        committer.residentFree(publication.freeHead == 0 ? publication.remaining : Set.of());
        nextRetiredAt.keySet().removeIf(id -> id >= published.meta().pageCount());
        snapshot = new Engine.Snapshot(tables, next.types(), next.views(), next.sequences(), txid, next.collations()); retiredAt = nextRetiredAt; held = nextHeld; catalogRetained = true; foreignWholesale = false;
        if (serviceWindows != null) serviceWindows.published(published.meta().encoded(), publication.writes.stream().mapToLong(ManifestEncoder.Write::page).toArray());
        if (rebuild) reclamationKnown = true;
        return snapshot;
    }

    private Pager plannedPager(List<ManifestEncoder.Write> writes, long count) throws IOException {
        Map<Long,byte[]> staged = new HashMap<>(); for (var write : writes) staged.put(write.page(), write.bytes());
        BlockStore view = new BlockStore() {
            public byte[] readAt(long offset, int length) throws IOException {
                if (length != pageSize() || offset % pageSize() != 0 || offset < 0 || offset / pageSize() >= count) throw new IOException("invalid planned page read");
                byte[] bytes = staged.get(offset / pageSize()); return bytes != null ? bytes.clone() : pager.readRaw(offset / pageSize());
            }
            public long size() { return count * pageSize(); }
            public void writeAt(long offset, byte[] bytes) { throw new UnsupportedOperationException(); }
            public void setSize(long size) { throw new UnsupportedOperationException(); }
            public void sync() { throw new UnsupportedOperationException(); }
            public void close() { }
        };
        return new Pager(view, pageSize(), pager.cacheBytes(), false, pager.codec());
    }
    private Set<Long> unlistedDead(PlaintextRecovery.Selected before, FreeList free) throws IOException {
        Set<Long> reachable = new HashSet<>();
        try (var catalog = CatalogChain.cursor(pager, before.meta().root(), before.meta().pageCount(), before.meta().rootTag())) {
            while (catalog.next() != null) { }
            reachable.addAll(catalog.visitedPages());
        }
        for (Engine.Table table : snapshot.tables().values()) {
            for (long page : table.rows().physicalPages()) if (!reachable.add(page)) throw new CorruptStorageException("catalog trees overlap");
            for (Engine.Index index : table.indexes()) for (long page : index.physicalPages()) if (!reachable.add(page)) throw new CorruptStorageException("catalog index trees overlap");
        }
        if (reachable.size() != before.meta().livePages()) throw new CorruptStorageException("stored live-page count differs from reachability");
        for (long page : free.chain()) reachable.add(page);
        // The previous committed manifest is stabilized before body writes. Its
        // unreachable dependencies may enter this commit's new free list, but
        // cannot be allocated until a later commit. Pending-batch dependencies
        // are removed from the candidate set by the caller's held-page pass.
        Set<Long> dead = new HashSet<>();
        for (long page = layout().firstBody(); page < before.meta().pageCount(); page++) if (!reachable.contains(page) && !free.contains(page)) dead.add(page);
        return dead;
    }
    private record Plan(PageAllocator allocator, List<ManifestEncoder.Write> writes, List<PlaintextRecovery.Remap> remaps,
                        Set<Long> retired, Map<String, PagePointer> roots) { }
    private long migrateHint() {
        long top = committer.current().meta().pageCount() - 1; boolean lower = false;
        for (long page : committer.freePages()) { if (page == top) return 0; if (page < top) lower = true; }
        return lower ? top : 0;
    }
    private Plan trees(Engine.Snapshot next, PageAllocator allocator, long txid, boolean fold, PageCodec encoder) throws IOException {
        List<ManifestEncoder.Write> writes = new ArrayList<>();
        List<PlaintextRecovery.Remap> remaps = committer.current().meta().remaps();
        Set<Long> retired = new HashSet<>(); Map<String, PagePointer> roots = new HashMap<>();
        List<PersistentTree> retainedTrees = new ArrayList<>();
        for (Engine.Table table : next.tables().values()) { retainedTrees.add(table.rows()); for (Engine.Index index : table.indexes()) retainedTrees.add(index.entries()); }
        for (Engine.Table old : snapshot.tables().values()) {
            if (retainedTrees.stream().noneMatch(old.rows()::sharesTreeHistory)) retired.addAll(old.rows().physicalPages());
            for (Engine.Index index : old.indexes()) if (retainedTrees.stream().noneMatch(index.entries()::sharesTreeHistory)) retired.addAll(index.physicalPages());
        }
        remaps = remaps.stream().filter(remap -> !retired.contains(remap.parent())).toList();
        for (Engine.Table table : CatalogCodec.ordered(next)) {
            var tree = table.rows().prepare(allocator, txid, remaps, fold, encoder, migrateHint());
            allocator = tree.allocator(); writes.addAll(tree.writes()); remaps = tree.remaps(); retired.addAll(tree.retiredPages()); roots.put(table.name(), new PagePointer(tree.root(), tree.rootTag()));
            for (Engine.Index index : CatalogCodec.orderedIndexes(table)) {
                if (index.kind() == 2) {
                    GistTree gist = index.gist() == null ? GistTree.build(index.keys().stream().map(Engine.IndexKey::type).toList(), index.entries(), null) : index.gist();
                    var prepared = gist.prepare(allocator, txid, layout(), encoder);
                    allocator = prepared.allocator(); writes.addAll(prepared.writes()); retired.addAll(prepared.retired()); roots.put(index.name(), prepared.root()); continue;
                }
                var prepared = index.entries().prepare(allocator, txid, remaps, fold, encoder, migrateHint());
                allocator = prepared.allocator(); writes.addAll(prepared.writes()); remaps = prepared.remaps(); retired.addAll(prepared.retiredPages()); roots.put(index.name(), new PagePointer(prepared.root(), prepared.rootTag()));
            }
        }
        return new Plan(allocator, writes, remaps, retired, roots);
    }
    private record FinalPlan(byte[] meta, List<ManifestEncoder.Write> writes, long freeHead, Set<Long> remaining) { }
    private FinalPlan finish(PageAllocator input, TreeSet<Long> inputRemaining, List<ManifestEncoder.Write> body,
                             long txid, long root, byte[] rootTag, long live, List<PlaintextRecovery.Remap> remaps) throws IOException {
        long high = input.highWater();
        long[] safe = input.remainingPages(); int safeCount = safe.length;
        while (remote == null && !committer.hasPending() && safeCount != 0 && safe[safeCount - 1] == high - 1) { safeCount--; high--; }
        TreeSet<Long> retained = new TreeSet<>(inputRemaining);
        retained.tailSet(high).clear();
        PageAllocator trimmed = new PageAllocator(high, java.util.Arrays.copyOf(safe, safeCount));
        int reserved = 0;
        while (true) {
            PageAllocator allocator = trimmed.copy(); TreeSet<Long> remaining = new TreeSet<>(retained);
            long[] manifests = new long[reserved];
            for (int i = 0; i < manifests.length; i++) { manifests[i] = allocator.allocate(); remaining.remove(manifests[i]); }
            List<Long> freeIds = new ArrayList<>(); int per = layout().chainCapacity() / 4;
            if (remaining.size() >= 2) while ((remaining.size() + per - 1L) / per > freeIds.size()) {
                long id = allocator.allocate(); remaining.remove(id); freeIds.add(id);
            }
            int required = Math.max(0, committer.pendingEntries().size() + body.size() + freeIds.size() - layout().inlineManifestCapacity());
            required = (required + layout().overflowManifestCapacity() - 1) / layout().overflowManifestCapacity();
            List<ManifestEncoder.Write> writes = new ArrayList<>(body); Long[] free = remaining.toArray(Long[]::new);
            byte[] freeTag = new byte[layout().tagBytes()];
            List<ManifestEncoder.Write> freeWrites = new ArrayList<>();
            for (int i = freeIds.size() - 1; i >= 0; i--) {
                int start = Math.min(i * per, free.length), count = Math.min(per, free.length - start);
                byte[] page = new byte[pageSize()]; page[0] = 7; PageFormat.put16(page, 2, (int) (txid & 0xffff));
                PageFormat.put32(page, 4, count); PageFormat.put32(page, 8, i + 1 == freeIds.size() ? 0 : freeIds.get(i + 1));
                System.arraycopy(freeTag, 0, page, 16, freeTag.length);
                for (int j = 0; j < count; j++) PageFormat.put32(page, 16 + freeTag.length + j * 4, free[start + j]);
                byte[] sealed = pager.codec().sealBody(freeIds.get(i), page); freeTag = pager.codec().bodyTag(sealed); freeWrites.add(new ManifestEncoder.Write(freeIds.get(i), sealed));
            }
            // Frozen peers seal each candidate free chain before increasing the manifest
            // reservation. Discarded candidates still consume their fresh key-stream nonces.
            if (required > reserved) { reserved = required; continue; }
            if (encryption == null) java.util.Collections.reverse(freeWrites); writes.addAll(freeWrites);
            List<ManifestEncoder.Entry> entries = new ArrayList<>(committer.pendingEntries());
            for (var write : writes) entries.add(new ManifestEncoder.Entry(write.page(), manifestCrc(write.bytes())));
            entries.sort(java.util.Comparator.comparingLong(ManifestEncoder.Entry::page));
            long freeHead = freeIds.isEmpty() ? 0 : freeIds.getFirst();
            var encoded = ManifestEncoder.encode(layout(), txid, root, rootTag, allocator.highWater(), freeHead, freeTag, live, entries, manifests, remaps);
            writes.addAll(encoded.overflow());
            byte[] meta = encryption == null ? encoded.meta() : encryption.sealMeta(committer.current().floorSlot() ^ 1, encoded.meta(), encoded.overflow());
            return new FinalPlan(meta, writes, freeHead, remaining);
        }
    }
    private long manifestCrc(byte[] bytes) { if (encryption == null) return PageFormat.bodyCrc(bytes); var crc = new java.util.zip.CRC32(); crc.update(bytes); return crc.getValue(); }
    private static final PageCodec DRAFT_CODEC = new PageCodec() {
        public boolean encrypted() { return true; }
        public byte[] sealBody(long index, byte[] plain) { return plain.clone(); }
        public byte[] openBody(long index, byte[] stored, byte[] expectedTag) { throw new UnsupportedOperationException("draft pages are never readable or persisted"); }
    };
    synchronized void abortHost() throws IOException {
        if (closed) return; closed = true;
        pager.poison(); pager.close();
    }
    @Override public synchronized void close() throws IOException {
        if (closed) return; closed = true;
        try { try { if (remote == null) committer.flush(true); } finally { pager.close(); } }
        finally { if (lock != null) lock.close(); else if (clientClaim != null) clientClaim.close(); }
    }
}
