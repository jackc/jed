package org.jed;

/** Host-controlled epoch-microsecond clock shared by every environment consumer. */
public final class ManualClock implements java.util.function.LongSupplier {
    private long micros;
    public ManualClock(long micros) { this.micros = micros; }
    public synchronized void set(long value) { micros = value; }
    public synchronized void advance(long step) { micros = Math.addExact(micros, step); }
    @Override public synchronized long getAsLong() { return micros; }
}
