package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import org.jed.host.PageConnection;
import org.jed.host.PageProtocol;
import org.jed.storage.BlockStore;
import org.jed.storage.PageFormat;
import org.jed.storage.PlaintextRecovery;
import static org.jed.host.PageProtocol.*;

/** Client byte device. It never opens the database file; COW writes become a checked owner delta. */
final class RemoteBlockStore implements BlockStore {
    record ReadLease(ReadBegun begun,AutoCloseable release) { }
    private final PageConnection connection;
    private final int pageSize;
    private final Map<Long,Integer> reads=new HashMap<>();
    private final TreeMap<Long,byte[]> staged=new TreeMap<>();
    private byte[] incarnation=new byte[16];
    private PlaintextRecovery.Meta meta,writeMeta;
    private boolean reuse;
    private long virtualSize;
    private boolean closed;
    private volatile boolean handoffPaused;
    private boolean readAhead;
    synchronized void enableReadAhead() { readAhead = true; }
    @Override public synchronized int maxReadPages() { return readAhead ? connection.hello().maxReadPages() : 0; }
    @Override public synchronized List<byte[]> readPages(long[] indices, int requestedPageSize) throws IOException {
        if (requestedPageSize != pageSize || indices.length < 1 || indices.length > maxReadPages()) throw new IOException("invalid batched page read");
        long newest = writeMeta == null ? 0 : writeMeta.txid(); for (long pinned : reads.keySet()) if (Long.compareUnsigned(pinned, newest) > 0) newest = pinned;
        if (newest == 0) throw new IOException("client batched read without snapshot pin");
        var reply = expect(connection.request(new ReadPages(newest, indices)), Pages.class);
        if (reply.pages().size() != indices.length) throw new Violation("wrong batched page reply count");
        return reply.pages();
    }
    RemoteBlockStore(PageConnection connection){this.connection=connection;pageSize=connection.hello().pageSize();connection.onNotice(notice->{if(notice.kind()==1)handoffPaused=notice.connection()!=connection.hello().connection();else if(notice.kind()==2)handoffPaused=false;});}
    int pageSize(){return pageSize;}
    boolean connected(){return connection.connected();}
    long promotionPausedUntil(){return handoffPaused&&connection.closedAt()!=0?connection.closedAt()+2_000_000_000L:0;}
    boolean ownerReadOnly(){return(connection.hello().flags()&2)!=0;}
    void preferOwnership(){if((connection.hello().flags()&4)!=0)try{requestOwnership();}catch(IOException ignored){}}
    boolean requestOwnership()throws IOException{return expect(connection.request(new RequestOwnership()),OwnershipReply.class).status()==0;}
    int commitStatus(long txid,long digest)throws IOException{return expect(connection.request(new CommitStatus(txid,digest)),CommitStatusReply.class).status();}
    synchronized PlaintextRecovery.Meta meta(){return meta;}
    synchronized boolean reuse(){return reuse;}
    synchronized ReadLease beginRead()throws IOException{
        ReadBegun begun=expect(connection.request(new BeginRead(incarnation,meta==null?0:meta.txid())),ReadBegun.class);
        long txid=begun.txid();Integer count=reads.get(txid);reads.put(txid,count==null?1:count+1);
        if(count!=null)connection.send(new EndRead(txid));
        return new ReadLease(begun,()->releaseRead(txid));
    }
    private synchronized void releaseRead(long txid)throws IOException{
        if(closed)return;Integer count=reads.get(txid);if(count==null)throw new IllegalStateException("unbalanced client pin");
        if(count==1){reads.remove(txid);if(!closed&&connection.connected())connection.send(new EndRead(txid));}else reads.put(txid,count-1);
    }
    synchronized AutoCloseable retain(long txid)throws IOException{
        Integer count=reads.get(txid);
        if(count!=null){reads.put(txid,count+1);return()->releaseRead(txid);}
        if(writeMeta!=null&&writeMeta.txid()==txid)return()->{};
        throw new IOException("snapshot has no owner pin");
    }
    WriteBegun beginWrite(long timeoutMillis,boolean relaxed)throws IOException{
        byte[] previous;long txid;synchronized(this){previous=incarnation.clone();txid=meta==null?0:meta.txid();}
        WriteBegun begun=expect(connection.request(new BeginWrite(previous,txid,timeoutMillis,relaxed?1:0)),WriteBegun.class);
        synchronized(this){if(writeMeta!=null)throw new Violation("duplicate client writer");writeMeta=PlaintextRecovery.parseMeta(begun.meta());reuse=begun.reuseGrant();}
        if(!relaxed&&(!begun.pending().isEmpty()||begun.reuse().length!=0))throw new Violation("synchronous writer received a pending batch");
        return begun;
    }
    synchronized void adopt(byte[] owner,byte[] encoded)throws IOException{
        var parsed=PlaintextRecovery.parseMeta(encoded);if(parsed.pageSize()!=pageSize)throw new Violation("owner changed page size");
        incarnation=owner.clone();meta=parsed;virtualSize=Math.max(virtualSize,meta.pageCount()*pageSize);
    }
    synchronized void abort()throws IOException{
        staged.clear();
        if(writeMeta!=null){
            writeMeta=null;
            // Aborting stages no publication. EOF releases the owner's writer gate too,
            // so a lost cleanup frame cannot turn an empty commit into a failed write.
            if(!closed&&connection.connected())try{connection.send(new Abort());}catch(PageConnection.ConnectionLost ignored){ }
        }
    }
    public synchronized byte[] readAt(long offset,int length)throws IOException{
        long end=BlockStore.checkedEnd(offset,length);if(closed)throw new PageConnection.ConnectionLost(new IOException("page client role ended"));if(meta==null||end>virtualSize)throw new IOException("invalid client byte read");
        byte[] result=new byte[length];int copied=0;
        while(copied<length){long index=(offset+copied)/pageSize;int within=(int)((offset+copied)%pageSize),count=Math.min(length-copied,pageSize-within);byte[] page=staged.get(index);
            if(page==null){if(index<2)page=meta.encoded();else{
                long newest=writeMeta==null?0:writeMeta.txid();for(long pinned:reads.keySet())if(Long.compareUnsigned(pinned,newest)>0)newest=pinned;
                if(newest==0)throw new IOException("client page read without a snapshot pin");
                Pages reply=expect(connection.request(new ReadPages(newest,new long[]{index})),Pages.class);if(reply.pages().size()!=1)throw new Violation("wrong page reply count");page=reply.pages().getFirst();}}
            System.arraycopy(page,within,result,copied,count);copied+=count;
        }return result;
    }
    public synchronized void writeAt(long offset,byte[] bytes)throws IOException{
        if(closed||writeMeta==null||offset<0||offset%pageSize!=0||bytes.length!=pageSize||offset+bytes.length>virtualSize)throw new IOException("invalid client staged write");
        staged.put(offset/pageSize,bytes.clone());
    }
    public synchronized void sync()throws IOException{
        if(staged.isEmpty())return;
        if(writeMeta==null)throw new IOException("staged commit without writer pin");
        byte[] encoded=staged.get(0L);if(encoded==null)encoded=staged.get(1L);if(encoded==null)throw new IOException("client sync without publication meta");
        var next=PlaintextRecovery.parseMeta(encoded);List<IndexedPage> manifests=new ArrayList<>();var manifestIds=new java.util.HashSet<Long>();
        for(long index=next.manifestHead();index!=0;){byte[] page=staged.get(index);if(page==null||!manifestIds.add(index))throw new IOException("missing staged manifest");manifests.add(new IndexedPage(index,page));index=PageFormat.u32(page,8);}
        // Reserve bounded frame bodies; connection serializes every frame through commit_end.
        List<CommitPages> batches=new ArrayList<>();List<IndexedPage> batch=new ArrayList<>();int cap=(MAX_FRAME-9)/(pageSize+4);
        for(var entry:staged.entrySet())if(entry.getKey()>=2&&!manifestIds.contains(entry.getKey())){batch.add(new IndexedPage(entry.getKey(),entry.getValue()));if(batch.size()==cap){batches.add(new CommitPages(List.copyOf(batch)));batch.clear();}}
        if(!batch.isEmpty())batches.add(new CommitPages(List.copyOf(batch)));
        // This preflight happens before any frame is sent, including the manifest size limit.
        PageProtocol.encode(new Frame(1,new CommitBegin(next.txid(),encoded,manifests)),pageSize);
        Committed reply=expect(connection.commit(new CommitBegin(next.txid(),encoded,manifests),batches),Committed.class);
        if(reply.txid()!=next.txid())throw new Violation("wrong committed generation");
        writeMeta=null;meta=next;staged.clear();
    }
    public synchronized long size(){return virtualSize;}
    public synchronized void setSize(long length)throws IOException{if(length<0)throw new IOException("invalid client size");virtualSize=Math.max(virtualSize,length);}
    public synchronized void close(){if(closed)return;closed=true;staged.clear();reads.clear();connection.close();}
    private static <T extends Message>T expect(Message actual,Class<T> expected)throws Violation{if(!expected.isInstance(actual))throw new Violation("unexpected owner reply");return expected.cast(actual);}
}
