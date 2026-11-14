package org.jed.storage;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.Random;
import java.util.concurrent.Executors;

/** Host-only contract tests: SQL corpus cannot express positioned device operations. */
public final class BlockStoreTest {
    public static void main(String[] args) throws Exception {
        Path directory = Files.createTempDirectory("jed-java-blockstore-");
        Path file = directory.resolve("device");
        try {
            try (BlockStore memory = new MemoryBlockStore(256);
                 BlockStore disk = FileBlockStore.createNew(file)) {
                // Exercise cross-block writes, holes, and shrink/grow zeroing against
                // a simple independent byte-array model, identically on both hosts.
                Random random = new Random(724608);
                byte[] model = new byte[0];
                for (int iteration = 0; iteration < 160; iteration++) {
                    if (iteration % 3 == 0) {
                        int length = random.nextInt(10000);
                        model = Arrays.copyOf(model, length);
                        memory.setSize(length);
                        disk.setSize(length);
                    } else {
                        int offset = random.nextInt(10000);
                        byte[] bytes = new byte[1 + random.nextInt(700)];
                        random.nextBytes(bytes);
                        if (offset + bytes.length > model.length) model = Arrays.copyOf(model, offset + bytes.length);
                        System.arraycopy(bytes, 0, model, offset, bytes.length);
                        memory.writeAt(offset, bytes);
                        disk.writeAt(offset, bytes);
                    }
                    check(memory.size() == model.length && disk.size() == model.length, "size");
                    check(Arrays.equals(memory.readAt(0, model.length), model), "memory bytes");
                    check(Arrays.equals(disk.readAt(0, model.length), model), "file bytes");
                }
                for (BlockStore store : new BlockStore[] {memory, disk}) {
                    long previous = store.size();
                    store.writeAt(previous + 100, new byte[0]);
                    check(store.size() == previous, "empty write extends file");
                    expectIo(() -> store.readAt(previous, 1));
                    expectIo(() -> store.readAt(-1, 1));
                    expectIo(() -> store.writeAt(Long.MAX_VALUE, new byte[1]));
                    expectIo(() -> store.setSize(-1));
                    store.sync();
                }
                byte[] expected = model;
                try (var executor = Executors.newVirtualThreadPerTaskExecutor()) {
                    var pending = new java.util.ArrayList<java.util.concurrent.Future<?>>();
                    for (int reader = 0; reader < 16; reader++) {
                        pending.add(executor.submit(() -> {
                            try {
                                for (int i = 0; i < 50; i++) {
                                    check(Arrays.equals(disk.readAt(0, expected.length), expected), "concurrent read");
                                }
                            } catch (IOException e) { throw new RuntimeException(e); }
                        }));
                    }
                    for (var result : pending) result.get();
                }
            }
            try (BlockStore reopened = FileBlockStore.open(file, false)) {
                check(reopened.size() > 0, "reopened bytes");
            }
            MemoryBlockStore sparse = new MemoryBlockStore(256);
            sparse.setSize(1L << 35);
            sparse.writeAt((1L << 35) - 1, new byte[] {27});
            check(sparse.readAt((1L << 35) - 1, 1)[0] == 27, "64-bit offset");
            sparse.close();
            expectIo(() -> sparse.readAt(0, 0));
            System.out.println("BlockStoreTest passed");
        } finally {
            Files.deleteIfExists(file);
            Files.delete(directory);
        }
    }

    private interface IoAction { void run() throws IOException; }
    private static void expectIo(IoAction action) throws IOException {
        try { action.run(); }
        catch (IOException expected) { return; }
        throw new AssertionError("expected IOException");
    }
    private static void check(boolean condition, String message) {
        if (!condition) throw new AssertionError(message);
    }
}
