package org.jed;

/** Reproducible splitmix64 source. For tests and reproducibility; not cryptographic randomness. */
public final class SeededRandom implements java.util.function.Consumer<byte[]> {
    private long state;
    public SeededRandom(long seed) { state = seed; }
    public synchronized void reseed(long seed) { state = seed; }
    private long next() {
        long value = state += 0x9e3779b97f4a7c15L;
        value = (value ^ value >>> 30) * 0xbf58476d1ce4e5b9L;
        value = (value ^ value >>> 27) * 0x94d049bb133111ebL;
        return value ^ value >>> 31;
    }
    @Override public synchronized void accept(byte[] output) {
        for (int position = 0; position < output.length;) {
            long word = next();
            for (int i = 0; i < 8 && position < output.length; i++) output[position++] = (byte) (word >>> (56 - i * 8));
        }
    }
}
