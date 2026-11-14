package org.jed;

import java.util.concurrent.atomic.AtomicBoolean;

/** Explicit cancellation signal; it never interrupts or closes a shared host device. */
public final class Cancellation {
    private final AtomicBoolean cancelled = new AtomicBoolean();
    public void cancel() { cancelled.set(true); }
    public boolean isCancelled() { return cancelled.get(); }
}
