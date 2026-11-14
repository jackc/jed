package org.jed;

import java.nio.CharBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.StandardCharsets;

/** File-unlock credential. It is never exposed to SQL or included in diagnostic text. */
public final class Credential {
    public record ArgonParameters(int memoryKiB, int iterations, int parallelism) {
        public ArgonParameters {
            if (parallelism < 1 || parallelism > 16 || iterations < 1 || iterations > 16
                || memoryKiB < 8 * parallelism || memoryKiB > 1_048_576)
                throw new SqlException("22023", "Argon2id parameters are outside the supported bounds");
        }
        public static ArgonParameters defaults() { return new ArgonParameters(65_536, 3, 4); }
    }
    final int kind;
    final byte[] bytes;
    final ArgonParameters parameters;
    private Credential(int kind, byte[] bytes, ArgonParameters parameters) {
        this.kind = kind; this.bytes = bytes.clone(); this.parameters = parameters;
    }
    public static Credential hostKey(byte[] key) {
        if (key == null || key.length != 32) throw new SqlException("22023", "host key must contain 32 bytes");
        return new Credential(2, key, null);
    }
    public static Credential passphrase(String passphrase) { return passphrase(passphrase, ArgonParameters.defaults()); }
    public static Credential passphrase(String passphrase, ArgonParameters parameters) {
        java.util.Objects.requireNonNull(passphrase); java.util.Objects.requireNonNull(parameters);
        try {
            var encoded = StandardCharsets.UTF_8.newEncoder().encode(CharBuffer.wrap(passphrase));
            byte[] bytes = new byte[encoded.remaining()]; encoded.get(bytes);
            return new Credential(1, bytes, parameters);
        } catch (CharacterCodingException failure) { throw new SqlException("22021", "passphrase contains invalid Unicode"); }
    }
    @Override public String toString() { return "Credential[redacted]"; }
}
