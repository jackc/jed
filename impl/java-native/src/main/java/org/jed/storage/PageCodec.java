package org.jed.storage;

import java.io.IOException;
import java.security.MessageDigest;
import java.util.Arrays;

/** Safe codec seam above the byte device and below the decoded page cache. */
public interface PageCodec {
    boolean encrypted();
    byte[] sealBody(long index, byte[] plain) throws IOException;
    byte[] openBody(long index, byte[] stored, byte[] expectedTag) throws IOException;
    default byte[] bodyTag(byte[] stored) { return encrypted() ? Arrays.copyOfRange(stored, stored.length - 16, stored.length) : new byte[0]; }
    default void checkTag(byte[] actual, byte[] expected) throws CorruptStorageException {
        if (encrypted() && (expected == null || expected.length != 16 || !MessageDigest.isEqual(actual, expected))) throw new CorruptStorageException("encrypted pointer authentication failed");
    }
    PageCodec PLAINTEXT = new PageCodec() {
        public boolean encrypted() { return false; }
        public byte[] sealBody(long index, byte[] plain) { byte[] bytes = plain.clone(); PageFormat.put32(bytes, 12, PageFormat.bodyCrc(bytes)); return bytes; }
        public byte[] openBody(long index, byte[] stored, byte[] expectedTag) throws IOException { PageFormat.parseBody(stored); return stored; }
    };
}
