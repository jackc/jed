package org.jed.storage;

/** Variant-dependent physical capacities. All algorithms use the same layout instance. */
public record PageLayout(int pageSize, boolean encrypted) {
    public PageLayout {
        if (!PageFormat.validPageSize(pageSize) || encrypted && pageSize < 1024) throw new IllegalArgumentException("invalid variant page size");
    }
    public int reserve() { return encrypted ? 40 : 0; }
    public int tagBytes() { return encrypted ? 16 : 0; }
    public int pointerBytes() { return 4 + tagBytes(); }
    public int firstBody() { return encrypted ? 4 : 2; }
    public int bodyEnd() { return pageSize - reserve(); }
    public int capacity() { return bodyEnd() - 16; }
    public int chainCapacity() { return capacity() - tagBytes(); }
    public int remapCapacity() { return ((pageSize - reserve() - (encrypted ? 96 : 64)) / 3) / (10 + tagBytes()); }
    public int remapStart() { return bodyEnd() - remapCapacity() * (10 + tagBytes()); }
    public int inlineManifestStart() { return encrypted ? 96 : 64; }
    public int inlineManifestCapacity() { return (remapStart() - inlineManifestStart()) / 8; }
    public int overflowManifestCapacity() { return (pageSize - 32) / 8; }
}
