package org.jed;

/** Local-file host options. Coordination remains outside SQL values and cost accounting. */
public final class FileOptions {
    public enum Ownership { PREFER, YIELD, NEVER }
    private final int pageSize,maxConnections;
    private final long maxStorageBytes,cacheBytes,lockTimeoutMillis,yieldGraceMillis,shutdownGraceMillis,idleSessionMillis,idleTransactionMillis;
    private final boolean serve,readOnly,protectedScratch,synchronous;
    private final long flushIntervalMillis;
    private final Ownership ownership;
    private final Credential credential;
    private final Environment environment;
    final ScratchHost scratchHost;
    final FileHost storageHost;
    final FileRoleHost roleHost;
    final boolean idleFlush;
    private FileOptions(Builder b){scratchHost=b.scratchHost;roleHost=b.roleHost;storageHost=b.storageHost;idleFlush=b.idleFlush;synchronous=b.synchronous;flushIntervalMillis=b.flushIntervalMillis;maxStorageBytes=b.maxStorageBytes;pageSize=b.pageSize;cacheBytes=b.cacheBytes;lockTimeoutMillis=b.lockTimeoutMillis;yieldGraceMillis=b.yieldGraceMillis;shutdownGraceMillis=b.shutdownGraceMillis;idleSessionMillis=b.idleSessionMillis;idleTransactionMillis=b.idleTransactionMillis;maxConnections=b.maxConnections;serve=b.serve;readOnly=b.readOnly;protectedScratch=b.protectedScratch;ownership=b.ownership;credential=b.credential;environment=b.environment;}
    public static Builder builder(){return new Builder();} public static FileOptions defaults(){return builder().build();}
    public boolean synchronous(){return synchronous;}public long flushIntervalMillis(){return flushIntervalMillis;}
    public long maxStorageBytes(){return maxStorageBytes;}public int pageSize(){return pageSize;}public long cacheBytes(){return cacheBytes;}public long lockTimeoutMillis(){return lockTimeoutMillis;}
    public long yieldGraceMillis(){return yieldGraceMillis;}public long shutdownGraceMillis(){return shutdownGraceMillis;}public long idleSessionMillis(){return idleSessionMillis;}public long idleTransactionMillis(){return idleTransactionMillis;}
    public int maxConnections(){return maxConnections;}public boolean protectedScratch(){return protectedScratch;}public boolean serve(){return serve;}public boolean readOnly(){return readOnly;}public Ownership ownership(){return ownership;}public Credential credential(){return credential;}public Environment environment(){return environment;}
    public Builder toBuilder() {
        return builder().pageSize(pageSize).maxStorageBytes(maxStorageBytes).cacheBytes(cacheBytes)
            .lockTimeoutMillis(lockTimeoutMillis).yieldGraceMillis(yieldGraceMillis).shutdownGraceMillis(shutdownGraceMillis)
            .idleSessionMillis(idleSessionMillis).idleTransactionMillis(idleTransactionMillis).maxConnections(maxConnections)
            .synchronous(synchronous).flushIntervalMillis(flushIntervalMillis).storageHost(storageHost).scratchHost(scratchHost).roleHost(roleHost).idleFlush(idleFlush).serve(serve).readOnly(readOnly).protectedScratch(protectedScratch).ownership(ownership).credential(credential).environment(environment);
    }
    public static final class Builder {
        private int pageSize=8192,maxConnections;
        private long maxStorageBytes;
        private long cacheBytes=1_048_576,lockTimeoutMillis=5000,yieldGraceMillis=5000,shutdownGraceMillis=30000,idleSessionMillis,idleTransactionMillis=60000;
        private boolean serve=true,readOnly,protectedScratch,synchronous=true;
        private long flushIntervalMillis=1000;
        private ScratchHost scratchHost = ScratchHost.NATIVE;
        Builder scratchHost(ScratchHost value){scratchHost=java.util.Objects.requireNonNull(value);return this;}
        private FileHost storageHost;
        private FileRoleHost roleHost;
        Builder roleHost(FileRoleHost value){roleHost=value;return this;}
        private boolean idleFlush=true;
        Builder storageHost(FileHost value){storageHost=value;return this;}
        Builder idleFlush(boolean value){idleFlush=value;return this;}
        private Ownership ownership=Ownership.PREFER;private Credential credential;private Environment environment=Environment.empty();
        public Builder pageSize(int value){if(!org.jed.storage.PageFormat.validPageSize(value))throw new IllegalArgumentException("page size");pageSize=value;return this;}
        public Builder maxStorageBytes(long value){maxStorageBytes=Math.max(0,value);return this;}
        public Builder cacheBytes(long value){cacheBytes=nonnegative(value);return this;}public Builder lockTimeoutMillis(long value){lockTimeoutMillis=nonnegative(value);return this;}
        public Builder yieldGraceMillis(long value){yieldGraceMillis=nonnegative(value);return this;}public Builder shutdownGraceMillis(long value){shutdownGraceMillis=nonnegative(value);return this;}
        public Builder idleSessionMillis(long value){idleSessionMillis=nonnegative(value);return this;}public Builder idleTransactionMillis(long value){idleTransactionMillis=nonnegative(value);return this;}
        public Builder maxConnections(int value){if(value<0)throw new IllegalArgumentException("negative connection limit");maxConnections=value;return this;}
        public Builder synchronous(boolean value){synchronous=value;return this;}
        public Builder flushIntervalMillis(long value){flushIntervalMillis=nonnegative(value);return this;}
        public Builder protectedScratch(boolean value){protectedScratch=value;return this;}
        public Builder serve(boolean value){serve=value;return this;}public Builder readOnly(boolean value){readOnly=value;return this;}
        public Builder ownership(Ownership value){ownership=java.util.Objects.requireNonNull(value);return this;}public Builder credential(Credential value){credential=value;return this;}public Builder environment(Environment value){environment=java.util.Objects.requireNonNull(value);return this;}
        public FileOptions build(){return new FileOptions(this);}private static long nonnegative(long value){if(value<0)throw new IllegalArgumentException("negative host option");return value;}
    }
}
