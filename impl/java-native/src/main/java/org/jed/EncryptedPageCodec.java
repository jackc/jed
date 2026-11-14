package org.jed;

import java.io.ByteArrayOutputStream;
import java.security.GeneralSecurityException;
import java.security.MessageDigest;
import java.util.Arrays;
import java.util.List;
import java.util.function.Consumer;
import org.jed.storage.ManifestEncoder;
import org.jed.storage.PageFormat;

/** XAES page sealing with position-bound authentication and parent-bound tags. */
final class EncryptedPageCodec implements org.jed.storage.PageCodec {
    private final Crypto.Xaes pages, metas;
    private final byte[] dataKey;
    private final Consumer<byte[]> entropy;
    EncryptedPageCodec(byte[] dataKey, Consumer<byte[]> entropy) {
        this.dataKey = dataKey.clone();
        pages = new Crypto.Xaes(Crypto.hkdf(dataKey, new byte[0], "jed/v1/page"));
        metas = new Crypto.Xaes(Crypto.hkdf(dataKey, new byte[0], "jed/v1/meta")); this.entropy = entropy;
    }
    public boolean encrypted() { return true; }
    public byte[] sealBody(long index, byte[] plain) {
        requireSize(plain); if (index < 4 || index > 0xffffffffL) throw new IllegalArgumentException("invalid encrypted page index");
        int end = plain.length - 40;
        byte[] input = Arrays.copyOf(plain, end); Arrays.fill(input, 12, 16, (byte) 0);
        byte[] nonce = nonce(); return pack(pages.seal(nonce, input, associated(1, index)), nonce);
    }
    public byte[] openBody(long index, byte[] stored, byte[] expectedTag) {
        requireSize(stored);
        if (expectedTag == null || expectedTag.length != 16 || !MessageDigest.isEqual(expectedTag, tag(stored))) throw Encoding.corrupt("encrypted page pointer authentication failed");
        int end = stored.length - 40;
        try {
            byte[] decoded = pages.open(Arrays.copyOfRange(stored, end, end + 24), unpack(stored), associated(1, index));
            if (!PageFormat.zeros(decoded, 12, 16)) throw Encoding.corrupt("encrypted body has a nonzero CRC field");
            int kind = decoded[0] & 255, flags = decoded[1] & 255;
            if (kind < 1 || kind > 7 || (kind == 1 ? (flags & ~1) != 0 : flags != 0)) throw Encoding.corrupt("invalid encrypted body header");
            return Arrays.copyOf(decoded, stored.length);
        } catch (GeneralSecurityException failure) { throw Encoding.corrupt("encrypted page authentication failed"); }
    }
    byte[] sealMeta(int slot, byte[] plain, List<ManifestEncoder.Write> manifests) {
        requireSize(plain); byte[] result = plain.clone(), nonce = nonce();
        byte[] tag = metas.seal(nonce, new byte[0], metaAssociated(slot, result, manifests));
        System.arraycopy(nonce, 0, result, result.length - 40, 24); System.arraycopy(tag, 0, result, result.length - 16, 16);
        PageFormat.put32(result, 32, PageFormat.metaCrc(result)); return result;
    }
    void authenticateMeta(int slot, byte[] stored, List<ManifestEncoder.Write> manifests) {
        requireSize(stored);
        try { metas.open(Arrays.copyOfRange(stored, stored.length - 40, stored.length - 16), tag(stored), metaAssociated(slot, stored, manifests)); }
        catch (GeneralSecurityException failure) { throw Encoding.corrupt("encrypted meta authentication failed"); }
    }
    private static byte[] metaAssociated(int slot, byte[] meta, List<ManifestEncoder.Write> manifests) {
        if (slot != 0 && slot != 1) throw new IllegalArgumentException("invalid meta slot");
        byte[] cleared = meta.clone(); Arrays.fill(cleared, 32, 36, (byte) 0); Arrays.fill(cleared, cleared.length - 40, cleared.length, (byte) 0);
        ByteArrayOutputStream output = new ByteArrayOutputStream(); output.writeBytes(associated(2, slot)); output.writeBytes(cleared);
        for (var page : manifests) { byte[] id = new byte[4]; PageFormat.put32(id, 0, page.page()); output.writeBytes(id); output.writeBytes(page.bytes()); }
        return output.toByteArray();
    }
    static byte[] tag(byte[] stored) { return Arrays.copyOfRange(stored, stored.length - 16, stored.length); }
    byte[] wrapKey(int pageSize, long generation, Credential credential) {
        return KeyBlock.wrap(pageSize, generation, dataKey, credential, this::fillEntropy);
    }
    java.util.function.Consumer<byte[]> entropyStream() { return this::fillEntropy; }
    private void fillEntropy(byte[] bytes) { synchronized (entropy) { entropy.accept(bytes); } }
    private byte[] nonce() { byte[] nonce = new byte[24]; fillEntropy(nonce); return nonce; }
    private static byte[] associated(int role, long index) {
        byte[] bytes = {'J', 'E', 'D', 'B', 1, (byte) role, 0, 0, 0, 0}; PageFormat.put32(bytes, 6, index); return bytes;
    }
    private static byte[] pack(byte[] sealed, byte[] nonce) {
        int length = sealed.length - 16; byte[] stored = new byte[length + 40];
        System.arraycopy(sealed, 0, stored, 0, length); System.arraycopy(nonce, 0, stored, length, 24); System.arraycopy(sealed, length, stored, length + 24, 16); return stored;
    }
    private static byte[] unpack(byte[] stored) {
        byte[] sealed = Arrays.copyOf(stored, stored.length - 24); System.arraycopy(stored, stored.length - 16, sealed, sealed.length - 16, 16); return sealed;
    }
    private static void requireSize(byte[] page) { if (!PageFormat.validPageSize(page.length) || page.length < 1024) throw Encoding.corrupt("invalid encrypted page size"); }
}
