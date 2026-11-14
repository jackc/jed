package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.HashSet;
import java.util.HexFormat;
import java.util.List;
import java.util.Map;
import java.util.function.Consumer;
import org.jed.storage.*;

/** Shared XAES/HKDF/Argon/key-block vectors and encrypted page goldens. */
public final class CryptoTest {
    public static void main(String[] args) throws Exception {
        Path root = Path.of(args.length == 0 ? "spec/fileformat" : args[0]);
        Map<String, Map<String, String>> sections = new HashMap<>(); List<Map<String, String>> xaes = new ArrayList<>(); Map<String, String> current = null;
        for (String source : Files.readAllLines(root.resolve("encryption_vectors.toml"))) {
            String line = source.trim(); if (line.isEmpty() || line.startsWith("#")) continue;
            if (line.equals("[[xaes]]")) { current = new HashMap<>(); xaes.add(current); }
            else if (line.startsWith("[")) { current = new HashMap<>(); sections.put(line.substring(1, line.length() - 1), current); }
            else if (current != null) { int equals = line.indexOf('='); String value = line.substring(equals + 1).trim(); current.put(line.substring(0, equals).trim(), value.startsWith("\"") ? value.substring(1, value.length() - 1) : value); }
        }
        for (var vector : xaes) {
            var codec = new Crypto.Xaes(hex(vector, "key_hex")); byte[] nonce = hex(vector, "nonce_hex"), plain = hex(vector, "plaintext_hex"), aad = hex(vector, "aad_hex"), sealed = hex(vector, "sealed_hex");
            check(Arrays.equals(codec.seal(nonce, plain, aad), sealed), vector.get("name") + " seal");
            check(Arrays.equals(codec.open(nonce, sealed, aad), plain), vector.get("name") + " open");
        }
        var subkeys = sections.get("subkeys"); byte[] dek = hex(subkeys, "dek_hex");
        check(Arrays.equals(Crypto.hkdf(dek, new byte[0], "jed/v1/page"), hex(subkeys, "page_key_hex")), "page subkey");
        check(Arrays.equals(Crypto.hkdf(dek, new byte[0], "jed/v1/meta"), hex(subkeys, "meta_key_hex")), "meta subkey");
        var host = sections.get("host_kek");
        check(Arrays.equals(Crypto.hkdf(hex(host, "key_hex"), hex(host, "kdf_salt_hex"), "jed/v1/kek"), hex(host, "kek_hex")), "host KEK");
        var argon = sections.get("argon2id");
        check(Arrays.equals(Crypto.argon(argon.get("password").getBytes(StandardCharsets.UTF_8), argon.get("salt").getBytes(StandardCharsets.UTF_8),
            new Credential.ArgonParameters(Integer.parseInt(argon.get("memory_kib")), Integer.parseInt(argon.get("iterations")), Integer.parseInt(argon.get("parallelism")))), hex(argon, "key_hex")), "Argon2id vector");
        var block = sections.get("key_block"); Consumer<byte[]> stream = seeded(Long.parseLong(block.get("seed"))); byte[] key = new byte[32]; stream.accept(key);
        check(Arrays.equals(key, hex(block, "dek_hex")), "DEK entropy order");
        var credential = Credential.hostKey(hex(host, "key_hex"));
        byte[] wrapped = KeyBlock.wrap(1024, 1, key, credential, stream);
        check(Arrays.equals(Arrays.copyOf(wrapped, 128), hex(block, "block_hex")), "key block golden");
        check(Arrays.equals(KeyBlock.unlock(KeyBlock.select(wrapped, wrapped), credential), key), "key unwrap");
        var rewrap = sections.get("rewrap"); byte[] newer = KeyBlock.wrap(1024, 2, key, Credential.hostKey(hex(rewrap, "key_hex")), seeded(Long.parseLong(rewrap.get("seed"))));
        check(Arrays.equals(Arrays.copyOf(newer, 128), hex(rewrap, "block_hex")), "rewrap golden");
        try { KeyBlock.unlock(KeyBlock.select(wrapped, newer), credential); throw new AssertionError("new key block fell back to old credential"); }
        catch (SqlException error) { check(error.sqlState().equals("28P01"), "credential failure state"); }
        byte[] damaged = wrapped.clone(); damaged[16] = 127; checksumKeyBlock(damaged);
        check(!KeyBlock.valid(damaged), "host-key KDF bounds");
        check(KeyBlock.select(damaged, wrapped).slot() == 3, "invalid copy selection");
        byte[] unequal = wrapped.clone(); unequal[32] ^= 1; checksumKeyBlock(unequal);
        try { KeyBlock.select(wrapped, unequal); throw new AssertionError("equal-generation copies accepted"); } catch (SqlException expected) { check(expected.sqlState().equals("XX001"), "pair corruption state"); }
        int pages = 0;
        for (String name : List.of("encrypted_empty_db", "encrypted_pk_table", "encrypted_catalog", "encrypted_overflow_table", "encrypted_index_table", "encrypted_rekey_index_table")) {
            try (var store = FileBlockStore.open(root.resolve("fixtures").resolve(name + ".jed"), false)) {
                int size = (int) PageFormat.u32(store.readAt(0, 12), 8);
                var selected = KeyBlock.select(store.readAt(2L * size, size), store.readAt(3L * size, size));
                byte[] fileKey = KeyBlock.unlock(selected, credential); var codec = new EncryptedPageCodec(fileKey, ignored -> { throw new AssertionError("unexpected entropy draw"); });
                var manifestIds = new HashSet<Long>();
                for (int slot = 0; slot < 2; slot++) {
                    byte[] meta = store.readAt((long) slot * size, size); List<ManifestEncoder.Write> manifests = new ArrayList<>();
                    for (long id = PageFormat.u32(meta, 36); id != 0;) {
                        byte[] page = store.readAt(id * size, size); manifestIds.add(id); manifests.add(new ManifestEncoder.Write(id, page)); id = PageFormat.u32(page, 8);
                    }
                    codec.authenticateMeta(slot, meta, manifests);
                    long rootId = PageFormat.u32(meta, 20); codec.openBody(rootId, store.readAt(rootId * size, size), Arrays.copyOfRange(meta, 64, 80));
                    byte[] modified = meta.clone(); modified[12] ^= 1;
                    try { codec.authenticateMeta(slot, modified, manifests); throw new AssertionError("altered meta authenticated"); } catch (SqlException expected) { check(expected.sqlState().equals("XX001"), "meta failure state"); }
                }
                for (long id = 4; id < store.size() / size; id++) if (!manifestIds.contains(id)) {
                    byte[] stored = store.readAt(id * size, size), nonce = Arrays.copyOfRange(stored, size - 40, size - 16);
                    byte[] plain = codec.openBody(id, stored, EncryptedPageCodec.tag(stored));
                    var reseal = new EncryptedPageCodec(fileKey, target -> System.arraycopy(nonce, 0, target, 0, nonce.length));
                    check(Arrays.equals(reseal.sealBody(id, plain), stored), name + " exact body seal"); pages++;
                    byte[] wrongTag = EncryptedPageCodec.tag(stored); wrongTag[0] ^= 1;
                    try { codec.openBody(id, stored, wrongTag); throw new AssertionError("wrong bound tag accepted"); } catch (SqlException expected) { check(expected.sqlState().equals("XX001"), "bound pointer failure state"); }
                }
            }
        }
        System.out.println("CryptoTest: passed shared crypto/key-block vectors and " + pages + " exact encrypted body pages across 6 images");
    }
    private static byte[] hex(Map<String, String> values, String key) { return HexFormat.of().parseHex(values.get(key)); }
    private static void checksumKeyBlock(byte[] page) { var crc = new java.util.zip.CRC32(); crc.update(page, 0, 124); PageFormat.put32(page, 124, crc.getValue()); }
    static Consumer<byte[]> seeded(long seed) {
        return new Consumer<>() {
            long state = seed;
            public void accept(byte[] bytes) {
                for (int offset = 0; offset < bytes.length;) {
                    long value = state += 0x9e3779b97f4a7c15L; value = (value ^ value >>> 30) * 0xbf58476d1ce4e5b9L;
                    value = (value ^ value >>> 27) * 0x94d049bb133111ebL; value ^= value >>> 31;
                    for (int i = 0; i < 8 && offset < bytes.length; i++) bytes[offset++] = (byte) (value >>> (56 - i * 8));
                }
            }
        };
    }
    private static void check(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
}
