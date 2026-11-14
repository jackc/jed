package org.jed.storage;

import java.io.IOException;

/** Format corruption (XX001), distinct from a host I/O failure (58030). */
public final class CorruptStorageException extends IOException {
    private static final long serialVersionUID = 1L;
    public CorruptStorageException(String message) { super(message); }
}
