package org.jed;

import java.nio.file.Path;
import java.util.Objects;

/** Host-only source for an independent attached database; SQL never opens a path. */
public final class AttachSource {
    private final Path path;
    private final FileOptions options;
    private final long maxStorageBytes;
    private AttachSource(Path path, FileOptions options, long limit) {
        this.path = path; this.options = options; maxStorageBytes = Math.max(0, limit);
    }
    public static AttachSource memory() { return new AttachSource(null, null, 0); }
    public static AttachSource file(Path path) { return file(path, FileOptions.defaults()); }
    public static AttachSource file(Path path, FileOptions options) {
        Objects.requireNonNull(options); return new AttachSource(Objects.requireNonNull(path), options, options.maxStorageBytes());
    }
    public AttachSource maxStorageBytes(long value) { return new AttachSource(path, options, value); }
    Path path() { return path; }
    FileOptions options() { return options; }
    long maxStorageBytes() { return maxStorageBytes; }
}
