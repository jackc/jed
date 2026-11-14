package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import org.jed.host.PageConnection;
import org.jed.host.PageProtocol;
import org.jed.host.PageWindows;
import org.jed.storage.*;
import static org.jed.host.PageProtocol.*;

/** Real Unix-socket host pins, multiplexing, streaming publication and deferred SQL adoption. */
public final class PageServiceTest {
    public static void main(String[] args)throws Exception{
        Path directory=Files.createTempDirectory("jed-page-service-");Path path=directory.resolve("db.jed");
        try(Database database=Database.createExclusive(path,256,512);Session session=database.session()){
            Path endpoint=database.startPageService();check(endpoint!=null,"serving endpoint");
            try(PageConnection client=PageConnection.connect(endpoint,53,false,n->{})){
                ReadBegun first=(ReadBegun)client.request(new BeginRead(new byte[16],0));var firstMeta=PlaintextRecovery.parseMeta(first.meta());
                check(first.invalidate()==1,"cold invalidation");byte[] catalog=((Pages)client.request(new ReadPages(first.txid(),new long[]{firstMeta.root()}))).pages().getFirst();
                check(PageFormat.parseBody(catalog).type()==1,"raw page reply");
                execute(session,"BEGIN");
                var write=CompletableFuture.supplyAsync(()->{try{return (WriteBegun)client.request(new BeginWrite(first.incarnation(),first.txid(),0,0));}catch(Exception e){throw new RuntimeException(e);}});
                check(((Pages)client.request(new ReadPages(first.txid(),new long[]{firstMeta.root()}))).pages().size()==1,"page read while writer waits");
                check(!write.isDone(),"writer did not bypass local gate");execute(session,"ROLLBACK");WriteBegun begun=write.get(5,TimeUnit.SECONDS);
                var before=PlaintextRecovery.parseMeta(begun.meta());long txid=before.txid()+1,root=before.pageCount();
                byte[] body=new byte[256];body[0]=1;PageFormat.put16(body,2,(int)txid);PageFormat.put32(body,12,PageFormat.bodyCrc(body));
                byte[] meta=ManifestEncoder.encode(256,txid,root,root+1,0,1,List.of(new ManifestEncoder.Entry(root,PageFormat.bodyCrc(body))),new long[0],List.of()).meta();
                var reply=(Committed)client.commit(new CommitBegin(txid,meta,List.of()),List.of(new CommitPages(List.of(new IndexedPage(root,body)))));
                check(reply.txid()==txid,"remote publication");
                check(((CommitStatusReply)client.request(new CommitStatus(txid,PageWindows.digest(meta)))).status()==0,"commit resolution");
                check(Arrays.equals(((Pages)client.request(new ReadPages(first.txid(),new long[]{firstMeta.root()}))).pages().getFirst(),catalog),"old pin retained body");
                check(Arrays.equals(((Pages)client.request(new ReadPages(first.txid(),new long[]{root}))).pages().getFirst(),body),"held old pin permits current committed pages");
                ReadBegun current=(ReadBegun)client.request(new BeginRead(first.incarnation(),first.txid()));check(current.txid()==txid&&current.invalidate()==0&&Arrays.equals(current.indices(),new long[]{root}),"exact invalidation");
                client.send(new EndRead(first.txid()));client.send(new EndRead(current.txid()));
                execute(session,"CREATE TABLE adopted(id i32 PRIMARY KEY)");
                ReadBegun adopted=(ReadBegun)client.request(new BeginRead(current.incarnation(),current.txid()));check(Long.compareUnsigned(adopted.txid(),txid)>0,"owner adopted client before local commit");client.send(new EndRead(adopted.txid()));
                // EOF drops the writer gate and every pin, allowing local progress.
                client.request(new BeginWrite(adopted.incarnation(),adopted.txid(),0,0));
            }
            execute(session,"INSERT INTO adopted VALUES (7)");
            try (PageConnection invalid = PageConnection.connect(endpoint, 53, false, n -> { })) {
                expectProtocolFailure(invalid, new ReadPages(1, new long[]{2}));
            }
            try (PageConnection invalid = PageConnection.connect(endpoint, 53, false, n -> { })) {
                ReadBegun held = (ReadBegun) invalid.request(new BeginRead(new byte[16], 0));
                expectProtocolFailure(invalid, new ReadPages(held.txid(), new long[]{PlaintextRecovery.parseMeta(held.meta()).pageCount()}));
            }
            try(PageConnection readOnly=PageConnection.connect(endpoint,53,true,n->{})){
                try{readOnly.request(new BeginWrite(new byte[16],0,0,0));throw new AssertionError("read-only writer accepted");}
                catch(PageConnection.RemoteFailure failure){check(failure.sqlState().equals("25006")&&!failure.fatal(),"read-only classification");}
                ReadBegun pin=(ReadBegun)readOnly.request(new BeginRead(new byte[16],0));readOnly.send(new EndRead(pin.txid()));
            }
        }finally{try(var files=Files.walk(directory)){for(Path file:files.sorted(java.util.Comparator.reverseOrder()).toList())Files.deleteIfExists(file);}}
        clientSql(false); clientSql(true); lifecycleLimits(); yieldCancellation();
        windows();
        System.out.println("PageServiceTest: passed Unix socket pins, writer multiplexing, streamed commit, exact invalidation, resolution, adoption and EOF cleanup");
    }
    private static void expectProtocolFailure(PageConnection client, Message message) throws Exception {
        try { client.request(message); throw new AssertionError("invalid page read accepted"); }
        catch (PageConnection.RemoteFailure failure) { check(failure.sqlState().equals("08P01") && failure.fatal(), "invalid page read classification"); }
    }
    private static void yieldCancellation() throws Exception {
        Path directory = Files.createTempDirectory("jed-page-cancel-yield-"); Path path = directory.resolve("db.jed");
        try (Database owner = Database.create(path, FileOptions.builder().ownership(FileOptions.Ownership.YIELD).yieldGraceMillis(5000).shutdownGraceMillis(0).build()); Session pinned = owner.session()) {
            execute(pinned, "BEGIN READ ONLY");
            Path endpoint = owner.startPageService();
            var noticed = new java.util.concurrent.atomic.AtomicInteger();
            try (PageConnection requester = PageConnection.connect(endpoint,53,false,n->{ }); PageConnection observer = PageConnection.connect(endpoint,53,false,n->noticed.set(n.kind()))) {
                check(((OwnershipReply)requester.request(new RequestOwnership())).status()==0,"yield accepted");
                var begin = CompletableFuture.supplyAsync(()->{try{return(ReadBegun)observer.request(new BeginRead(new byte[16],0));}catch(Exception failure){throw new RuntimeException(failure);}});
                Thread.sleep(50); check(!begin.isDone(),"yield leaves new begin unanswered"); requester.close();
                ReadBegun result = begin.get(3,TimeUnit.SECONDS);observer.send(new EndRead(result.txid()));
                long until=System.nanoTime()+TimeUnit.SECONDS.toNanos(3);while(noticed.get()!=2&&System.nanoTime()<until)Thread.sleep(5);
                check(noticed.get()==2,"requester EOF cancels yield");
            }
            execute(pinned,"ROLLBACK");try(Session session=owner.session()){execute(session,"CREATE TABLE after_cancel(id i32)");}
        } finally {try(var files=Files.walk(directory)){for(Path file:files.sorted(java.util.Comparator.reverseOrder()).toList())Files.deleteIfExists(file);}}
    }
    private static void lifecycleLimits() throws Exception {
        Path directory = Files.createTempDirectory("jed-page-limits-"); Path path = directory.resolve("db.jed");
        var options = FileOptions.builder().pageSize(256).maxConnections(1).idleTransactionMillis(75).shutdownGraceMillis(100).build();
        try {
            Database owner = Database.create(path, options); Path endpoint = owner.startPageService();
            try (PageConnection client = PageConnection.connect(endpoint, 53, false, n -> { }); Session session = owner.session()) {
                try { PageConnection.connect(endpoint, 53, false, n -> { }).close(); throw new AssertionError("connection limit ignored"); }
                catch (PageConnection.RemoteFailure failure) { check(failure.sqlState().equals("53300"), "connection limit classification"); }
                client.request(new BeginWrite(new byte[16], 0, 0, 0)); Thread.sleep(200);
                execute(session, "CREATE TABLE idle_release(id i32)");
                try { client.request(new CommitEnd()); throw new AssertionError("revoked writer accepted"); }
                catch (PageConnection.RemoteFailure failure) { check(failure.sqlState().equals("25P03") && !failure.fatal(), "revocation classification"); }
                ReadBegun pin = (ReadBegun) client.request(new BeginRead(new byte[16], 0));
                long started = System.nanoTime(); owner.close(); long elapsed = (System.nanoTime() - started) / 1_000_000;
                check(elapsed >= 75 && elapsed < 3000, "shutdown pin grace");
                try { client.request(new ReadPages(pin.txid(), new long[]{PlaintextRecovery.parseMeta(pin.meta()).root()})); throw new AssertionError("closed owner served a pin"); }
                catch (java.io.IOException expected) { }
            } finally { owner.close(); }
            try (Database readOnly = Database.open(path, FileOptions.builder().readOnly(true).shutdownGraceMillis(0).build()); Session session = readOnly.session()) {
                execute(session, "SELECT id FROM idle_release");
                try { execute(session, "INSERT INTO idle_release VALUES (1)"); throw new AssertionError("read-only owner wrote"); }
                catch (SqlException failure) { check(failure.sqlState().equals("25006"), "read-only owner classification"); }
            }
        } finally { try (var files = Files.walk(directory)) { for (Path file : files.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(file); } }
    }
    private static void clientSql(boolean encrypted) throws Exception {
        Path directory = Files.createTempDirectory("jed-page-client-"); Path path = directory.resolve("db.jed");
        Credential credential = encrypted ? Credential.hostKey(new byte[32]) : null;
        try (Database owner = Database.createExclusive(path, encrypted ? 1024 : 256, 1024, credential); Session local = owner.session()) {
            Path endpoint = owner.startPageService();
            try (Database client = Database.openPageClient(endpoint, 1024, credential, Environment.empty()); Session remote = client.session(); Session pinned = client.session()) {
                execute(remote, "CREATE TABLE shared(id i32 PRIMARY KEY, text_value text)");
                execute(remote, "INSERT INTO shared VALUES (1, 'first')");
                execute(pinned, "BEGIN READ ONLY"); check(text(pinned, "SELECT text_value FROM shared WHERE id = 1").equals("first"), "client initial snapshot");
                for (int i = 0; i < 12; i++) {
                    Session writer = i % 2 == 0 ? local : remote;
                    execute(writer, "UPDATE shared SET text_value = 'revision" + i + "' WHERE id = 1");
                    check(text(pinned, "SELECT text_value FROM shared WHERE id = 1").equals("first"), "client pinned version changed");
                    check(text(remote, "SELECT text_value FROM shared WHERE id = 1").equals("revision" + i), "client freshness");
                    check(text(local, "SELECT text_value FROM shared WHERE id = 1").equals("revision" + i), "owner adoption");
                }
                execute(pinned, "ROLLBACK");
                execute(remote, "BEGIN"); execute(remote, "INSERT INTO shared VALUES (2, 'second')"); execute(remote, "COMMIT");
                check(text(local, "SELECT text_value FROM shared WHERE id = 2").equals("second"), "explicit client writer");
                execute(remote, "BEGIN"); execute(remote, "UPDATE shared SET text_value = 'aborted' WHERE id = 2"); execute(remote, "ROLLBACK");
                check(text(local, "SELECT text_value FROM shared WHERE id = 2").equals("second"), "client rollback");
                execute(local, "BEGIN"); remote.setLockTimeoutMillis(50);
                try { execute(remote, "BEGIN"); throw new AssertionError("remote writer bypassed gate timeout"); }
                catch (SqlException failure) { check(failure.sqlState().equals("55P03"), "remote gate timeout classification"); }
                execute(local, "ROLLBACK"); execute(remote, "BEGIN"); execute(remote, "ROLLBACK");
                execute(local, "BEGIN");
                try (Session waiting = owner.session()) {
                    waiting.setLockTimeoutMillis(50);
                    try { execute(waiting, "BEGIN"); throw new AssertionError("local writer bypassed gate timeout"); }
                    catch (SqlException failure) { check(failure.sqlState().equals("55P03"), "local gate timeout classification"); }
                }
                execute(local, "ROLLBACK");
            }
        } finally { try (var files = Files.walk(directory)) { for (Path file : files.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(file); } }
    }
    private static String text(Session session, String sql) { try (Rows rows = session.query(sql)) { return rows.next().values().getFirst().asText(); } }
    private static void windows(){byte[] incarnation=new byte[16];incarnation[0]=1;byte[] meta=new byte[256];PageFormat.put64(meta,12,1);PageWindows window=new PageWindows(incarnation,meta,null);
        for(long txid=2;txid<=300;txid++){PageFormat.put64(meta,12,txid);window.published(meta,new long[]{2,txid+2});}
        check(window.invalidation(incarnation,1).all(),"bounded invalidation history");check(!window.invalidation(incarnation,299).all(),"recent history retained");check(window.status(1,0)==2&&window.status(301,0)==1,"bounded resolution history");
        check(PageWindows.digest("123456789".getBytes(java.nio.charset.StandardCharsets.US_ASCII))==0x6c40df5f0b497347L,"CRC64 check vector");
    }
    private static void execute(Session session,String sql){try(Rows rows=session.query(sql)){while(rows.next()!=null){}}}
    private static void check(boolean value,String message){if(!value)throw new AssertionError(message);}
}
