package org.jed;

import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.util.Arrays;
import javax.crypto.Cipher;
import javax.crypto.KDF;
import javax.crypto.spec.GCMParameterSpec;
import javax.crypto.spec.HKDFParameterSpec;
import javax.crypto.spec.SecretKeySpec;
import org.bouncycastle.crypto.generators.Argon2BytesGenerator;
import org.bouncycastle.crypto.params.Argon2Parameters;

/** Approved JDK cryptographic primitives; Bouncy Castle is used only for Argon2id. */
final class Crypto {
    private Crypto() { }
    static byte[] hkdf(byte[] input, byte[] salt, String info) {
        try {
            return KDF.getInstance("HKDF-SHA256").deriveData(HKDFParameterSpec.ofExtract()
                .addIKM(input).addSalt(salt).thenExpand(info.getBytes(StandardCharsets.US_ASCII), 32));
        } catch (GeneralSecurityException failure) { throw new IllegalStateException("required HKDF provider unavailable", failure); }
    }
    static byte[] argon(byte[] password, byte[] salt, Credential.ArgonParameters parameters) {
        var generator = new Argon2BytesGenerator();
        generator.init(new Argon2Parameters.Builder(Argon2Parameters.ARGON2_id)
            .withVersion(Argon2Parameters.ARGON2_VERSION_13).withSalt(salt)
            .withMemoryAsKB(parameters.memoryKiB()).withIterations(parameters.iterations())
            .withParallelism(parameters.parallelism()).build());
        byte[] output = new byte[32]; generator.generateBytes(password, output); return output;
    }
    static byte[] gcm(boolean encrypt, byte[] key, byte[] nonce, byte[] input, byte[] associated) throws GeneralSecurityException {
        Cipher cipher = Cipher.getInstance("AES/GCM/NoPadding");
        cipher.init(encrypt ? Cipher.ENCRYPT_MODE : Cipher.DECRYPT_MODE, new SecretKeySpec(key, "AES"), new GCMParameterSpec(128, nonce));
        cipher.updateAAD(associated); return cipher.doFinal(input);
    }
    static final class Xaes {
        private final byte[] key, doubled;
        Xaes(byte[] key) {
            if (key.length != 32) throw new IllegalArgumentException("XAES requires a 32-byte key");
            this.key = key.clone(); byte[] block = block(new byte[16]); doubled = new byte[16];
            for (int i = 0; i < 16; i++) doubled[i] = (byte) ((block[i] & 255) << 1 | (i == 15 ? 0 : (block[i + 1] & 255) >>> 7));
            doubled[15] ^= (byte) (0x87 & -((block[0] & 255) >>> 7));
            Arrays.fill(block, (byte)0);
        }
        private byte[] block(byte[] input) {
            try {
                Cipher cipher = Cipher.getInstance("AES/ECB/NoPadding"); cipher.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(key, "AES"));
                return cipher.doFinal(input);
            } catch (GeneralSecurityException failure) { throw new IllegalStateException("required AES provider unavailable", failure); }
        }
        private byte[] subkey(byte[] nonce) {
            if (nonce.length != 24) throw new IllegalArgumentException("XAES requires a 24-byte nonce");
            byte[] input = new byte[32];
            for (int n = 0; n < 2; n++) {
                int offset = n * 16; input[offset + 1] = (byte) (n + 1); input[offset + 2] = 0x58;
                System.arraycopy(nonce, 0, input, offset + 4, 12);
                for (int i = 0; i < 16; i++) input[offset + i] ^= doubled[i];
            }
            return block(input);
        }
        byte[] seal(byte[] nonce, byte[] plain, byte[] associated) {
            byte[] temporaryKey = subkey(nonce);
            try { return gcm(true, temporaryKey, Arrays.copyOfRange(nonce, 12, 24), plain, associated); }
            catch (GeneralSecurityException failure) { throw new IllegalStateException("required AES-GCM provider unavailable", failure); }
            finally { Arrays.fill(temporaryKey, (byte)0); }
        }
        byte[] open(byte[] nonce, byte[] sealed, byte[] associated) throws GeneralSecurityException {
            byte[] temporaryKey = subkey(nonce);
            try { return gcm(false, temporaryKey, Arrays.copyOfRange(nonce, 12, 24), sealed, associated); }
            finally { Arrays.fill(temporaryKey, (byte)0); }
        }
    }
}
