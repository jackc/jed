package org.jed.host;

import java.util.ArrayDeque;
import java.util.Arrays;
import java.util.Deque;
import java.util.TreeSet;
import org.jed.storage.PageFormat;

/** Owner-local bounded invalidation and uncertain-commit resolution histories. */
public final class PageWindows {
    public record Invalidation(boolean all,long[] pages) { }
    private record Commit(long txid,long digest,long[] pages) { }
    private final byte[] incarnation;
    private final Deque<Commit> history=new ArrayDeque<>();
    private long oldest,current;
    public PageWindows(byte[] incarnation,byte[] currentMeta,byte[] previousMeta){
        if(incarnation.length!=16)throw new IllegalArgumentException("incarnation");this.incarnation=incarnation.clone();
        current=PageFormat.u64(currentMeta,12);oldest=current;
        if(previousMeta!=null&&PageFormat.u64(previousMeta,12)==current-1)history.add(new Commit(current-1,digest(previousMeta),new long[0]));
        history.add(new Commit(current,digest(currentMeta),new long[0]));
    }
    public byte[] incarnation(){return incarnation.clone();}
    public synchronized void published(byte[] meta,long[] written){
        long txid=PageFormat.u64(meta,12);if(current==-1L||txid!=current+1)throw new IllegalArgumentException("nonconsecutive publication");
        long[] pages=Arrays.stream(written).filter(p->p>=2).distinct().sorted().toArray();
        history.addLast(new Commit(txid,digest(meta),pages));current=txid;
        while(history.size()>256){Commit removed=history.removeFirst();if(Long.compareUnsigned(removed.txid(),oldest)>0)oldest=removed.txid();}
    }
    public synchronized Invalidation invalidation(byte[] clientIncarnation,long last){
        if(!Arrays.equals(incarnation,clientIncarnation)||last==0||Long.compareUnsigned(last,current)>0||Long.compareUnsigned(last,oldest)<0)return new Invalidation(true,new long[0]);
        TreeSet<Long> pages=new TreeSet<>();
        for(Commit commit:history)if(Long.compareUnsigned(commit.txid(),last)>0){for(long page:commit.pages()){pages.add(page);if(pages.size()>PageProtocol.MAX_INVALIDATION)return new Invalidation(true,new long[0]);}}
        return new Invalidation(false,pages.stream().mapToLong(Long::longValue).toArray());
    }
    public synchronized int status(long txid,long digest){for(Commit commit:history)if(commit.txid()==txid)return commit.digest()==digest?0:1;return Long.compareUnsigned(txid,current)>0?1:2;}
    /** CRC-64/ECMA-182 with initial zero; encrypted resealing fields are excluded. */
    public static long digest(byte[] meta){
        boolean encrypted=meta.length>7&&meta[6]==1;long crc=0;
        for(int i=0;i<meta.length;i++){int value=encrypted&&(i>=32&&i<36||i>=meta.length-40)?0:meta[i]&255;crc^=(long)value<<56;
            for(int bit=0;bit<8;bit++)crc=(crc<<1)^((crc<0)?0x42f0e1eba9ea3693L:0);}
        return crc;
    }
}
