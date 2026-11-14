package org.jed.storage;

import java.io.EOFException;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.channels.ClosedChannelException;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/** Raw file host. The caller must establish coordination before reading database bytes. */
public final class FileBlockStore implements BlockStore {
    private static final int ZERO_CHUNK = 64 * 1024;
    private final FileChannel channel;
    private final boolean durable;

    private FileBlockStore(FileChannel channel) {
        this(channel, true);
    }
    private FileBlockStore(FileChannel channel, boolean durable) {
        this.channel = channel; this.durable = durable;
    }

    /** Open an existing file, without following a final-component symbolic link. */
    public static FileBlockStore open(Path path, boolean writable) throws IOException {
        return new FileBlockStore(writable
            ? FileChannel.open(path, StandardOpenOption.READ, StandardOpenOption.WRITE,
                LinkOption.NOFOLLOW_LINKS)
            : FileChannel.open(path, StandardOpenOption.READ, LinkOption.NOFOLLOW_LINKS));
    }

    /** Private disposable scratch: no growth or sync durability barriers. */
    public static FileBlockStore openScratch(Path path) throws IOException {
        return new FileBlockStore(FileChannel.open(path, StandardOpenOption.READ, StandardOpenOption.WRITE,
            LinkOption.NOFOLLOW_LINKS), false);
    }

    /** Create an unpublished new file. This does not implement database publication. */
    public static FileBlockStore createNew(Path path) throws IOException {
        return new FileBlockStore(FileChannel.open(path, StandardOpenOption.READ,
            StandardOpenOption.WRITE, StandardOpenOption.CREATE_NEW, LinkOption.NOFOLLOW_LINKS));
    }

    /** Rewrite an unpublished temporary image without a separate directory deletion. */
    public static FileBlockStore createOrTruncate(Path path) throws IOException {
        return new FileBlockStore(FileChannel.open(path, StandardOpenOption.READ,
            StandardOpenOption.WRITE, StandardOpenOption.CREATE, StandardOpenOption.TRUNCATE_EXISTING, LinkOption.NOFOLLOW_LINKS));
    }

    @Override public byte[] readAt(long offset, int length) throws IOException {
        ensureOpen();
        BlockStore.checkedEnd(offset, length);
        byte[] bytes = new byte[length];
        ByteBuffer target = ByteBuffer.wrap(bytes);
        while (target.hasRemaining()) {
            int n = channel.read(target, offset + target.position());
            if (n < 0) throw new EOFException("short byte-device read");
            if (n == 0) throw new IOException("byte-device read made no progress");
        }
        return bytes;
    }

    @Override public void writeAt(long offset, byte[] bytes) throws IOException {
        ensureOpen();
        BlockStore.checkedEnd(offset, bytes.length);
        writeBuffer(offset, ByteBuffer.wrap(bytes));
    }

    private void writeBuffer(long offset, ByteBuffer source) throws IOException {
        int initial = source.position();
        while (source.hasRemaining()) {
            if (channel.write(source, offset + source.position() - initial) <= 0) {
                throw new IOException("byte-device write made no progress");
            }
        }
    }

    @Override public void sync() throws IOException {
        // The JDK file host maps force(false) to its data-only barrier where available.
        if (durable) channel.force(false);
    }

    @Override public long size() throws IOException { return channel.size(); }

    @Override public synchronized void setSize(long length) throws IOException {
        if (length < 0) throw new IOException("negative byte-device size");
        long previous = channel.size();
        if (length < previous) {
            channel.truncate(length);
        } else if (length > previous) {
            // Do not sparse-truncate: allocate real zeros, with bounded scratch space.
            byte[] zeros = new byte[(int) Math.min(ZERO_CHUNK, length - previous)];
            for (long at = previous; at < length;) {
                int n = (int) Math.min(zeros.length, length - at);
                writeBuffer(at, ByteBuffer.wrap(zeros, 0, n));
                at += n;
            }
            if (durable) channel.force(true);
        }
    }

    @Override public void close() throws IOException { channel.close(); }

    private void ensureOpen() throws ClosedChannelException {
        if (!channel.isOpen()) throw new ClosedChannelException();
    }
}
