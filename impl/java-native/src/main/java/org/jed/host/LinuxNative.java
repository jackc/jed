package org.jed.host;

import java.io.IOException;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.invoke.MethodHandle;
import java.nio.file.Path;

import static java.lang.foreign.ValueLayout.ADDRESS;
import static java.lang.foreign.ValueLayout.JAVA_INT;

/**
 * Narrow Linux x86-64 OS seam. These calls operate only on host-owned paths and
 * lock descriptors. No database values or engine memory cross the native seam.
 * Requires --enable-native-access=ALL-UNNAMED for the classpath distribution.
 */
public final class LinuxNative {
    private static final int O_RDONLY = 0;
    private static final int O_RDWR = 2;
    private static final int O_CREAT = 0x40;
    private static final int O_DIRECTORY = 0x10000;
    private static final int O_NOFOLLOW = 0x20000;
    private static final int O_CLOEXEC = 0x80000;
    private static final int LOCK_SH = 1;
    private static final int LOCK_EX = 2;
    private static final int LOCK_NB = 4;
    private static final int LOCK_UN = 8;
    private static final int EINTR = 4;
    private static final int EAGAIN = 11;
    private static final int AT_FDCWD = -100;
    private static final int RENAME_NOREPLACE = 1;

    private LinuxNative() { }

    /** Preserves errno so the API layer can classify unsupported host operations. */
    public static final class NativeIOException extends IOException {
        private static final long serialVersionUID = 1L;
        private final int errno;

        private NativeIOException(String operation, int errno) {
            super(operation + " failed (errno " + errno + ")");
            this.errno = errno;
        }

        public int errno() { return errno; }
        public boolean unsupported() { return errno == 38 || errno == 95; }
    }

    /** Lazy initialization leaves the pure-memory core independent of native access. */
    private static final class Calls {
        static final Linker LINKER = Linker.nativeLinker();
        static final MemoryLayout STATE = Linker.Option.captureStateLayout();
        static final long ERRNO = STATE.byteOffset(MemoryLayout.PathElement.groupElement("errno"));
        static final MethodHandle OPEN = bind("open",
            FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_INT, JAVA_INT), true);
        static final MethodHandle CLOSE = bind("close", FunctionDescriptor.of(JAVA_INT, JAVA_INT), false);
        static final MethodHandle FLOCK = bind("flock", FunctionDescriptor.of(JAVA_INT, JAVA_INT, JAVA_INT), false);
        static final MethodHandle FSYNC = bind("fsync", FunctionDescriptor.of(JAVA_INT, JAVA_INT), false);
        static final MethodHandle RENAME = bind("renameat2",
            FunctionDescriptor.of(JAVA_INT, JAVA_INT, ADDRESS, JAVA_INT, ADDRESS, JAVA_INT), false);

        private static MethodHandle bind(String name, FunctionDescriptor descriptor, boolean variadic) {
            MemorySegment symbol = LINKER.defaultLookup().find(name)
                .orElseThrow(() -> new UnsupportedOperationException("missing Linux operation: " + name));
            return variadic
                ? LINKER.downcallHandle(symbol, descriptor, Linker.Option.captureCallState("errno"),
                    Linker.Option.firstVariadicArg(2))
                : LINKER.downcallHandle(symbol, descriptor, Linker.Option.captureCallState("errno"));
        }
    }

    private static void requireHost() throws IOException {
        if (!System.getProperty("os.name").equals("Linux")
            || !(System.getProperty("os.arch").equals("amd64")
                || System.getProperty("os.arch").equals("x86_64"))) {
            throw new IOException("Java native file coordination requires Linux x86-64");
        }
    }

    private static String nativePath(Path path) throws IOException {
        String text = path.toAbsolutePath().toString();
        if (text.indexOf('\0') >= 0) throw new IOException("NUL in native path");
        return text;
    }

    private static IOException invocationFailure(Throwable failure) {
        if (failure instanceof Error error) throw error;
        return new IOException("Linux native host invocation failed", failure);
    }

    private static int open(Path path, int flags, int mode) throws IOException {
        requireHost();
        String text = nativePath(path);
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment state = arena.allocate(Calls.STATE);
            MemorySegment name = arena.allocateFrom(text);
            while (true) {
                int fd = (int) Calls.OPEN.invokeExact(state, name, flags, mode);
                if (fd >= 0) return fd;
                int errno = state.get(JAVA_INT, Calls.ERRNO);
                if (errno != EINTR) throw new NativeIOException("open " + path, errno);
            }
        } catch (IOException e) {
            throw e;
        } catch (Throwable e) {
            throw invocationFailure(e);
        }
    }

    private static void closeDescriptor(int fd) throws IOException {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment state = arena.allocate(Calls.STATE);
            int result = (int) Calls.CLOSE.invokeExact(state, fd);
            // Linux releases the descriptor even on EINTR. Never retry close: a
            // different thread could already have reused the descriptor number.
            if (result < 0 && state.get(JAVA_INT, Calls.ERRNO) != EINTR) {
                throw new NativeIOException("close", state.get(JAVA_INT, Calls.ERRNO));
            }
        } catch (IOException e) {
            throw e;
        } catch (Throwable e) {
            throw invocationFailure(e);
        }
    }

    public static LockFile openLockFile(Path path) throws IOException {
        return new LockFile(open(path, O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0666));
    }

    /** No timeout loop lives here: every acquisition is nonblocking. */
    public static final class LockFile implements AutoCloseable {
        private int fd;
        private LockFile(int fd) { this.fd = fd; }

        public synchronized boolean tryShared() throws IOException { return flock(LOCK_SH | LOCK_NB); }
        public synchronized boolean tryExclusive() throws IOException { return flock(LOCK_EX | LOCK_NB); }
        public synchronized void unlock() throws IOException { flock(LOCK_UN); }

        private boolean flock(int operation) throws IOException {
            if (fd < 0) throw new IOException("lock file is closed");
            try (Arena arena = Arena.ofConfined()) {
                MemorySegment state = arena.allocate(Calls.STATE);
                while (true) {
                    int result = (int) Calls.FLOCK.invokeExact(state, fd, operation);
                    if (result == 0) return true;
                    int errno = state.get(JAVA_INT, Calls.ERRNO);
                    if (errno == EAGAIN) return false;
                    if (errno != EINTR) throw new NativeIOException("flock", errno);
                }
            } catch (IOException e) {
                throw e;
            } catch (Throwable e) {
                throw invocationFailure(e);
            }
        }

        @Override public synchronized void close() throws IOException {
            if (fd < 0) return;
            int previous = fd;
            fd = -1;
            closeDescriptor(previous);
        }
    }

    /** Makes an already-completed directory entry change durable. */
    public static void syncDirectory(Path directory) throws IOException {
        int fd = open(directory, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW, 0);
        IOException failure = null;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment state = arena.allocate(Calls.STATE);
            while (true) {
                int result = (int) Calls.FSYNC.invokeExact(state, fd);
                if (result == 0) break;
                int errno = state.get(JAVA_INT, Calls.ERRNO);
                if (errno != EINTR) throw new NativeIOException("directory fsync", errno);
            }
        } catch (IOException e) {
            failure = e;
            throw e;
        } catch (Throwable e) {
            failure = invocationFailure(e);
            throw failure;
        } finally {
            try {
                closeDescriptor(fd);
            } catch (IOException e) {
                if (failure != null) failure.addSuppressed(e);
                else throw e;
            }
        }
    }

    /**
     * Atomically publishes a same-filesystem temporary file without clobbering
     * an existing target. Caller syncs the source first and parent afterward.
     */
    public static void renameNoReplace(Path source, Path target) throws IOException {
        requireHost();
        String sourceText = nativePath(source);
        String targetText = nativePath(target);
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment state = arena.allocate(Calls.STATE);
            MemorySegment from = arena.allocateFrom(sourceText);
            MemorySegment to = arena.allocateFrom(targetText);
            int result = (int) Calls.RENAME.invokeExact(state, AT_FDCWD, from, AT_FDCWD, to, RENAME_NOREPLACE);
            if (result < 0) throw new NativeIOException("renameat2", state.get(JAVA_INT, Calls.ERRNO));
        } catch (IOException e) {
            throw e;
        } catch (Throwable e) {
            throw invocationFailure(e);
        }
    }
}
