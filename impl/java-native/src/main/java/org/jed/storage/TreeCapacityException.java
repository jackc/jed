package org.jed.storage;

import java.io.IOException;

/** A legal record/key cannot fit this page size (write-side SQLSTATE 0A000). */
public final class TreeCapacityException extends IOException {
    private static final long serialVersionUID = 1L;
    public TreeCapacityException(String message) { super(message); }
}
