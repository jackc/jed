package org.jed;

/** Host-controlled deterministic clock that advances exactly once per read. */
public final class AdvancingClock implements java.util.function.LongSupplier {
    private long next, step;
    public AdvancingClock(long start, long step) { reset(start, step); }
    public synchronized void reset(long start, long step) { this.next = start; this.step = step; }
    @Override public synchronized long getAsLong() { long result = next; next = Math.addExact(next, step); return result; }
}
