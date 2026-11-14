package org.jed.storage;

import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.function.LongPredicate;
import java.util.zip.CRC32;
import org.jed.host.PageProtocol;
import static org.jed.storage.PageFormat.*;

/** Streaming protocol checks A/B/C. Only descriptors and arrival bits remain resident, never page bodies. */
public final class PageCommitDelta {
    private final PlaintextRecovery.Meta meta;
    private final Map<Long,Long> expected=new HashMap<>();
    private final Set<Long> received=new HashSet<>();
    private final Set<Long> dependencies=new HashSet<>();
    private final List<ManifestEncoder.Write> manifests;
    public PageCommitDelta(PlaintextRecovery.Meta before,PageProtocol.CommitBegin begin,LongPredicate granted)throws IOException{
        this(before, begin, granted, List.of());
    }
    public PageCommitDelta(PlaintextRecovery.Meta before,PageProtocol.CommitBegin begin,LongPredicate granted,List<ManifestEncoder.Entry> pending)throws IOException{
        Map<Long,Long> retained = new HashMap<>(); for(var entry:pending) retained.put(entry.page(),entry.crc());
        try{meta=PlaintextRecovery.parseMeta(begin.meta());}catch(CorruptStorageException failure){throw violation(failure.getMessage());}
        if(before.txid()==-1L||begin.txid()!=before.txid()+1||meta.txid()!=begin.txid()||!meta.layout().equals(before.layout())
                ||meta.pageCount()<before.pageCount()||meta.manifestEntries()==0)throw violation("invalid client commit generation");
        LongPredicate legal=id->id>=meta.layout().firstBody()&&id<meta.pageCount()&&(retained.containsKey(id)||id>=before.pageCount()||granted.test(id));
        int inline=(int)Math.min(meta.manifestEntries(),meta.layout().inlineManifestCapacity());
        long previous=entries(meta.encoded(),meta.layout().inlineManifestStart(),inline,0,legal);
        if(begin.manifests().size()!=meta.manifestPages())throw violation("manifest chain length");
        long next=meta.manifestHead();CRC32 chain=new CRC32();byte[] index=new byte[4];List<ManifestEncoder.Write> pages=new ArrayList<>();
        for(int ordinal=0;ordinal<begin.manifests().size();ordinal++){
            var page=begin.manifests().get(ordinal);long id=page.index();byte[] bytes=page.bytes();
            if(id!=next||retained.containsKey(id)||!legal.test(id)||!dependencies.add(id)||expected.containsKey(id))throw violation("illegal manifest chain page");
            BodyHeader header;try{header=parseBody(bytes);}catch(CorruptStorageException failure){throw violation(failure.getMessage());}
            int count=(int)Math.min(meta.manifestEntries()-expected.size(),meta.layout().overflowManifestCapacity());
            if(header.type()!=8||header.stamp()!=(int)(meta.txid()&65535)||header.count()!=count||u64(bytes,16)!=meta.txid()
                    ||u32(bytes,24)!=ordinal||u32(bytes,28)!=0||!zeros(bytes,32+count*8,bytes.length))throw violation("invalid manifest page header");
            put32(index,0,id);chain.update(index);chain.update(bytes);
            previous=entries(bytes,32,count,previous,legal);next=header.next();pages.add(new ManifestEncoder.Write(id,bytes));
        }
        if(next!=0||expected.size()!=meta.manifestEntries()||chain.getValue()!=meta.manifestCrc()
                ||!expected.containsKey(meta.root())||meta.freeListHead()!=0&&!expected.containsKey(meta.freeListHead()))throw violation("incomplete commit manifest");
        for(long id:expected.keySet())if(!dependencies.add(id))throw violation("manifest/body overlap");
        for(var entry:retained.entrySet()) { if(!entry.getValue().equals(expected.get(entry.getKey())))throw violation("pending manifest identity changed");received.add(entry.getKey()); }
        manifests=List.copyOf(pages);
    }
    private long entries(byte[] bytes,int offset,int count,long previous,LongPredicate legal)throws PageProtocol.Violation{
        for(int i=0;i<count;i++,offset+=8){long id=u32(bytes,offset);if(id<=previous||!legal.test(id)||expected.putIfAbsent(id,u32(bytes,offset+4))!=null)throw violation("illegal or unordered body manifest index " + id + " after " + previous + " at high-water " + meta.pageCount());previous=id;}return previous;
    }
    public PlaintextRecovery.Meta meta(){return meta;}public List<ManifestEncoder.Write> manifests(){return manifests;}
    public Set<Long> dependencies(){return Set.copyOf(dependencies);}
    public List<ManifestEncoder.Entry> identities(){return expected.entrySet().stream().sorted(Map.Entry.comparingByKey()).map(e->new ManifestEncoder.Entry(e.getKey(),e.getValue())).toList();}
    public void accept(long index,byte[] bytes)throws IOException{
        Long digest=expected.get(index);if(digest==null||received.contains(index)||bytes.length!=meta.pageSize())throw violation("unlisted or duplicate body page");
        if(meta.encrypted()){CRC32 crc=new CRC32();crc.update(bytes);if(crc.getValue()!=digest)throw violation("ciphertext manifest identity");}
        else{BodyHeader header;try{header=parseBody(bytes);}catch(CorruptStorageException failure){throw violation(failure.getMessage());}
            if(header.type()>7||header.stamp()!=(int)(meta.txid()&65535)||u32(bytes,12)!=digest)throw violation("body manifest identity");}
        received.add(index);
    }
    public void complete()throws PageProtocol.Violation{if(received.size()!=expected.size())throw violation("missing commit body pages");}
    private static PageProtocol.Violation violation(String message){return new PageProtocol.Violation(message);}
}
