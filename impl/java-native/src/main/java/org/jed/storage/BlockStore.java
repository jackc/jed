package org.jed.storage;

import java.io.IOException;

/**
 * Positioned byte device from spec/design/hosts.md. Page layout, allocation policy,
 * locking, encryption, and commit ordering belong above this seam.
 *
 * <p>Reads return an independently owned array. A write is staged until sync;
 * growing setSize writes zeros and completes a metadata durability barrier.
 * Implementations support concurrent positioned reads. Mutation has one writer.
 */
public interface BlockStore extends AutoCloseable {
    byte[] readAt(long offset, int length) throws IOException;
    void writeAt(long offset, byte[] bytes) throws IOException;
    void sync() throws IOException;
    long size() throws IOException;
    void setSize(long length) throws IOException;
    /** Optional bounded transport batching; ordinary local devices leave it disabled. */
    default int maxReadPages() { return 0; }
    default java.util.List<byte[]> readPages(long[] indices, int pageSize) throws IOException {
        var result = new java.util.ArrayList<byte[]>(indices.length);
        for (long index : indices) result.add(readAt(Math.multiplyExact(index, pageSize), pageSize));
        return result;
    }
    @Override void close() throws IOException;

    static long checkedEnd(long offset, int length) throws IOException {
        if (offset < 0 || length < 0 || offset > Long.MAX_VALUE - length) {
            throw new IOException("invalid byte-device range");
        }
        return offset + length;
    }
}
