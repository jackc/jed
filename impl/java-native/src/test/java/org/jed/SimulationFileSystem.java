package org.jed;

import java.io.IOException;
import java.nio.file.Path;
import java.util.*;
import org.jed.storage.BlockStore;

/** Content-keyed crash host from simulation-testing.md6; production code owns recovery. */
final class SimulationFileSystem implements FileHost {
    record Fault(String kind, int probability, int tear, int nth, String operation) { }
    static final class Crash extends IOException { private static final long serialVersionUID = 1L; Crash() { super("simulated process ended"); } }
    private record Pending(byte[] bytes, boolean issued) { }
    private static final class Inode {
        final String origin;
        byte[] durable = new byte[0], cache = new byte[0];
        final TreeMap<Long,List<Pending>> pending = new TreeMap<>();
        final TreeMap<Long,List<byte[]>> archive = new TreeMap<>();
        Inode(String origin) { this.origin = origin; }
    }
    private record Change(String kind, String from, String to, Inode inode) { }
    final long seed;
    final int pageSize, sector;
    private Map<String,Inode> directory = new TreeMap<>(), durableDirectory = new TreeMap<>();
    private final List<Inode> inodes = new ArrayList<>();
    private final List<Change> changes = new ArrayList<>();
    final List<String> operations = new ArrayList<>();
    final Set<String> durableBarriers = new TreeSet<>();
    final Map<String,Set<Long>> written = new TreeMap<>();
    long step, generation;
    private int barriers, syncs;
    private long spillOperations;
    private Fault fault;
    boolean dead, oracleReads;
    private boolean barrierCrash;
    String fired = "none";
    SimulationFileSystem(long seed, int pageSize, int sector) { this.seed = seed; this.pageSize = pageSize; this.sector = sector; }
    ScratchHost scratchHost() { return (directory, unlink) -> {
        spillOperation(); long opened = generation;
        var memory = new org.jed.storage.MemoryBlockStore();
        return new BlockStore() {
            private void live() throws IOException { check(); if (opened != generation) throw new Crash(); }
            public byte[] readAt(long offset, int length) throws IOException { live(); spillOperation(); return memory.readAt(offset, length); }
            public void writeAt(long offset, byte[] bytes) throws IOException { live(); spillOperation(); memory.writeAt(offset, bytes); }
            public long size() throws IOException { live(); return memory.size(); }
            public void setSize(long length) throws IOException { live(); memory.setSize(length); }
            public void sync() throws IOException { live(); }
            public void close() { memory.close(); }
        };
    }; }
    private void spillOperation() throws IOException {
        check(); long ordinal = spillOperations++;
        operations.add("spill " + step + " " + ordinal);
        if (!oracleReads && fault != null && fault.kind.equals("spill_error")
            && Long.remainderUnsigned(decide(seed, 11, step, ordinal, 0), 1000) < fault.probability) {
            fired = "spill"; throw new IOException("simulated scratch I/O error");
        }
    }
    static long decide(long seed, long lane, long step, long offset, long version) {
        for (long word : new long[]{lane, step, offset, version}) { seed = (seed ^ word) + 0x9e3779b97f4a7c15L; seed = (seed ^ (seed >>> 30)) * 0xbf58476d1ce4e5b9L; seed = (seed ^ (seed >>> 27)) * 0x94d049bb133111ebL; seed ^= seed >>> 31; }
        return seed;
    }
    private long decision(Inode inode, long lane, long offset, long version) {
        if (!inode.origin.equals("sim.jed")) { int hash = 0x811c9dc5; for (byte b : SqlText.encode(inode.origin)) hash = (hash ^ (b & 255)) * 0x01000193; offset += (Integer.toUnsignedLong(hash) + 1) << 40; }
        return decide(seed, lane, step, offset, version);
    }
    private boolean chance(Inode inode, long lane, long offset, long version, int permille) { return Long.remainderUnsigned(decision(inode, lane, offset, version), 1000) < permille; }
    void begin(long step, Fault fault) throws IOException {
        check(); this.step = step; this.fault = fault; barriers = syncs = 0; spillOperations = 0; fired = "none"; durableBarriers.clear(); written.clear();
        for (Inode inode : inodes) for (var entry : inode.pending.entrySet()) entry.setValue(entry.getValue().stream().map(p -> new Pending(p.bytes, true)).collect(java.util.stream.Collectors.toCollection(ArrayList::new)));
    }
    void restart() { if (barrierCrash) finishFault(); dead = false; barrierCrash = false; fault = null; fired = "none"; barriers = syncs = 0; }
    void finishFault() {
        if (fault == null || dead && !barrierCrash) return;
        barrierCrash = false; if (fired.equals("none")) fired = "end";
        if (fault.kind.equals("power_loss")) powerLoss(fault.probability, fault.tear); else processCrash();
    }
    void crash(boolean power, int survive, int tear) throws IOException { check(); if (power) powerLoss(survive, tear); else processCrash(); }
    byte[] image(String path) throws IOException { return find(path).cache.clone(); }
    void install(String path, byte[] image) { Inode inode = new Inode(path); inode.cache = image.clone(); inode.durable = image.clone(); inodes.add(inode); directory.put(path, inode); durableDirectory.put(path, inode); }
    record Replacement(long page, byte[] original) { }
    List<Replacement> corrupt(String path, int flips, long pageCount, boolean reseal) throws IOException {
        Inode inode = find(path); var touched = new TreeMap<Long, Replacement>();
        if (pageCount <= 2) return List.of();
        for (int ordinal = 0; ordinal < flips; ordinal++) {
            long page = 2 + Long.remainderUnsigned(decide(seed, 12, step, ordinal, 0), pageCount - 2);
            long bit = Long.remainderUnsigned(decide(seed, 13, step, ordinal, 0), pageSize * 8L), start = page * pageSize;
            if (start + pageSize > inode.durable.length || start + pageSize > inode.cache.length
                || inode.pending.entrySet().stream().anyMatch(entry -> entry.getKey() < start + pageSize && entry.getKey() + entry.getValue().getLast().bytes.length > start)) continue;
            touched.computeIfAbsent(page, ignored -> new Replacement(page, Arrays.copyOfRange(inode.durable, (int)start, (int)start + pageSize)));
            inode.durable[(int)(start + bit / 8)] ^= (byte)(1 << (bit % 8));
        }
        for (long page : touched.keySet()) {
            int start = Math.toIntExact(page * pageSize); byte[] bytes = Arrays.copyOfRange(inode.durable, start, start + pageSize);
            if (reseal) org.jed.storage.PageFormat.put32(bytes, 12, org.jed.storage.PageFormat.bodyCrc(bytes));
            System.arraycopy(bytes, 0, inode.durable, start, pageSize); System.arraycopy(bytes, 0, inode.cache, start, pageSize);
        }
        return List.copyOf(touched.values());
    }
    void repair(String path, List<Replacement> pages) throws IOException { Inode inode = find(path); for (var page : pages) {
        int start = Math.toIntExact(page.page * pageSize); System.arraycopy(page.original, 0, inode.cache, start, pageSize); System.arraycopy(page.original, 0, inode.durable, start, pageSize);
    } }
    List<byte[]> replayVersions(String path, long page) throws IOException {
        Inode inode = find(path); long start = page * pageSize;
        if (start + pageSize > inode.cache.length || start + pageSize > inode.durable.length || inode.pending.entrySet().stream().anyMatch(entry -> entry.getKey() < start + pageSize && entry.getKey() + entry.getValue().getLast().bytes.length > start)) return List.of();
        byte[] current = Arrays.copyOfRange(inode.cache, (int)start, (int)start + pageSize);
        return inode.archive.getOrDefault(page, List.of()).stream().filter(bytes -> !Arrays.equals(bytes, current)).map(byte[]::clone).sorted(Arrays::compareUnsigned).toList();
    }
    Replacement replace(String path, long page, byte[] bytes) throws IOException {
        Inode inode = find(path); int start = Math.toIntExact(page * pageSize);
        Replacement result = new Replacement(page, Arrays.copyOfRange(inode.durable, start, start + pageSize));
        System.arraycopy(bytes, 0, inode.durable, start, pageSize); System.arraycopy(bytes, 0, inode.cache, start, pageSize); return result;
    }
    void restore(String from, String to) throws IOException {
        Inode source = find(from), prior = find(to), restored = new Inode(to); restored.cache = source.cache.clone(); restored.durable = source.cache.clone();
        prior.archive.forEach((page, versions) -> versions.forEach(bytes -> archive(restored, page, bytes)));
        source.archive.forEach((page, versions) -> versions.forEach(bytes -> archive(restored, page, bytes)));
        inodes.add(restored); directory.put(to, restored); durableDirectory = new TreeMap<>(directory); changes.clear();
    }
    boolean entryDurable(String path) { return directory.get(path) != null && directory.get(path) == durableDirectory.get(path); }
    boolean contentDurable(String path) throws IOException { Inode inode = find(path); return inode.pending.isEmpty() && Arrays.equals(inode.cache, inode.durable); }
    private static String name(Path path) { return path.getFileName().toString(); }
    private Inode find(String path) throws IOException { Inode result = directory.get(path); if (result == null) throw new java.nio.file.NoSuchFileException(path); return result; }
    private void check() throws IOException { if (dead) throw new Crash(); }
    public boolean exists(Path path) throws IOException { check(); return directory.containsKey(name(path)); }
    public BlockStore open(Path path, boolean writable) throws IOException { check(); return new Device(find(name(path)), name(path), writable); }
    public BlockStore create(Path path) throws IOException {
        check(); String name = name(path); if (directory.containsKey(name)) throw new java.nio.file.FileAlreadyExistsException(name);
        return rewrite(path);
    }
    public BlockStore rewrite(Path path) throws IOException {
        check(); String name = name(path);
        Inode inode = new Inode(name); inodes.add(inode); directory.put(name, inode); changes.add(new Change("create", name, null, inode)); operations.add("create " + name); return new Device(inode, name, true);
    }
    public void delete(Path path) throws IOException { check(); String name = name(path); if (directory.remove(name) != null) { changes.add(new Change("delete", name, null, null)); operations.add("delete " + name); } }
    public void rename(Path source, Path target, boolean replace) throws IOException {
        check(); String from = name(source), to = name(target); Inode inode = find(from), previous = directory.get(to);
        if (previous != null && !replace) throw new java.nio.file.FileAlreadyExistsException(to);
        if (previous != null) { var own = new TreeMap<>(inode.archive); inode.archive.clear(); previous.archive.forEach((page, versions) -> versions.forEach(bytes -> archive(inode, page, bytes))); own.forEach((page, versions) -> versions.forEach(bytes -> archive(inode, page, bytes))); }
        directory.remove(from); directory.put(to, inode); changes.add(new Change("rename", from, to, inode)); operations.add("rename " + from + " " + to);
    }
    public void permissions(Path source, Path target) throws IOException { check(); find(name(source)); find(name(target)); }
    public void syncDirectory(Path path) throws IOException {
        check(); if (changes.isEmpty()) return; operations.add("sync directory");
        try { barrier(); } catch (Crash stopped) { return; }
        durableDirectory = new TreeMap<>(directory); changes.clear();
    }
    private void barrier() throws IOException {
        barriers++;
        if (fault != null && fired.equals("none") && barriers == fault.nth && (fault.kind.equals("power_loss") || fault.kind.equals("process_crash"))) { fired = "barrier"; barrierCrash = true; die(); throw new Crash(); }
    }
    private void sync(Inode inode) throws IOException {
        syncs++; barrier();
        if (fault != null && fault.kind.equals("io_error") && fault.operation.equals("sync") && fired.equals("none") && syncs == fault.nth) {
            fired = "sync";
            for (var entry : inode.pending.entrySet()) { long offset = entry.getKey(); var versions = entry.getValue(); byte[] newest = versions.getLast().bytes;
                if ((decision(inode, 7, offset, versions.size() - 1) & 1) == 0) inode.durable = copy(inode.durable, offset, newest);
                else for (int i = 0; i < newest.length; i++) inode.cache[(int)offset + i] = offset + i < inode.durable.length ? inode.durable[(int)offset + i] : 0;
            }
            inode.pending.clear(); throw new IOException("simulated sync failure");
        }
        commit(inode);
    }
    private void commit(Inode inode) {
        for (var entry : inode.pending.entrySet()) if (entry.getKey() % pageSize == 0) {
            byte[] bytes = entry.getValue().getLast().bytes;
            for (int start = 0; start + pageSize <= bytes.length; start += pageSize) { long page = (entry.getKey() + start) / pageSize; if (page >= 4) archive(inode, page, Arrays.copyOfRange(bytes, start, start + pageSize)); }
        }
        inode.durable = inode.cache.clone(); inode.pending.clear();
        if (changes.isEmpty()) directory.forEach((name, node) -> { if (node == inode && durableDirectory.get(name) == inode) durableBarriers.add(name); });
    }
    private static void archive(Inode inode, long page, byte[] bytes) { var versions = inode.archive.computeIfAbsent(page, ignored -> new ArrayList<>()); for (byte[] old : versions) if (Arrays.equals(old, bytes)) return; versions.add(bytes.clone()); if (versions.size() > 32) versions.removeFirst(); }
    private void powerLoss(int survive, int tear) {
        for (Inode inode : inodes) {
            for (var entry : inode.pending.entrySet()) { long offset = entry.getKey(); var versions = entry.getValue(); if (!chance(inode, 1, offset, 0, survive)) continue;
                int selected = (int)Long.remainderUnsigned(decision(inode, 2, offset, 0), versions.size()); byte[] bytes = versions.get(selected).bytes.clone();
                if (chance(inode, 3, offset, selected, tear)) for (int local = 0; local < bytes.length; local += sector) if ((decision(inode, 4, offset + local, selected) & 1) == 0)
                    for (int j = local; j < Math.min(bytes.length, local + sector); j++) bytes[j] = offset + j < inode.durable.length ? inode.durable[(int)offset + j] : 0;
                inode.durable = copy(inode.durable, offset, bytes);
            }
            inode.pending.clear(); inode.cache = inode.durable.clone();
        }
        int keep = (int)Long.remainderUnsigned(decide(seed, 10, step, 0, 0), changes.size() + 1); var recovered = new TreeMap<>(durableDirectory);
        for (int i = 0; i < keep; i++) { Change change = changes.get(i); if (change.kind.equals("create")) recovered.put(change.from, change.inode); else if (change.kind.equals("delete")) recovered.remove(change.from); else { recovered.remove(change.from); recovered.put(change.to, change.inode); } }
        directory = recovered; durableDirectory = new TreeMap<>(recovered); changes.clear(); die();
    }
    private void processCrash() {
        for (Inode inode : inodes) { inode.cache = inode.durable.clone(); var iterator = inode.pending.entrySet().iterator(); while (iterator.hasNext()) { var entry = iterator.next(); entry.getValue().removeIf(value -> !value.issued); if (entry.getValue().isEmpty()) iterator.remove(); else inode.cache = copy(inode.cache, entry.getKey(), entry.getValue().getLast().bytes); } }
        die();
    }
    private void die() { dead = true; generation++; }
    private static byte[] copy(byte[] target, long offset, byte[] bytes) { int end = Math.toIntExact(Math.addExact(offset, bytes.length)); if (end > target.length) target = Arrays.copyOf(target, end); System.arraycopy(bytes, 0, target, Math.toIntExact(offset), bytes.length); return target; }
    private final class Device implements BlockStore {
        final Inode inode; final String initial; final long opened = generation; final boolean writable; boolean closed;
        Device(Inode inode, String initial, boolean writable) { this.inode = inode; this.initial = initial; this.writable = writable; }
        private void valid(boolean write) throws IOException { check(); if (opened != generation) throw new Crash(); if (closed) throw new IOException("simulated device closed"); if (write && !writable) throw new IOException("read-only simulated device"); }
        private String path() { for (var entry : directory.entrySet()) if (entry.getValue() == inode) return entry.getKey(); return initial; }
        public byte[] readAt(long offset, int length) throws IOException {
            valid(false); long end = BlockStore.checkedEnd(offset, length); if (end > inode.cache.length) throw new java.io.EOFException("short simulated read");
            if (!oracleReads && fault != null && fault.kind.equals("read_error") && chance(inode, 9, offset, 0, fault.probability)) { fired = "read"; throw new IOException("simulated read failure"); }
            return Arrays.copyOfRange(inode.cache, (int)offset, (int)end);
        }
        public void writeAt(long offset, byte[] bytes) throws IOException {
            valid(true); BlockStore.checkedEnd(offset, bytes.length); var versions = inode.pending.computeIfAbsent(offset, ignored -> new ArrayList<>());
            if (fault != null && fault.kind.equals("io_error") && fault.operation.equals("write") && fired.equals("none") && chance(inode, 5, offset, versions.size(), fault.probability)) { fired = "write"; if (versions.isEmpty()) inode.pending.remove(offset); throw new IOException("simulated write failure"); }
            boolean issued = fault == null || !fault.kind.equals("process_crash") || chance(inode, 6, offset, versions.size(), fault.probability);
            versions.add(new Pending(bytes.clone(), issued)); inode.cache = copy(inode.cache, offset, bytes);
            var pages = written.computeIfAbsent(path(), ignored -> new TreeSet<>()); for (long page = offset / pageSize; bytes.length != 0 && page <= (offset + bytes.length - 1) / pageSize; page++) pages.add(page);
            operations.add("write " + path() + " " + offset + " " + HexFormat.of().formatHex(bytes));
        }
        public long size() throws IOException { valid(false); return inode.cache.length; }
        public void setSize(long length) throws IOException {
            valid(true); int size = Math.toIntExact(length), old = inode.cache.length;
            if (size > old && fault != null && fault.kind.equals("enospc") && fired.equals("none")) { fired = "enospc"; throw new IOException("simulated disk full"); }
            inode.cache = Arrays.copyOf(inode.cache, size);
            if (size > old) commit(inode);
            else if (size < old) { inode.durable = Arrays.copyOf(inode.durable, Math.min(size, inode.durable.length)); var iterator = inode.pending.entrySet().iterator(); while (iterator.hasNext()) { var entry = iterator.next(); if (entry.getKey() >= size) iterator.remove(); else for (int i = 0; i < entry.getValue().size(); i++) { Pending value = entry.getValue().get(i); if (entry.getKey() + value.bytes.length > size) entry.getValue().set(i, new Pending(Arrays.copyOf(value.bytes, (int)(size - entry.getKey())), value.issued)); } } }
            operations.add("size " + path() + " " + length);
        }
        public void sync() throws IOException { valid(true); operations.add("sync " + path()); SimulationFileSystem.this.sync(inode); }
        public void close() { closed = true; }
    }
}
