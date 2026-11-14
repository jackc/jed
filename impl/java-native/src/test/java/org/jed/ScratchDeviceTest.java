package org.jed;

import java.io.IOException;
import java.util.Arrays;
import org.jed.storage.MemoryBlockStore;

/** Scratch authenticity, sparse zeros, replay rejection and lifetime counters are host invariants. */
public final class ScratchDeviceTest {
    public static void main(String[] args) throws Exception {
        try (var raw = new MemoryBlockStore(); var device = new ScratchDevice(raw, CryptoTest.seeded(33))) {
            byte[] content = new byte[15_000]; new java.util.Random(19).nextBytes(content); device.writeAt(0, content);
            check(Arrays.equals(device.readAt(0, content.length), content), "multi-block scratch round trip");
            byte[] old = raw.readAt(0, ScratchDevice.STORED_BYTES);
            device.writeAt(0, new byte[]{1, 2, 3}); byte[] latest = raw.readAt(0, ScratchDevice.STORED_BYTES);
            raw.writeAt(0, old); rejects(() -> device.readAt(0, 3), "overwritten block replay"); raw.writeAt(0, latest);
            device.setSize(2); device.setSize(10_000);
            check(Arrays.equals(device.readAt(0, 5), new byte[]{1, 2, 0, 0, 0}), "truncation tail zeros");
            check(Arrays.equals(device.readAt(4096, 4096), new byte[4096]), "truncated block became visible on growth");
            device.writeAt(0, new byte[]{4, 5}); latest = raw.readAt(0, ScratchDevice.STORED_BYTES);
            raw.writeAt(0, old); rejects(() -> device.readAt(0, 2), "truncation reset write counter"); raw.writeAt(0, latest);
            device.writeAt(4096, new byte[]{9}); byte[] second = raw.readAt(ScratchDevice.STORED_BYTES, ScratchDevice.STORED_BYTES);
            raw.writeAt(ScratchDevice.STORED_BYTES, latest); rejects(() -> device.readAt(4096, 1), "block ordinal swap"); raw.writeAt(ScratchDevice.STORED_BYTES, second);
            second[14] ^= 1; raw.writeAt(ScratchDevice.STORED_BYTES, second); rejects(() -> device.readAt(4096, 1), "ciphertext alteration");
        }
        try (var rawA = new MemoryBlockStore(); var a = new ScratchDevice(rawA, CryptoTest.seeded(41));
             var rawB = new MemoryBlockStore(); var b = new ScratchDevice(rawB, CryptoTest.seeded(42))) {
            a.writeAt(0, new byte[]{1}); b.writeAt(0, new byte[]{1}); rawB.writeAt(0, rawA.readAt(0, ScratchDevice.STORED_BYTES));
            rejects(() -> b.readAt(0, 1), "cross-file block transplant");
        }
        try (var raw = new MemoryBlockStore(); var device = new ScratchDevice(raw, CryptoTest.seeded(55))) {
            device.setSize(1_000_000); device.setSize(500_000);
            check(raw.size() == 0 && device.readAt(499_999, 1)[0] == 0, "sparse resize allocated scratch storage");
        }
        System.out.println("ScratchDeviceTest: passed authenticated blocks, overwrite/truncate replay, ordinal/file swaps and sparse zeros");
    }
    private interface Read { byte[] run() throws IOException; }
    private static void rejects(Read read, String message) throws IOException { try { read.run(); throw new AssertionError(message); } catch (IOException expected) { } }
    private static void check(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
}
