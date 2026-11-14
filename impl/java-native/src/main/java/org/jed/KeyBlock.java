package org.jed;

import java.security.GeneralSecurityException;
import java.util.Arrays;
import java.util.function.Consumer;
import java.util.zip.CRC32;
import org.jed.storage.PageFormat;

/** Format-v53 key-block selection, bounded derivation and credential wrapping. */
final class KeyBlock {
    record Selected(byte[] bytes, int slot, long generation, boolean repair) {
        Selected { bytes = bytes.clone(); }
        @Override public byte[] bytes() { return bytes.clone(); }
    }
    private KeyBlock() { }
    static Selected publishChange(org.jed.storage.Pager pager, Selected previous, byte[] replacement,
                                  org.jed.storage.DurableCommitter.HostBarrier directoryBarrier) throws java.io.IOException {
        if (!valid(replacement) || previous.generation() == -1L || PageFormat.u64(replacement, 8) != previous.generation() + 1)
            throw new IllegalArgumentException("invalid next key block");
        try {
            // Stabilize every credential operation, including a second change on this handle.
            // This also fixes the fault schedule shared by the independently implemented peers.
            pager.sync(); if (directoryBarrier != null) directoryBarrier.run();
            pager.writePage(previous.slot() == 2 ? 3 : 2, replacement); pager.sync();
            pager.writePage(previous.slot(), replacement); pager.sync();
            return select(replacement, replacement);
        } catch (java.io.IOException failure) { pager.poison(); throw failure; }
    }
    static Selected select(byte[] first, byte[] second) {
        boolean a = valid(first), b = valid(second);
        if (!a && !b) throw Encoding.corrupt("no valid key-block copy");
        long ga = a ? PageFormat.u64(first, 8) : 0, gb = b ? PageFormat.u64(second, 8) : 0;
        if (a && b && ga == gb && !Arrays.equals(first, second)) throw Encoding.corrupt("equal-generation key-block copies differ");
        int slot = !b || a && Long.compareUnsigned(ga, gb) >= 0 ? 2 : 3;
        return new Selected(slot == 2 ? first : second, slot, slot == 2 ? ga : gb, !a || !b || ga != gb);
    }
    static boolean valid(byte[] page) {
        if (!PageFormat.validPageSize(page.length) || page.length < 1024 || page[0] != 'J' || page[1] != 'E' || page[2] != 'D' || page[3] != 'K'
            || PageFormat.u16(page, 4) != 1 || page[6] != 1 || PageFormat.u64(page, 8) == 0
            || !PageFormat.zeros(page, 28, 32) || !PageFormat.zeros(page, 128, page.length)) return false;
        CRC32 crc = new CRC32(); crc.update(page, 0, 124);
        if (crc.getValue() != PageFormat.u32(page, 124)) return false;
        long m = PageFormat.u32(page, 16), t = PageFormat.u32(page, 20), p = PageFormat.u32(page, 24);
        return page[7] == 2 ? m == 0 && t == 0 && p == 0
            : page[7] == 1 && p >= 1 && p <= 16 && t >= 1 && t <= 16 && m >= 8 * p && m <= 1_048_576;
    }
    static byte[] unlock(Selected selected, Credential credential) {
        if (credential == null) throw new SqlException("28P02", "database credential is required");
        byte[] page = selected.bytes;
        if (credential.kind != page[7]) throw new SqlException("28P01", "invalid database credential");
        byte[] key = derive(credential, page);
        try { return Crypto.gcm(false, key, Arrays.copyOfRange(page, 64, 76), Arrays.copyOfRange(page, 76, 124), Arrays.copyOf(page, 76)); }
        catch (GeneralSecurityException failure) { throw new SqlException("28P01", "invalid database credential"); }
        finally { Arrays.fill(key, (byte) 0); }
    }
    static byte[] wrap(int pageSize, long generation, byte[] dataKey, Credential credential, Consumer<byte[]> entropy) {
        if (!PageFormat.validPageSize(pageSize) || pageSize < 1024 || generation == 0 || dataKey.length != 32) throw new IllegalArgumentException("invalid key-block input");
        java.util.Objects.requireNonNull(credential);
        byte[] page = new byte[pageSize]; page[0] = 'J'; page[1] = 'E'; page[2] = 'D'; page[3] = 'K';
        PageFormat.put16(page, 4, 1); page[6] = 1; page[7] = (byte) credential.kind; PageFormat.put64(page, 8, generation);
        if (credential.kind == 1) {
            PageFormat.put32(page, 16, credential.parameters.memoryKiB()); PageFormat.put32(page, 20, credential.parameters.iterations()); PageFormat.put32(page, 24, credential.parameters.parallelism());
        }
        byte[] salt = new byte[32], nonce = new byte[12]; entropy.accept(salt); entropy.accept(nonce);
        System.arraycopy(salt, 0, page, 32, 32); System.arraycopy(nonce, 0, page, 64, 12);
        byte[] key = derive(credential, page);
        try { System.arraycopy(Crypto.gcm(true, key, nonce, dataKey, Arrays.copyOf(page, 76)), 0, page, 76, 48); }
        catch (GeneralSecurityException failure) { throw new IllegalStateException("required key-wrap provider unavailable", failure); }
        finally { Arrays.fill(key, (byte) 0); }
        CRC32 crc = new CRC32(); crc.update(page, 0, 124); PageFormat.put32(page, 124, crc.getValue());
        return page;
    }
    private static byte[] derive(Credential credential, byte[] page) {
        byte[] salt = Arrays.copyOfRange(page, 32, 64);
        if (credential.kind == 2) return Crypto.hkdf(credential.bytes, salt, "jed/v1/kek");
        return Crypto.argon(credential.bytes, salt, new Credential.ArgonParameters((int) PageFormat.u32(page, 16), (int) PageFormat.u32(page, 20), (int) PageFormat.u32(page, 24)));
    }
}
