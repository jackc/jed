package org.jed.host;

import java.io.ByteArrayOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.ByteBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.List;
import java.util.ArrayList;
import java.util.zip.CRC32;
import org.jed.storage.PageFormat;

/** Handwritten protocol-v1 framing and bodies. No database values cross this host seam. */
public final class PageProtocol {
    public static final int VERSION = 1, MAX_FRAME = 16_777_216, MAX_INVALIDATION = 65_536;
    private PageProtocol() { }
    public static final class Violation extends IOException {
        private static final long serialVersionUID = 1L;
        public Violation(String message) { super(message); }
    }
    public sealed interface Message { }
    public record Hello(int minimum, int maximum, int format, int flags, String client, WireCredential credential) implements Message { }
    public record HelloOk(int protocol, int format, int pageSize, long connection, int flags, int maxReadPages) implements Message { }
    public record BeginRead(byte[] incarnation, long lastTxid) implements Message { }
    public record ReadBegun(byte[] incarnation, long txid, int invalidate, long[] indices, byte[] meta) implements Message { }
    public record ReadPages(long txid, long[] indices) implements Message { }
    public record Pages(List<byte[]> pages) implements Message { }
    public record EndRead(long txid) implements Message { }
    public record BeginWrite(byte[] incarnation, long lastTxid, long timeoutMillis, int flags) implements Message { }
    public record WriteBegun(byte[] incarnation, long txid, int invalidate, long[] indices, boolean reuseGrant,
                             byte[] meta, List<Digest> pending, long[] reuse) implements Message { }
    public record CommitBegin(long txid, byte[] meta, List<IndexedPage> manifests) implements Message { }
    public record CommitPages(List<IndexedPage> pages) implements Message { }
    public record CommitEnd() implements Message { }
    public record Abort() implements Message { }
    public record Committed(long txid) implements Message { }
    public record CommitStatus(long txid, long digest) implements Message { }
    public record CommitStatusReply(int status) implements Message { }
    public record RequestOwnership() implements Message { }
    public record OwnershipReply(int status) implements Message { }
    public record Notice(int kind, long connection) implements Message { }
    public record Error(String sqlstate, String message, String detail, int flags) implements Message { }
    public record IndexedPage(long index, byte[] bytes) { }
    public record Digest(long index, long digest) { }
    public record Frame(long requestId, Message message) { }
    public record WireCredential(int kind, String passphrase, byte[] key) {
        public static WireCredential none() { return new WireCredential(0, "", new byte[0]); }
        @Override public String toString() { return "WireCredential[redacted]"; }
    }
    public record OwnerRecord(int flags, long uid, byte[] incarnation, long pid, String endpoint) {
        public boolean serving() { return (flags & 1) != 0; }
    }
    public static int maxReadPages(int pageSize) {
        if (pageSize <= 0) throw new IllegalArgumentException("page size");
        return Math.min(1024, (MAX_FRAME - 9) / pageSize);
    }
    public static int code(Message message) {
        return switch (message) {
            case Hello m -> 1; case BeginRead m -> 2; case ReadPages m -> 3; case EndRead m -> 4;
            case BeginWrite m -> 5; case CommitBegin m -> 6; case CommitPages m -> 7;
            case CommitEnd m -> 8; case Abort m -> 9; case CommitStatus m -> 10; case RequestOwnership m -> 11;
            case HelloOk m -> 0x41; case ReadBegun m -> 0x42; case Pages m -> 0x43; case WriteBegun m -> 0x45;
            case Committed m -> 0x48; case CommitStatusReply m -> 0x4a; case OwnershipReply m -> 0x4b;
            case Notice m -> 0x7e; case Error m -> 0x7f;
        };
    }
    public static byte[] encode(Frame frame, int pageSize) throws Violation {
        Writer out = new Writer(pageSize); out.u32(0); out.u8(code(frame.message())); out.u32(frame.requestId());
        if (frame.requestId() == 0 && !(frame.message() instanceof Notice || frame.message() instanceof Error)
            || frame.requestId() != 0 && frame.message() instanceof Notice) throw new Violation("invalid request id");
        switch (frame.message()) {
            case Hello m -> { out.u16(m.minimum()); out.u16(m.maximum()); out.u16(m.format()); out.u8(m.flags()); out.string(m.client()); out.credential(m.credential()); }
            case HelloOk m -> { out.u16(m.protocol()); out.u16(m.format()); out.u32(m.pageSize()); out.u64(m.connection()); out.u8(m.flags()); out.u32(m.maxReadPages()); }
            case BeginRead m -> { out.fixed(m.incarnation(),16); out.u64(m.lastTxid()); }
            case ReadBegun m -> { out.fixed(m.incarnation(),16); out.u64(m.txid()); out.u8(m.invalidate()); out.indices(m.indices()); out.page(m.meta()); }
            case ReadPages m -> { out.u64(m.txid()); out.indices(m.indices()); }
            case Pages m -> { out.u32(m.pages().size()); for (byte[] page : m.pages()) out.page(page); }
            case EndRead m -> out.u64(m.txid());
            case BeginWrite m -> { out.fixed(m.incarnation(),16); out.u64(m.lastTxid()); out.u64(m.timeoutMillis()); out.u8(m.flags()); }
            case WriteBegun m -> { out.fixed(m.incarnation(),16); out.u64(m.txid()); out.u8(m.invalidate()); out.indices(m.indices()); out.u8(m.reuseGrant()?1:0); out.page(m.meta()); out.u32(m.pending().size()); for (Digest d : m.pending()) { out.u32(d.index()); out.u64(d.digest()); } out.indices(m.reuse()); }
            case CommitBegin m -> { out.u64(m.txid()); out.page(m.meta()); out.indexed(m.manifests()); }
            case CommitPages m -> out.indexed(m.pages());
            case CommitEnd m -> { } case Abort m -> { } case RequestOwnership m -> { }
            case Committed m -> out.u64(m.txid());
            case CommitStatus m -> { out.u64(m.txid()); out.u64(m.digest()); }
            case CommitStatusReply m -> out.u8(m.status()); case OwnershipReply m -> out.u8(m.status());
            case Notice m -> { out.u8(m.kind()); out.u64(m.connection()); }
            case Error m -> { out.sqlstate(m.sqlstate()); out.string(m.message()); out.string(m.detail()); out.u8(m.flags()); }
        }
        byte[] bytes = out.bytes(); PageFormat.put32(bytes,0,bytes.length-4); return bytes;
    }
    public static Frame decode(byte[] bytes, int pageSize) throws Violation {
        Reader in = new Reader(bytes,pageSize); long length = in.u32();
        if (length < 5 || length > MAX_FRAME || length != bytes.length-4L) throw new Violation("invalid frame length");
        int code = in.u8(); long id = in.u32();
        if (id == 0 && code != 0x7e && code != 0x7f || id != 0 && code == 0x7e) throw new Violation("invalid request id");
        Message message = switch (code) {
            case 1 -> new Hello(in.u16(),in.u16(),in.u16(),in.u8(),in.string(),in.credential());
            case 2 -> new BeginRead(in.fixed(16),in.u64());
            case 3 -> new ReadPages(in.u64(),in.indices());
            case 4 -> new EndRead(in.u64());
            case 5 -> new BeginWrite(in.fixed(16),in.u64(),in.u64(),in.u8());
            case 6 -> new CommitBegin(in.u64(),in.page(),in.indexed());
            case 7 -> new CommitPages(in.indexed());
            case 8 -> new CommitEnd(); case 9 -> new Abort();
            case 10 -> new CommitStatus(in.u64(),in.u64()); case 11 -> new RequestOwnership();
            case 0x41 -> new HelloOk(in.u16(),in.u16(),in.pageSizeField(),in.u64(),in.u8(),in.positiveInt());
            case 0x42 -> new ReadBegun(in.fixed(16),in.u64(),in.u8(),in.indices(),in.page());
            case 0x43 -> new Pages(in.pages());
            case 0x45 -> new WriteBegun(in.fixed(16),in.u64(),in.u8(),in.indices(),in.bool(),in.page(),in.digests(),in.indices());
            case 0x48 -> new Committed(in.u64()); case 0x4a -> new CommitStatusReply(in.u8());
            case 0x4b -> new OwnershipReply(in.u8()); case 0x7e -> new Notice(in.u8(),in.u64());
            case 0x7f -> new Error(in.sqlstate(),in.string(),in.string(),in.u8());
            default -> throw new Violation("unknown or reserved message code");
        };
        in.end(); return new Frame(id,message);
    }
    /** Returns null only on a clean boundary EOF; never allocates an unchecked body length. */
    public static Frame read(InputStream stream, int pageSize) throws IOException {
        int first = stream.read(); if (first < 0) return null;
        byte[] prefix = new byte[4]; prefix[0]=(byte)first; readFully(stream,prefix,1,3);
        long length = PageFormat.u32(prefix,0);
        if (length < 5 || length > MAX_FRAME) throw new Violation("invalid frame length");
        byte[] bytes = new byte[(int)length+4]; System.arraycopy(prefix,0,bytes,0,4);
        readFully(stream,bytes,4,(int)length); return decode(bytes,pageSize);
    }
    private static void readFully(InputStream stream, byte[] target, int offset, int length) throws IOException {
        int end=offset+length;
        while (offset<end) { int n=stream.read(target,offset,end-offset); if(n<0)throw new EOFException("truncated page-service frame"); if(n==0) { int b=stream.read(); if(b<0)throw new EOFException("truncated page-service frame"); target[offset++]=(byte)b; } else offset+=n; }
    }
    public static void write(OutputStream stream, Frame frame, int pageSize) throws IOException { stream.write(encode(frame,pageSize)); stream.flush(); }
    public static byte[] encodeOwner(OwnerRecord owner) throws Violation {
        if ((owner.flags() & ~1) != 0) throw new Violation("owner flags");
        Writer out=new Writer(0); out.fixed("JEDOWNER".getBytes(StandardCharsets.US_ASCII),8); out.u16(1); out.u8(owner.flags());
        out.u32(owner.uid()); out.fixed(owner.incarnation(),16); out.u32(owner.pid()); out.string(owner.endpoint());
        byte[] bytes=out.bytes(); CRC32 crc=new CRC32(); crc.update(bytes); out.u32(crc.getValue()); return out.bytes();
    }
    public static OwnerRecord decodeOwner(byte[] bytes) throws Violation {
        Reader in=new Reader(bytes,0);
        if(!Arrays.equals(in.fixed(8),"JEDOWNER".getBytes(StandardCharsets.US_ASCII)) || in.u16()!=1) throw new Violation("owner magic/version");
        int flags=in.u8(); if((flags & ~1)!=0)throw new Violation("owner flags");
        OwnerRecord owner=new OwnerRecord(flags,in.u32(),in.fixed(16),in.u32(),in.string());
        long expected=in.u32(); in.end(); CRC32 crc=new CRC32(); crc.update(bytes,0,bytes.length-4);
        if(crc.getValue()!=expected)throw new Violation("owner checksum"); return owner;
    }
    private static final class Reader {
        final byte[] bytes; final int pageSize; int offset;
        Reader(byte[] bytes,int pageSize) { this.bytes=bytes; this.pageSize=pageSize; }
        void need(int n) throws Violation { if(n<0 || n>bytes.length-offset)throw new Violation("truncated field"); }
        int u8() throws Violation { need(1); return bytes[offset++] & 255; }
        int u16() throws Violation { need(2); int n=PageFormat.u16(bytes,offset); offset+=2; return n; }
        long u32() throws Violation { need(4); long n=PageFormat.u32(bytes,offset); offset+=4; return n; }
        long u64() throws Violation { need(8); long n=PageFormat.u64(bytes,offset); offset+=8; return n; }
        int positiveInt() throws Violation { long n=u32(); if(n>Integer.MAX_VALUE)throw new Violation("integer out of range"); return(int)n; }
        int pageSizeField() throws Violation { int n=positiveInt(); if(!PageFormat.validPageSize(n))throw new Violation("invalid page size"); return n; }
        boolean bool() throws Violation { int n=u8(); if(n>1)throw new Violation("invalid boolean"); return n==1; }
        byte[] fixed(int n) throws Violation { need(n); byte[] result=Arrays.copyOfRange(bytes,offset,offset+n); offset+=n; return result; }
        int count(int width) throws Violation { long count=u32(); if(width<=0 || count>(bytes.length-offset)/width)throw new Violation("list count exceeds body"); return(int)count; }
        String string() throws Violation { int length=count(1); byte[] value=fixed(length); try { return StandardCharsets.UTF_8.newDecoder().onMalformedInput(CodingErrorAction.REPORT).onUnmappableCharacter(CodingErrorAction.REPORT).decode(ByteBuffer.wrap(value)).toString(); }catch(CharacterCodingException e){throw new Violation("invalid UTF-8");} }
        String sqlstate() throws Violation { byte[] value=fixed(5); for(byte b:value)if(!(b>='0'&&b<='9'||b>='A'&&b<='Z'))throw new Violation("invalid SQLSTATE"); return new String(value,StandardCharsets.US_ASCII); }
        long[] indices() throws Violation { int n=count(4); long[] result=new long[n]; for(int i=0;i<n;i++)result[i]=u32(); return result; }
        byte[] page() throws Violation { if(pageSize<=0)throw new Violation("page size not negotiated"); return fixed(pageSize); }
        List<byte[]> pages() throws Violation { int n=count(pageSize); List<byte[]> result=new ArrayList<>(n); for(int i=0;i<n;i++)result.add(page()); return List.copyOf(result); }
        List<IndexedPage> indexed() throws Violation { if(pageSize<=0)throw new Violation("page size not negotiated"); int n=count(pageSize+4); List<IndexedPage> result=new ArrayList<>(n); for(int i=0;i<n;i++)result.add(new IndexedPage(u32(),page())); return List.copyOf(result); }
        List<Digest> digests() throws Violation { int n=count(12); List<Digest> result=new ArrayList<>(n); for(int i=0;i<n;i++)result.add(new Digest(u32(),u64())); return List.copyOf(result); }
        WireCredential credential() throws Violation { return switch(u8()){case 0->WireCredential.none();case 1->new WireCredential(1,string(),new byte[0]);case 2->new WireCredential(2,"",fixed(32));default->throw new Violation("invalid credential kind");}; }
        void end() throws Violation { if(offset!=bytes.length)throw new Violation("trailing bytes"); }
    }
    private static final class Writer {
        final ByteArrayOutputStream out=new ByteArrayOutputStream(); final int pageSize;
        Writer(int pageSize){this.pageSize=pageSize;}
        void room(int n)throws Violation{if(n<0||n>MAX_FRAME+4-out.size())throw new Violation("frame exceeds limit");}
        void u8(int value)throws Violation{if(value<0||value>255)throw new Violation("u8 range");room(1);out.write(value);}
        void u16(int value)throws Violation{if(value<0||value>65535)throw new Violation("u16 range");u8(value>>>8);u8(value&255);}
        void u32(long value)throws Violation{if(value<0||value>0xffffffffL)throw new Violation("u32 range");room(4);for(int i=3;i>=0;i--)out.write((int)(value>>>(i*8))&255);}
        void u64(long value)throws Violation{room(8);for(int i=7;i>=0;i--)out.write((int)(value>>>(i*8))&255);}
        void fixed(byte[] value,int n)throws Violation{if(value.length!=n)throw new Violation("fixed field length");room(n);out.writeBytes(value);}
        void string(String value)throws Violation{try{ByteBuffer bytes=StandardCharsets.UTF_8.newEncoder().onMalformedInput(CodingErrorAction.REPORT).onUnmappableCharacter(CodingErrorAction.REPORT).encode(java.nio.CharBuffer.wrap(value));u32(bytes.remaining());room(bytes.remaining());out.writeBytes(Arrays.copyOfRange(bytes.array(),bytes.position(),bytes.limit()));}catch(CharacterCodingException e){throw new Violation("invalid UTF-16");}}
        void sqlstate(String value)throws Violation{if(!value.matches("[0-9A-Z]{5}"))throw new Violation("invalid SQLSTATE");fixed(value.getBytes(StandardCharsets.US_ASCII),5);}
        void indices(long[] values)throws Violation{u32(values.length);for(long value:values)u32(value);}
        void page(byte[] value)throws Violation{if(pageSize<=0)throw new Violation("page size not negotiated");fixed(value,pageSize);}
        void indexed(List<IndexedPage> values)throws Violation{u32(values.size());for(IndexedPage value:values){u32(value.index());page(value.bytes());}}
        void credential(WireCredential value)throws Violation{u8(value.kind());switch(value.kind()){case 0->{}case 1->string(value.passphrase());case 2->fixed(value.key(),32);default->throw new Violation("invalid credential kind");}}
        byte[] bytes(){return out.toByteArray();}
    }
}
