package org.jed;

import java.io.IOException;
import java.nio.channels.SocketChannel;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.atomic.AtomicLong;
import org.jed.host.PageConnection;
import org.jed.host.PageEndpoint;
import org.jed.host.PageProtocol;
import org.jed.storage.PageCommitDelta;
import org.jed.storage.PageFormat;
import org.jed.storage.PlaintextRecovery;
import static org.jed.host.PageProtocol.*;

/** Owner host. Remote writers and pins participate in the same publication gate as local sessions. */
final class PageService implements AutoCloseable {
    private final Database database;
    private final FileDatabase file;
    private final PageEndpoint endpoint;
    private final Set<Connection> connections=new HashSet<>();
    private final AtomicLong ids=new AtomicLong();
    private boolean closed, draining;
    private int admitted;
    private int acknowledgementFault;
    void interruptNextAcknowledgement(boolean crash) { acknowledgementFault = crash ? 2 : 1; }
    private Connection yieldRequester;
    PageService(Database database, FileDatabase file) throws IOException { this(database, file, false); }
    PageService(Database database, FileDatabase file, boolean injected) throws IOException {
        this.database = database; this.file = file; file.serviceWindows();
        endpoint = injected ? null : PageEndpoint.bind(file.ownership());
        if (endpoint != null) Thread.ofVirtual().name("jed-page-accept").start(this::accept);
    }
    void serve(PageTransport.Connection streams) {
        Connection connection = new Connection(streams, ids.incrementAndGet());
        synchronized (database) { if (closed) { try { streams.close(); } catch (IOException ignored) { } return; } connections.add(connection); }
        connection.run();
    }
    java.nio.file.Path endpoint(){return endpoint==null?null:endpoint.path();}
    private void accept(){
        while(true){try{SocketChannel channel=endpoint.server().accept();Connection connection=new Connection(channel,ids.incrementAndGet());
            synchronized(database){if(closed){channel.close();return;}connections.add(connection);}
            Thread.ofVirtual().name("jed-page-client").start(connection::run);
        }catch(IOException failure){synchronized(database){if(!closed)close();}return;}}
    }
    private final class Connection implements AutoCloseable {
        private final PageTransport.Connection channel;
        private final java.io.InputStream input;
        private final java.io.OutputStream output;
        private final Object sending=new Object();
        private final long id;
        private final Map<Long,Pin> pins=new HashMap<>();
        private boolean active=true,readOnly,waiting,writing,accepted,revoked,relaxed;
        private long lastFrame=System.nanoTime(), revokedTxid;
        private long writeTxid;
        private PlaintextRecovery.Meta writeMeta;
        private long[] granted=new long[0];
        private PageCommitDelta delta;
        private IOException rejected;
        private Thread gateWait;
        private final java.util.ArrayDeque<Frame> deferredBegins = new java.util.ArrayDeque<>();
        private record Pin(PlaintextRecovery.Meta meta,int count) { }
        Connection(SocketChannel channel,long id){this(new PageTransport.Connection(PageConnection.input(channel), PageConnection.output(channel), channel), id);}
        Connection(PageTransport.Connection channel,long id){this.channel=channel;this.id=id;input=channel.input();output=channel.output();}
        void run(){
            long request=0;
            try{
                Frame first=PageProtocol.read(input,0);if(first==null)return;request=first.requestId();
                if(!(first.message() instanceof Hello hello))throw new Violation("first message must be hello");
                if(hello.credential().kind()!=0)throw new Violation("local endpoint does not accept credentials");
                if(hello.minimum()>VERSION||hello.maximum()<VERSION||hello.format()!=PageFormat.VERSION){error(request,"0A000","unsupported page-service protocol or file format",true);return;}
                synchronized(database){if(draining) { error(request,"08006","owner is shutting down",true);return; }
                    if(database.fileOptions().maxConnections()!=0&&admitted>=database.fileOptions().maxConnections()){error(request,"53300","owner connection limit",true);return;}admitted++;accepted=true;}
                readOnly=(hello.flags()&1)!=0;
                Thread.ofVirtual().name("jed-page-idle").start(this::idle);
                send(request,new HelloOk(VERSION,PageFormat.VERSION,file.pageSize(),id,(readOnly?1:0)|(database.fileOptions().readOnly()?2:0)|(database.fileOptions().ownership()==FileOptions.Ownership.YIELD?4:0),maxReadPages(file.pageSize())));
                while(true){Frame frame=PageProtocol.read(input,file.pageSize());if(frame==null)return;request=frame.requestId();synchronized(database){lastFrame=System.nanoTime();}handle(frame);}
            }catch(Violation failure){try{error(request,"08P01",failure.getMessage(),true);}catch(IOException ignored){}}
            catch(IOException failure){/* EOF releases every pin and the gate. Storage failure closes the owner below. */}
            catch(SqlException failure){try{error(request,failure.sqlState(),failure.getMessage(),true);}catch(IOException ignored){}}
            finally{close();}
        }
        void handle(Frame frame)throws IOException{
            Message message=frame.message();long request=frame.requestId();
            synchronized(database){if(!active)throw new IOException("connection closed");
                if(draining && (message instanceof BeginRead || message instanceof BeginWrite)) { deferredBegins.addLast(frame);return; }
                if(revoked) {
                    if(message instanceof Abort){revoked=false;return;}
                    if(message instanceof CommitBegin || message instanceof CommitPages)return;
                    if(message instanceof CommitEnd || message instanceof BeginWrite || message instanceof ReadPages pages && pages.txid()==revokedTxid){revoked=false;error(request,"25P03","idle writer transaction revoked",false);return;}
                }
                if((delta!=null||rejected!=null)&&!(message instanceof CommitPages||message instanceof CommitEnd||message instanceof Abort))throw new Violation("interleaved commit delta");}
            switch(message){
                case BeginRead value -> {
                    ReadBegun reply;synchronized(database){var meta=file.serviceMeta();pin(meta);var invalid=file.serviceWindows().invalidation(value.incarnation(),value.lastTxid());reply=new ReadBegun(file.serviceIncarnation(),meta.txid(),invalid.all()?1:0,invalid.pages(),meta.encoded());}send(request,reply);
                }
                case ReadPages value -> {
                    synchronized(database){Pin pin=pins.get(value.txid());PlaintextRecovery.Meta pinned = pin != null ? pin.meta() : writing && value.txid() == writeTxid ? writeMeta : null;if(pinned==null||value.indices().length<1||value.indices().length>maxReadPages(file.pageSize()))throw new Violation("read requires a pinned snapshot and bounded page count");
                        // The frozen peer owners use the current committed high-water while requiring a live pin.
                        // Go post-commit leaf demotion can fault a new leaf after its writer pin has ended.
                        // See the documented frozen-baseline compatibility exception in the Java charter.
                        long limit = file.serviceMeta().pageCount();
                        for(long index:value.indices())if(index<2||index>=limit)throw new Violation("page outside committed high-water under held pin");}
                    var pages=new java.util.ArrayList<byte[]>(value.indices().length);for(long index:value.indices())pages.add(file.serviceRead(index));send(request,new Pages(pages));
                }
                case EndRead value -> {synchronized(database){unpin(value.txid());}}
                case BeginWrite value -> beginWrite(request,value);
                case CommitBegin value -> {
                    synchronized(database){requireWriter();try{delta=file.serviceBegin(value,granted);}catch(Violation failure){rejected=failure;}catch(IOException failure){close();PageService.this.close();throw failure;}}
                }
                case CommitPages value -> {
                    synchronized(database){requireWriter();if(rejected!=null)break;if(delta==null)throw new Violation("commit pages without begin");
                        try{for(var page:value.pages())file.serviceWrite(delta,page);}catch(Violation failure){rejected=failure;}catch(IOException failure){close();PageService.this.close();throw failure;}}
                }
                case CommitEnd value -> {
                    long txid;
                    synchronized(database){requireWriter();if(rejected!=null){IOException failure=rejected;releaseWrite();throw new Violation(failure.getMessage());}if(delta==null)throw new Violation("commit end without begin");
                        try{delta.complete();txid=file.serviceCommit(delta,relaxed);}catch(Violation|org.jed.storage.CorruptStorageException failure){releaseWrite();throw new Violation(failure.getMessage());}
                        catch(IOException failure){close();PageService.this.close();throw failure;}}
                    // Publication is irrevocable before the acknowledgement. Cleanup still releases on EOF.
                    try {
                        int fault; synchronized (database) { fault = acknowledgementFault; acknowledgementFault = 0; }
                        if (fault == 2) Runtime.getRuntime().halt(0);
                        if (fault == 1) channel.close(); else send(request,new Committed(txid));
                    }
                    finally { synchronized(database) {
                        if (writing) { file.serviceUnpin(writeTxid); writeMeta=null; writing=false; }
                        delta=null; rejected=null; granted=new long[0];
                        try { file.serviceMaintenance(); }
                        catch (SqlException unsupported) { /* Durable foreign SQL may be unavailable to this owner. */ }
                        catch (IOException failure) { PageService.this.close(); throw failure; }
                        finally { database.releaseServiceWriter(this); }
                    } }
                }
                case Abort value -> {synchronized(database){requireWriter();releaseWrite();}}
                case CommitStatus value -> {int status;synchronized(database){status=file.serviceWindows().status(value.txid(),value.digest());}send(request,new CommitStatusReply(status));}
                case RequestOwnership value -> requestOwnership(request);
                default -> throw new Violation("unexpected request message");
            }
        }
        private void requestOwnership(long request) throws IOException {
            boolean start = false; int status;
            synchronized (database) {
                if (yieldRequester == this) status = 0;
                else if (!draining && !closed && (database.fileOptions().ownership() == FileOptions.Ownership.YIELD || database.fileOptions().readOnly())) {
                    yieldRequester = this; draining = true; database.beginOwnerDrain(); status = 0; start = true;
                } else status = 1;
            }
            send(request, new OwnershipReply(status));
            if (start) Thread.ofVirtual().name("jed-page-yield").start(PageService.this::yieldOwnership);
        }
        private void idle() {
            while (true) {
                try { Thread.sleep(50); } catch (InterruptedException failure) { Thread.currentThread().interrupt(); return; }
                synchronized (database) {
                    if (!active) return;
                    long elapsed = (System.nanoTime() - lastFrame) / 1_000_000;
                    if (writing && database.fileOptions().idleTransactionMillis() != 0 && elapsed >= database.fileOptions().idleTransactionMillis()) {
                        revokedTxid = writeTxid; releaseWrite(); revoked = true;
                    }
                    if (!writing && !waiting && pins.isEmpty() && deferredBegins.isEmpty() && database.fileOptions().idleSessionMillis() != 0 && elapsed >= database.fileOptions().idleSessionMillis()) { close(); return; }
                }
            }
        }
        private void beginWrite(long request,BeginWrite value)throws IOException{
            if ((value.flags() & ~1) != 0) throw new Violation("unsupported begin_write flags");
            synchronized(database){if(writing||waiting)throw new Violation("duplicate writer acquisition");if(readOnly||database.fileOptions().readOnly()){error(request,"25006","read-only page connection",false);return;}waiting=true;
                gateWait=Thread.ofVirtual().name("jed-page-writer-wait").start(()->{
                    try{database.acquireServiceWriter(this,value.timeoutMillis(),()->active);WriteBegun reply;
                        synchronized(database){if(!active){database.releaseServiceWriter(this);return;}relaxed=(value.flags()&1)!=0;file.serviceBeginWrite(relaxed);waiting=false;writing=true;var meta=file.serviceMeta();writeTxid=meta.txid();writeMeta=meta;file.servicePin(writeTxid);
                            boolean reuse=file.serviceReuse();granted=reuse?file.serviceFree():new long[0];var invalid=file.serviceWindows().invalidation(value.incarnation(),value.lastTxid());
                            reply=new WriteBegun(file.serviceIncarnation(),meta.txid(),invalid.all()?1:0,invalid.pages(),reuse,meta.encoded(),file.servicePending(),file.serviceFloorFree());}
                        send(request,reply);
                    }catch(SqlException failure){synchronized(database){waiting=false;}try{error(request,failure.sqlState(),failure.getMessage(),false);}catch(IOException ignored){close();}}
                    catch(IOException failure){close();}
                });}
        }
        private void requireWriter()throws Violation{if(!writing)throw new Violation("write request without writer gate");}
        private void pin(PlaintextRecovery.Meta meta){Pin previous=pins.get(meta.txid());pins.put(meta.txid(),new Pin(meta,previous==null?1:previous.count()+1));file.servicePin(meta.txid());}
        private void unpin(long txid)throws Violation{Pin previous=pins.get(txid);if(previous==null)throw new Violation("unbalanced read pin");if(previous.count()==1)pins.remove(txid);else pins.put(txid,new Pin(previous.meta(),previous.count()-1));file.serviceUnpin(txid);}
        private void releaseWrite(){if(writing){file.serviceUnpin(writeTxid);writeMeta=null;writing=false;database.releaseServiceWriter(this);}delta=null;rejected=null;granted=new long[0];}
        private void send(long request,Message message)throws IOException{synchronized(sending){PageProtocol.write(output,new Frame(request,message),file.pageSize());}}
        private void error(long request,String state,String message,boolean fatal)throws IOException{send(request,new PageProtocol.Error(state,message,"",fatal?1:0));}
        public void close(){
            synchronized(database){if(!active)return;active=false;if(gateWait!=null)gateWait.interrupt();if(writing)releaseWrite();for(var pin:pins.entrySet())for(int i=0;i<pin.getValue().count();i++)file.serviceUnpin(pin.getKey());pins.clear();connections.remove(this);if(accepted){accepted=false;admitted--;}database.notifyAll();}
            try{channel.close();}catch(IOException ignored){}
        }
    }
    private void broadcast(Notice notice) {
        Connection[] active; synchronized (database) { active = connections.toArray(Connection[]::new); }
        for (Connection connection : active) Thread.ofVirtual().start(() -> { try { connection.send(0, notice); } catch (IOException ignored) { connection.close(); } });
    }
    private void yieldOwnership() {
        Connection requester; synchronized (database) { requester = yieldRequester; }
        // Complete the first notice before cancellation or EOF can overtake it. A slow reader
        // gets at most the configured grace; closing it also removes its pin from the drain.
        long started = System.nanoTime(); Connection[] notified;
        synchronized (database) { notified = connections.toArray(Connection[]::new); }
        var notices = new java.util.ArrayList<java.util.concurrent.CompletableFuture<Void>>();
        for (Connection connection : notified) {
            var sent = new java.util.concurrent.CompletableFuture<Void>(); notices.add(sent);
            Thread.ofVirtual().start(() -> { try { connection.send(0, new Notice(1, requester.id)); sent.complete(null); }
                catch (IOException failure) { connection.close(); sent.complete(null); } });
        }
        for (int i = 0; i < notices.size(); i++) {
            try {
                long grace = database.fileOptions().yieldGraceMillis();
                if (grace == 0) notices.get(i).get();
                else notices.get(i).get(Math.max(1, grace - (System.nanoTime() - started) / 1_000_000), java.util.concurrent.TimeUnit.MILLISECONDS);
            } catch (java.util.concurrent.TimeoutException failure) { notified[i].close(); }
            catch (InterruptedException failure) { Thread.currentThread().interrupt(); return; }
            catch (java.util.concurrent.ExecutionException impossible) { throw new AssertionError(impossible); }
        }
        synchronized (database) {
            while (!closed && requester.active) {
                boolean busy = database.ownerBusy() || connections.stream().anyMatch(c -> c.writing || !c.pins.isEmpty());
                long grace = database.fileOptions().yieldGraceMillis();
                if (!busy || grace != 0 && (System.nanoTime() - started) / 1_000_000 >= grace) break;
                try { database.wait(25); } catch (InterruptedException failure) { Thread.currentThread().interrupt(); return; }
            }
            if (closed) return;
            if (!requester.active) {
                yieldRequester = null; draining = false; database.cancelOwnerDrain(); broadcast(new Notice(2, 0));
                for (Connection connection : connections) {
                    var deferred = java.util.List.copyOf(connection.deferredBegins); connection.deferredBegins.clear();
                    if (!deferred.isEmpty()) Thread.ofVirtual().start(() -> { try { for (Frame frame : deferred) connection.handle(frame); } catch (IOException | RuntimeException failure) { connection.close(); } });
                }
                return;
            }
            try { database.yieldOwner(this, file); }
            catch (IOException failure) { close(); database.cancelOwnerDrain(); }
        }
    }
    void shutdown() {
        Connection[] active;
        synchronized (database) { if (closed) return; draining = true; active = connections.toArray(Connection[]::new); }
        for (Connection connection : active) Thread.ofVirtual().start(() -> { try { connection.send(0, new Notice(3, 0)); } catch (IOException ignored) { connection.close(); } });
        long started = System.nanoTime();
        synchronized (database) {
            while (!closed) {
                boolean busy = connections.stream().anyMatch(c -> c.writing || c.waiting || !c.pins.isEmpty());
                if (!busy || (System.nanoTime() - started) / 1_000_000 >= database.fileOptions().shutdownGraceMillis()) break;
                try { database.wait(25); } catch (InterruptedException failure) { Thread.currentThread().interrupt(); break; }
            }
        }
        close();
    }
    @Override public void close(){
        Connection[] active;synchronized(database){if(closed)return;closed=true;active=connections.toArray(Connection[]::new);}
        if(endpoint!=null)try{endpoint.close();}catch(IOException ignored){}
        for(Connection connection:active)connection.close();
    }
}
