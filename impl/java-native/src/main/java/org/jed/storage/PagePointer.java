package org.jed.storage;

/** Immutable physical page identity, authenticated by the parent in encrypted layouts. */
public record PagePointer(long page, byte[] tag) {
    public PagePointer { tag = tag == null ? new byte[0] : tag.clone(); }
    public PagePointer(long page) { this(page, new byte[0]); }
    @Override public byte[] tag() { return tag.clone(); }
    @Override public boolean equals(Object value) { return value instanceof PagePointer other && page == other.page && java.util.Arrays.equals(tag, other.tag); }
    @Override public int hashCode() { return Long.hashCode(page) * 31 + java.util.Arrays.hashCode(tag); }
}
