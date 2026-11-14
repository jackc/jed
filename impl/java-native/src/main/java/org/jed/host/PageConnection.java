package org.jed.host;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.UnixDomainSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.SocketChannel;
import java.nio.file.Path;
import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.function.Consumer;
import static org.jed.host.PageProtocol.*;

/** Multiplexed host connection: a waiting writer never prevents an unrelated page read. */
public final class PageConnection implements AutoCloseable {
    public static final class ConnectionLost extends IOException {
        private static final long serialVersionUID = 1L;
        public ConnectionLost(IOException cause) { super(cause.getMessage(), cause); }
    }
    public static final class RemoteFailure extends IOException {
        private static final long serialVersionUID = 1L;
        private final String state, detail;
        private final boolean fatal;
        public RemoteFailure(PageProtocol.Error error) { super(error.message()); state=error.sqlstate(); detail=error.detail(); fatal=(error.flags()&1)!=0; }
        public String sqlState(){return state;} public String detail(){return detail;} public boolean fatal(){return fatal;}
    }
    /** commit_end reached the transport; only the owner's resolution window can decide the outcome. */
    public static final class CommitLost extends IOException {
        private static final long serialVersionUID = 1L;
        private final long txid, digest;
        CommitLost(long txid, long digest, IOException cause) { super("page commit acknowledgement lost", cause); this.txid=txid; this.digest=digest; }
        public long txid(){return txid;} public long digest(){return digest;}
    }
    /** Direct channel wrappers avoid Channels' shared blocking lock on duplex socket streams. */
    public static InputStream input(SocketChannel channel) {
        return new InputStream(){
            public int read()throws IOException{byte[] one=new byte[1];return read(one,0,1)<0?-1:one[0]&255;}
            public int read(byte[] bytes,int offset,int count)throws IOException{return channel.read(ByteBuffer.wrap(bytes,offset,count));}
            public void close()throws IOException{channel.close();}
        };
    }
    public static OutputStream output(SocketChannel channel) {
        return new OutputStream(){
            public void write(int value)throws IOException{write(new byte[]{(byte)value});}
            public void write(byte[] bytes,int offset,int count)throws IOException{ByteBuffer buffer=ByteBuffer.wrap(bytes,offset,count);while(buffer.hasRemaining())channel.write(buffer);}
            public void close()throws IOException{channel.close();}
        };
    }
    private final InputStream input;
    private final OutputStream output;
    private final AutoCloseable transport;
    private final HelloOk hello;
    private volatile Consumer<Notice> notice;
    private final Object sending=new Object();
    private final Map<Long,CompletableFuture<Message>> pending=new HashMap<>();
    private long nextId=2;
    private IOException closed;
    private long closedAt;

    public static PageConnection connect(Path endpoint,int format,boolean readOnly,Consumer<Notice> notice)throws IOException {
        return connect(endpoint, format, readOnly, notice, 5000);
    }
    public static PageConnection connect(Path endpoint,int format,boolean readOnly,Consumer<Notice> notice,long timeoutMillis)throws IOException {
        SocketChannel channel=SocketChannel.open(java.net.StandardProtocolFamily.UNIX);
        CompletableFuture<PageConnection> result=new CompletableFuture<>();
        Thread connector=Thread.ofVirtual().name("jed-page-connect").start(()->{
            try { channel.connect(UnixDomainSocketAddress.of(endpoint));
                result.complete(new PageConnection(input(channel),output(channel),channel,new Hello(1,1,format,readOnly?1:0,"jed-java",WireCredential.none()),notice)); }
            catch(IOException|RuntimeException|java.lang.Error failure){try{channel.close();}catch(IOException cleanup){failure.addSuppressed(cleanup);}result.completeExceptionally(failure);}
        });
        try { return result.get(Math.max(1,timeoutMillis),java.util.concurrent.TimeUnit.MILLISECONDS); }
        catch(InterruptedException failure){Thread.currentThread().interrupt();channel.close();connector.interrupt();throw new IOException("page-service connect interrupted",failure);}
        catch(java.util.concurrent.TimeoutException failure){channel.close();connector.interrupt();throw new IOException("page-service hello timeout",failure);}
        catch(ExecutionException failure){if(failure.getCause() instanceof IOException io)throw io;if(failure.getCause() instanceof RuntimeException runtime)throw runtime;if(failure.getCause() instanceof java.lang.Error error)throw error;throw new IOException("page-service connect failed",failure.getCause());}
    }
    public PageConnection(InputStream input,OutputStream output,AutoCloseable transport,Hello request,Consumer<Notice> notice)throws IOException {
        this.input=input;this.output=output;this.transport=transport;this.notice=notice;
        PageProtocol.write(output,new Frame(1,request),0);
        Frame reply=PageProtocol.read(input,0);
        if(reply==null)throw new IOException("owner closed during hello");
        if(reply.requestId()!=1)throw new Violation("unexpected hello request id");
        if(reply.message() instanceof PageProtocol.Error error)throw new RemoteFailure(error);
        if(!(reply.message() instanceof HelloOk accepted)||accepted.protocol()!=VERSION||accepted.format()!=request.format()
                ||accepted.connection()==0||accepted.maxReadPages()<1||accepted.maxReadPages()>maxReadPages(accepted.pageSize()))throw new Violation("invalid hello reply");
        hello=accepted;
        Thread.ofVirtual().name("jed-page-replies").start(this::receive);
    }
    public HelloOk hello(){return hello;}
    public boolean connected(){synchronized(pending){return closed==null;}}
    public long closedAt(){synchronized(pending){return closedAt;}}
    public void onNotice(Consumer<Notice> handler){notice=handler;}
    public Message request(Message message)throws IOException {
        CompletableFuture<Message> result=new CompletableFuture<>();
        synchronized(sending){long id=allocate();synchronized(pending){pending.put(id,result);}try{PageProtocol.write(output,new Frame(id,message),hello.pageSize());}catch(IOException failure){fail(failure);throw classified(failure);}}
        try{return result.get();}
        catch(InterruptedException failure){Thread.currentThread().interrupt();IOException io=new IOException("page request interrupted",failure);fail(io);throw io;}
        catch(ExecutionException failure){if(failure.getCause() instanceof IOException io)throw io;throw new IOException("page request failed",failure.getCause());}
    }
    public void send(Message message)throws IOException {
        synchronized(sending){long id=allocate();try{PageProtocol.write(output,new Frame(id,message),hello.pageSize());}catch(IOException failure){fail(failure);throw classified(failure);}}
    }
    /** Serializes an entire commit, keeping other requests outside the delta. */
    public Message commit(CommitBegin begin,Iterable<CommitPages> batches)throws IOException {
        CompletableFuture<Message> result=new CompletableFuture<>();
        synchronized(sending){
            send(begin);for(CommitPages pages:batches)send(pages);
            long id=allocate(); synchronized(pending){pending.put(id,result);}
            try { PageProtocol.write(output,new Frame(id,new CommitEnd()),hello.pageSize()); }
            catch(IOException failure){fail(failure);throw classified(failure);}
        }
        try { return result.get(); }
        catch(InterruptedException failure){Thread.currentThread().interrupt();IOException lost=new IOException("page commit wait interrupted",failure);fail(lost);throw new CommitLost(begin.txid(),PageWindows.digest(begin.meta()),lost);}
        catch(ExecutionException failure){
            if(failure.getCause() instanceof RemoteFailure remote)throw remote;
            IOException lost=failure.getCause() instanceof IOException io?io:new IOException("page commit reply failed",failure.getCause());
            throw new CommitLost(begin.txid(),PageWindows.digest(begin.meta()),lost);
        }
    }
    private long allocate()throws IOException {
        synchronized(pending){if(closed!=null)throw closed;long first=nextId;do{long id=nextId;nextId=id==0xffffffffL?1:id+1;if(!pending.containsKey(id))return id;}while(nextId!=first);}
        throw new IOException("page request id space exhausted");
    }
    private void receive(){
        try{while(true){Frame frame=PageProtocol.read(input,hello.pageSize());if(frame==null)throw new IOException("page owner disconnected");
            if(frame.message() instanceof Notice value){if(notice!=null)notice.accept(value);continue;}
            CompletableFuture<Message> future;synchronized(pending){future=pending.remove(frame.requestId());}
            if(future==null)throw new Violation("unexpected page-service reply");
            if(frame.message() instanceof PageProtocol.Error error){RemoteFailure failure=new RemoteFailure(error);future.completeExceptionally(failure);if(failure.fatal())throw failure;}
            else future.complete(frame.message());
        }}catch(IOException failure){fail(failure);}catch(RuntimeException failure){fail(new IOException("page reply handler failed",failure));}
    }
    private static IOException classified(IOException failure) {
        return failure instanceof ConnectionLost || failure instanceof RemoteFailure || failure instanceof Violation ? failure : new ConnectionLost(failure);
    }
    private void fail(IOException failure){
        failure=classified(failure);
        synchronized(pending){if(closed!=null)return;closed=failure;closedAt=System.nanoTime();for(var result:pending.values())result.completeExceptionally(failure);pending.clear();}
        try{transport.close();}catch(Exception cleanup){failure.addSuppressed(cleanup);}
    }
    public void close(){fail(new IOException("page connection closed"));}
}
