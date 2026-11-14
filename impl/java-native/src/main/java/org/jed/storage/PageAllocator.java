package org.jed.storage;

import java.io.IOException;
import java.util.Arrays;

/** Ascending safe-free-page allocation followed by the u32 high-water. */
public final class PageAllocator {
    private final long[] reusable;
    private int position;
    private long highWater;

    /** Caller supplies only the current reader-watermark-safe subset. */
    public PageAllocator(long highWater, long[] reusable) {
        if (highWater < 2 || highWater > 0xffffffffL) throw new IllegalArgumentException("invalid page high-water");
        this.highWater = highWater;
        this.reusable = reusable.clone();
        long previous = 1;
        for (long page : reusable) {
            if (page <= previous || page >= highWater) throw new IllegalArgumentException("invalid reusable page ordering");
            previous = page;
        }
    }

    private PageAllocator(PageAllocator original) {
        reusable = original.reusable;
        position = original.position;
        highWater = original.highWater;
    }

    public PageAllocator copy() { return new PageAllocator(this); }
    public long highWater() { return highWater; }
    public long[] remainingPages() { return Arrays.copyOfRange(reusable, position, reusable.length); }

    public long allocate() throws IOException {
        if (position < reusable.length) return reusable[position++];
        if (highWater == 0xffffffffL) throw new TreeCapacityException("database page limit exceeded");
        return highWater++;
    }
}
