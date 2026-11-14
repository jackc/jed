package org.jed.storage;

import java.io.IOException;
import java.nio.ReadOnlyBufferException;
import java.nio.file.Path;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicInteger;

public final class PagerTest {
    public static void main(String[] args) throws Exception {
        try (MemoryBlockStore memory = new MemoryBlockStore(256);
             Pager pager = new Pager(memory, 256, 1, false)) {
            pager.reserve(3);
            check(pager.allocatedPages() == 64, "16 KiB growth floor");
            pager.writePage(2, body(1));
            Pager.Page first = pager.readBody(2);
            check(first == pager.readBody(2), "cache sharing");
            try { first.buffer().put(16, (byte) 99); throw new AssertionError("mutable page view"); }
            catch (ReadOnlyBufferException expected) { }
            pager.writePage(2, body(2));
            check(first.buffer().get(16) == 1 && pager.readBody(2).buffer().get(16) == 2, "immutable retained read");
            pager.writePage(3, body(3));
            pager.readBody(3);
            check(pager.residentPages() == 1, "bounded cache floor");
        }
        CountDownLatch entered = new CountDownLatch(1), resume = new CountDownLatch(1);
        AtomicInteger reads = new AtomicInteger();
        try (MemoryBlockStore memory = new MemoryBlockStore(256)) {
            memory.setSize(1024);
            memory.writeAt(512, body(1));
            BlockStore controlled = new DelegatingStore(memory) {
                @Override public byte[] readAt(long offset, int length) throws IOException {
                    byte[] result = super.readAt(offset, length);
                    if (reads.incrementAndGet() == 1) {
                        entered.countDown();
                        try { resume.await(); }
                        catch (InterruptedException e) { Thread.currentThread().interrupt(); throw new IOException(e); }
                    }
                    return result;
                }
            };
            try (Pager pager = new Pager(controlled, 256, 1024, false);
                 var executor = Executors.newVirtualThreadPerTaskExecutor()) {
                var old = executor.submit(() -> pager.readBody(2));
                entered.await();
                pager.writePage(2, body(2));
                resume.countDown();
                check(old.get().buffer().get(16) == 1, "in-flight waiter keeps old bytes");
                check(pager.readBody(2).buffer().get(16) == 2, "detached load cannot reinstall stale page");
            }
        }
        try (MemoryBlockStore memory = new MemoryBlockStore(256)) {
            memory.setSize(1024);
            BlockStore failing = new DelegatingStore(memory) {
                @Override public void sync() throws IOException { throw new IOException("injected barrier failure"); }
            };
            try (Pager pager = new Pager(failing, 256, 256, false)) {
                try { pager.sync(); throw new AssertionError("missing failure"); }
                catch (IOException expected) { check(pager.poisoned(), "barrier failure poisons writer"); }
                try { pager.writePage(2, body(3)); throw new AssertionError("poisoned writer reused"); }
                catch (IOException expected) { check(memory.readAt(512, 1)[0] == 0, "poison fails before write"); }
            }
        }
        Path fixtures = Path.of(args.length == 0 ? "spec/fileformat/fixtures" : args[0]);
        try (MemoryBlockStore memory = new MemoryBlockStore(256)) {
            memory.setSize(2048); memory.writeAt(512, body(1)); memory.writeAt(768, body(2));
            AtomicInteger batches = new AtomicInteger();
            BlockStore batched = new DelegatingStore(memory) {
                public int maxReadPages() { return 64; }
                public java.util.List<byte[]> readPages(long[] pages, int pageSize) throws IOException {
                    check(pages.length == 1, "prefetch exceeds one quarter of four-page pool"); batches.incrementAndGet();
                    return java.util.List.of(readAt(pages[0] * pageSize, pageSize));
                }
            };
            try (Pager pager = new Pager(batched, 256, 1024, false)) {
                var hints = java.util.List.of(new PagePointer(2, new byte[0]), new PagePointer(3, new byte[0]));
                pager.prefetch(hints); check(pager.residentPages() == 1 && batches.get() == 1, "bounded prefetch populates ordinary cache");
                pager.prefetch(hints); check(pager.residentPages() == 2 && batches.get() == 2, "prefetch skips cached pages");
                pager.prefetch(hints); check(batches.get() == 2, "fully cached hint issued I/O");
                pager.invalidateAll(); memory.writeAt(512, new byte[256]); pager.prefetch(hints);
                try { pager.readBody(2); throw new AssertionError("bad prefetched page accepted"); } catch (CorruptStorageException expected) { }
            }
        }
        try (BlockStore store = FileBlockStore.open(fixtures.resolve("cow_invalid_free_list.jed"), false)) {
            var selected = PlaintextRecovery.recover(store);
            try (Pager pager = new Pager(store, 256, 1024, false)) {
                check(selected.meta().txid() == 2, "complete manifest selects new generation");
                try { FreeList.read(pager, selected); throw new AssertionError("invalid free list accepted"); }
                catch (CorruptStorageException expected) { }
            }
        }
        System.out.println("PagerTest passed");
    }

    private static byte[] body(int value) {
        byte[] bytes = new byte[256];
        bytes[0] = 4;
        bytes[16] = (byte) value;
        PageFormat.put32(bytes, 12, PageFormat.bodyCrc(bytes));
        return bytes;
    }

    private static class DelegatingStore implements BlockStore {
        final BlockStore source;
        DelegatingStore(BlockStore source) { this.source = source; }
        public byte[] readAt(long offset, int length) throws IOException { return source.readAt(offset, length); }
        public void writeAt(long offset, byte[] bytes) throws IOException { source.writeAt(offset, bytes); }
        public void sync() throws IOException { source.sync(); }
        public long size() throws IOException { return source.size(); }
        public void setSize(long length) throws IOException { source.setSize(length); }
        public void close() { }
    }

    private static void check(boolean condition, String message) {
        if (!condition) throw new AssertionError(message);
    }
}
