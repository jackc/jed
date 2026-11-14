package org.jed;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HexFormat;
import java.util.List;
import java.util.concurrent.TimeUnit;

/** Real peer processes from the frozen core revision; both ownership directions and storage variants. */
public final class PageServiceMixedTest {
    public static void main(String[] args)throws Exception{
        if(args.length!=1)throw new IllegalArgumentException("frozen Go process actor path required");
        allocationHistory(Path.of(args[0]));
        reuseHistory(Path.of(args[0]));
        for(boolean encrypted:new boolean[]{false,true}){run(Path.of(args[0]),true,encrypted);run(Path.of(args[0]),false,encrypted);indexes(Path.of(args[0]),true,encrypted);indexes(Path.of(args[0]),false,encrypted);ownerLoss(Path.of(args[0]), encrypted);lostAcknowledgement(Path.of(args[0]), encrypted, false);lostAcknowledgement(Path.of(args[0]), encrypted, true);yieldOwnership(Path.of(args[0]), encrypted, true);yieldOwnership(Path.of(args[0]), encrypted, false);readOnlyHandoff(Path.of(args[0]), encrypted, true);readOnlyHandoff(Path.of(args[0]), encrypted, false);}
        attachmentGates(Path.of(args[0]), true); attachmentGates(Path.of(args[0]), false);
        System.out.println("PageServiceMixedTest: passed 24 Go/Java plaintext/encrypted cases: both ownership directions, pinned readers, alternating writes, ordered/GIN/GiST index mutation, owner loss, retained DEK, lost acknowledgements, voluntary/read-only handoff, attachment gate order and partial-acquisition timeout cleanup");
    }
    private static void allocationHistory(Path actor) throws Exception {
        Path directory = Files.createTempDirectory("jed-java-allocation-peer-");
        try (Database database = Database.create(directory.resolve("java.jed"), FileOptions.builder().build());
             Session session = database.session(); Peer peer = new Peer(actor, "create", directory.resolve("go.jed"), false)) {
            String[] statements = {
                "CREATE TABLE t (id i64 PRIMARY KEY, pad text)", "CREATE TABLE u (id i64 PRIMARY KEY, body text)",
                "INSERT INTO t SELECT g, rpad(g::text, 302, 'x') FROM generate_series(1, 2000) g", "DELETE FROM t WHERE id > 100",
                "INSERT INTO t SELECT g, rpad(g::text, 302, 'z') FROM generate_series(2001, 4000) g", "DELETE FROM t WHERE id > 100",
                "INSERT INTO u SELECT g, (SELECT string_agg(md5((g * 1000 + k)::text), '') FROM generate_series(1, 200) k) FROM generate_series(1, 10) g",
                "DELETE FROM u WHERE id > 5", "UPDATE t SET pad = 'y' WHERE id = 1", "INSERT INTO t VALUES(5000, 'pending')"
            };
            for (int step = 0; step < statements.length; step++) {
                execute(session, statements[step]); peer.sql("EXEC", statements[step]);
                long actual = database.processPageCount(), expected = Long.parseLong(peer.command("PAGE_COUNT", ""));
                check(actual == expected, "allocation history step " + (step + 1) + " Java=" + actual + " Go=" + expected);
                byte[] javaBytes = java.util.Arrays.copyOf(Files.readAllBytes(directory.resolve("java.jed")), Math.toIntExact(actual * 8192));
                byte[] goBytes = java.util.Arrays.copyOf(Files.readAllBytes(directory.resolve("go.jed")), Math.toIntExact(expected * 8192));
                long mismatch = java.util.Arrays.mismatch(javaBytes, goBytes);
                if (mismatch >= 0) {
                    Path evidence = Path.of(".dev/java-native/allocation-diagnostic"); Files.createDirectories(evidence);
                    Files.write(evidence.resolve("java-" + (step + 1) + ".jed"), javaBytes); Files.write(evidence.resolve("go-" + (step + 1) + ".jed"), goBytes);
                }
                check(mismatch < 0, "allocation bytes step " + (step + 1) + " differ page=" + mismatch / 8192 + " offset=" + mismatch % 8192);
            }
        } finally { try (var paths = Files.walk(directory)) { for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(path); } }
    }
    private static void reuseHistory(Path actor) throws Exception {
        Path directory = Files.createTempDirectory("jed-java-reuse-peer-");
        Path source = actor.getParent().resolve("spec/fileformat/fixtures/cow_inline.jed");
        Files.copy(source, directory.resolve("java.jed")); Files.copy(source, directory.resolve("go.jed"));
        try { for (int phase = 0; phase < 2; phase++) {
        try (Database database = Database.openExclusive(directory.resolve("java.jed"), 1048576);
             Session session = database.session(); Peer peer = new Peer(actor, "open-exclusive", directory.resolve("go.jed"), false)) {
            for (int step = 0; step < (phase == 0 ? 13 : 12); step++) {
                String sql = phase == 0 && step == 0 ? "INSERT INTO t SELECT g, 20 FROM generate_series(2,400) g" : "UPDATE t SET v = " + (phase == 0 ? 20 + step : 33 + step);
                execute(session, sql); peer.sql("EXEC", sql);
                long actual = database.processPageCount(), expected = Long.parseLong(peer.command("PAGE_COUNT", ""));
                byte[] javaBytes = java.util.Arrays.copyOf(Files.readAllBytes(directory.resolve("java.jed")), Math.toIntExact(actual * 256));
                byte[] goBytes = java.util.Arrays.copyOf(Files.readAllBytes(directory.resolve("go.jed")), Math.toIntExact(expected * 256));
                long mismatch = java.util.Arrays.mismatch(javaBytes, goBytes);
                if (mismatch >= 0) {
                    Path evidence = Path.of(".dev/java-native/reuse-diagnostic"); Files.createDirectories(evidence);
                    Files.write(evidence.resolve("java-" + phase + "-" + step + ".jed"), javaBytes); Files.write(evidence.resolve("go-" + phase + "-" + step + ".jed"), goBytes);
                }
                check(mismatch < 0, "reuse bytes phase " + phase + " step " + step + " differ page=" + mismatch / 256 + " offset=" + mismatch % 256 + " high=" + actual + "/" + expected);
            }
        }
        } } finally { try (var paths = Files.walk(directory)) { for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(path); } }
    }
    private static void attachmentGates(Path actor, boolean javaFirstPath) throws Exception {
        Path directory = Files.createTempDirectory("jed-java-attachment-peer-");
        Path own = directory.resolve(javaFirstPath ? "a.jed" : "z.jed"), foreign = directory.resolve(javaFirstPath ? "z.jed" : "a.jed");
        Peer peer = null; Database database = null;
        try {
            database = Database.create(own, FileOptions.builder().shutdownGraceMillis(100).build());
            peer = new Peer(actor, "create", foreign, false);
            try (Session session = database.session(); Session reader = database.session()) {
                execute(session, "CREATE TABLE t(id i32 PRIMARY KEY)"); peer.sql("EXEC", "CREATE TABLE t(id i32 PRIMARY KEY)");
                database.attach("remote", AttachSource.file(foreign), false);
                peer.command("ATTACH", "remote\t0\t" + own);
                execute(session, "BEGIN"); execute(session, "INSERT INTO remote.t VALUES(1)");
                Peer heldPeer = peer;
                var waiting = java.util.concurrent.CompletableFuture.supplyAsync(() -> {
                    try { return heldPeer.command("WRITE_OPEN", "5000"); } catch (IOException failure) { throw new java.util.concurrent.CompletionException(failure); }
                });
                Thread.sleep(75); check(!waiting.isDone(), "cross-owner writer did not wait");
                execute(session, "COMMIT"); waiting.get(5, TimeUnit.SECONDS);
                peer.sql("WRITE_EXEC", "INSERT INTO remote.t VALUES(2)"); peer.command("WRITE_COMMIT", "");
                check(integer(session, "SELECT id FROM main.t") == 2, "peer attachment write visible");
                check(peer.sql("QUERY_I64", "SELECT id FROM main.t").equals("1"), "Java attachment write visible");
                execute(reader, "BEGIN READ ONLY");
                check(integer(reader, "SELECT id FROM remote.t") == 1, "attached old pin");
                peer.sql("EXEC", "UPDATE main.t SET id = 3");
                check(integer(reader, "SELECT id FROM remote.t") == 1, "attached snapshot retained");
                execute(reader, "ROLLBACK");
                check(integer(session, "SELECT id FROM remote.t") == 3, "attachment refresh after peer commit");
                long detachDeadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2);
                for (;;) {
                    try { peer.command("DETACH", "remote"); break; }
                    catch (AssertionError busy) { if (!busy.getMessage().contains("ERR\t55006") || System.nanoTime() >= detachDeadline) throw busy; Thread.sleep(10); }
                }
                peer.command("WRITE_OPEN", "0"); session.setLockTimeoutMillis(30);
                try { execute(session, "BEGIN"); throw new AssertionError("attachment writer timeout missing"); }
                catch (SqlException failure) { check(failure.sqlState().equals("55P03"), "attachment timeout classification"); }
                peer.command("WRITE_COMMIT", ""); session.setLockTimeoutMillis(0);
                execute(session, "BEGIN"); execute(session, "ROLLBACK");
                database.detach("remote");
            }
        } finally {
            if (peer != null) peer.close(); if (database != null) database.close();
            try (var paths = Files.walk(directory)) { for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(path); }
        }
    }
    private static void run(Path actor,boolean javaOwner,boolean encrypted)throws Exception{
        Path directory=Files.createTempDirectory("jed-java-peer-");Path path=directory.resolve("db.jed");Credential credential=encrypted?Credential.hostKey(new byte[32]):null;
        FileOptions options=FileOptions.builder().credential(credential).shutdownGraceMillis(100).build();
        Database javaDatabase=null;Peer peer=null;
        try{
            if(javaOwner){javaDatabase=Database.create(path,options);peer=new Peer(actor,"open",path,encrypted);}
            else{peer=new Peer(actor,"create",path,encrypted);javaDatabase=Database.open(path,options);}
            try(Session session=javaDatabase.session();Session reader=javaDatabase.session()){
                execute(session,"CREATE TABLE peer_data(id i32 PRIMARY KEY, value i32)");peer.sql("EXEC","INSERT INTO peer_data VALUES (1, 10)");
                check(integer(session,"SELECT value FROM peer_data WHERE id = 1")==10,"Java reads peer insert");
                execute(reader,"BEGIN READ ONLY");check(integer(reader,"SELECT value FROM peer_data WHERE id = 1")==10,"initial Java pin");
                peer.command("READ_OPEN","");check(peer.sql("READ_QUERY_I64","SELECT value FROM peer_data WHERE id = 1").equals("10"),"initial peer pin");
                for(int i=0;i<12;i++){
                    int value=i+20;String sql="UPDATE peer_data SET value = "+value+" WHERE id = 1";
                    if(i%2==0)execute(session,sql);else peer.sql("EXEC",sql);
                    check(integer(session,"SELECT value FROM peer_data WHERE id = 1")==value,"Java observes alternating write");
                    check(peer.sql("QUERY_I64","SELECT value FROM peer_data WHERE id = 1").equals(Integer.toString(value)),"peer observes alternating write");
                    check(integer(reader,"SELECT value FROM peer_data WHERE id = 1")==10,"Java old snapshot preserved");
                    check(peer.sql("READ_QUERY_I64","SELECT value FROM peer_data WHERE id = 1").equals("10"),"peer old snapshot preserved");
                }
                execute(reader,"ROLLBACK");peer.command("READ_CLOSE","");
                execute(session,"BEGIN");execute(session,"INSERT INTO peer_data VALUES (2, 50)");execute(session,"COMMIT");
                check(peer.sql("QUERY_I64","SELECT value FROM peer_data WHERE id = 2").equals("50"),"explicit Java commit visible");
                peer.command("WRITE_OPEN","0");peer.sql("WRITE_EXEC","INSERT INTO peer_data VALUES (3, 60)");peer.command("WRITE_COMMIT","");
                check(integer(session,"SELECT value FROM peer_data WHERE id = 3")==60,"explicit peer commit visible");
            }
        }finally{
            if(peer!=null)peer.close();if(javaDatabase!=null)javaDatabase.close();
            try(var files=Files.walk(directory)){for(Path file:files.sorted(java.util.Comparator.reverseOrder()).toList())Files.deleteIfExists(file);}
        }
    }
    private static void indexes(Path actor, boolean javaOwner, boolean encrypted) throws Exception {
        Path directory = Files.createTempDirectory("jed-java-peer-index-"); Path path = directory.resolve("db.jed");
        Credential credential = encrypted ? Credential.hostKey(new byte[32]) : null;
        FileOptions options = FileOptions.builder().credential(credential).pageSize(1024).shutdownGraceMillis(100).build();
        Database database = null; Peer peer = null;
        try {
            if (javaOwner) { database = Database.create(path, options); peer = new Peer(actor, "open", path, encrypted); }
            else { peer = new Peer(actor, "create", path, encrypted, "page_size=1024"); database = Database.open(path, options); }
            try (Session session = database.session(); Session old = database.session()) {
                execute(session, "CREATE TABLE indexed(id i32 PRIMARY KEY, value i32, active boolean)");
                execute(session, "CREATE UNIQUE INDEX indexed_value ON indexed(value)");
                peer.sql("EXEC", "CREATE INDEX indexed_expression ON indexed((value + 1))");
                execute(session, "CREATE INDEX indexed_partial ON indexed(value DESC NULLS LAST) WHERE active");
                StringBuilder insert = new StringBuilder("INSERT INTO indexed VALUES ");
                for (int i = 0; i < 160; i++) { if (i != 0) insert.append(','); insert.append('(').append(i).append(',').append(i+1000).append(',').append(i%2==0?"true":"false").append(')'); }
                peer.sql("EXEC", insert.toString());
                execute(old, "BEGIN READ ONLY"); check(integer(old, "SELECT id FROM indexed WHERE value = 1000") == 0, "pinned indexed row");
                peer.command("READ_OPEN", ""); check(peer.sql("READ_QUERY_I64", "SELECT id FROM indexed WHERE value = 1000").equals("0"), "peer pinned indexed row");
                for (int i = 0; i < 16; i++) {
                    String sql = "UPDATE indexed SET value = " + (2000+i) + ", active = true WHERE id = " + i;
                    if (i%2==0) execute(session, sql); else peer.sql("EXEC", sql);
                    check(integer(session, "SELECT id FROM indexed WHERE value = " + (2000+i)) == i, "Java updated index lookup");
                    check(peer.sql("QUERY_I64", "SELECT id FROM indexed WHERE value = " + (2000+i)).equals(Integer.toString(i)), "peer updated index lookup");
                    check(integer(session, "SELECT id FROM indexed WHERE value + 1 = " + (2001+i)) == i, "Java expression index lookup");
                    check(peer.sql("QUERY_I64", "SELECT id FROM indexed WHERE value + 1 = " + (2001+i)).equals(Integer.toString(i)), "peer expression index lookup");
                    check(integer(old, "SELECT id FROM indexed WHERE value = 1000") == 0, "old Java index tree retained");
                    check(peer.sql("READ_QUERY_I64", "SELECT id FROM indexed WHERE value = 1000").equals("0"), "old peer index tree retained");
                }
                execute(old, "ROLLBACK"); peer.command("READ_CLOSE", "");
                try { execute(session, "INSERT INTO indexed VALUES (500, 2000, true)"); throw new AssertionError("unique index accepted duplicate"); }
                catch (SqlException failure) { check(failure.sqlState().equals("23505"), "unique index conflict classification"); }
                execute(session, "DELETE FROM indexed WHERE id = 0");
                peer.sql("EXEC", "INSERT INTO indexed VALUES (500, 2000, true)");
                check(integer(session, "SELECT id FROM indexed WHERE value = 2000") == 500, "peer reuses deleted unique key");
                peer.sql("EXEC", "DROP INDEX indexed_expression"); execute(session, "DROP INDEX indexed_partial");
                check(peer.sql("QUERY_I64", "SELECT count(*) FROM indexed").equals("160"), "index drop preserves rows");
                execute(session, "CREATE TABLE inverted(id i32 PRIMARY KEY, tags i32[], doc jsonb)");
                execute(session, "CREATE INDEX inverted_tags ON inverted USING gin(tags)");
                peer.sql("EXEC", "CREATE INDEX inverted_doc ON inverted USING gin(doc)");
                StringBuilder ginInsert = new StringBuilder("INSERT INTO inverted VALUES ");
                for (int i=0;i<160;i++) { if(i!=0)ginInsert.append(',');ginInsert.append('(').append(i).append(",ARRAY[").append(i).append(",1000],'{\"n\":").append(i).append("}')"); }
                peer.sql("EXEC", ginInsert.toString());
                for(int i=0;i<16;i++) {
                    String update="UPDATE inverted SET tags = ARRAY["+(2000+i)+",1000], doc = '{\"n\":"+(2000+i)+"}' WHERE id = "+i;
                    if(i%2==0)execute(session,update);else peer.sql("EXEC",update);
                    check(peer.sql("QUERY_I64","SELECT id FROM inverted WHERE tags @> ARRAY["+(2000+i)+"]").equals(Integer.toString(i)),"peer reads updated GIN array posting");
                    check(peer.sql("QUERY_I64","SELECT id FROM inverted WHERE doc @> '{\"n\":"+(2000+i)+"}'").equals(Integer.toString(i)),"peer reads updated GIN JSON posting");
                    check(integer(session,"SELECT id FROM inverted WHERE tags @> ARRAY["+(2000+i)+"]")==i,"Java reads updated GIN row");
                }
                execute(session,"DELETE FROM inverted WHERE id = 0");
                check(peer.sql("QUERY_I64","SELECT count(*) FROM inverted WHERE tags @> ARRAY[2000]").equals("0"),"removed GIN posting");
                execute(session, "CREATE TABLE spatial(id i32 PRIMARY KEY, span int4range, room i32)");
                execute(session, "CREATE INDEX spatial_span ON spatial USING gist(span)");
                peer.sql("EXEC", "CREATE INDEX spatial_room ON spatial USING gist(room)");
                StringBuilder gistInsert = new StringBuilder("INSERT INTO spatial VALUES ");
                for (int i = 0; i < 40; i++) { if (i != 0) gistInsert.append(','); gistInsert.append('(').append(i).append(",int4range(").append(i*10).append(',').append(i*10+5).append("),").append(i).append(')'); }
                peer.sql("EXEC", gistInsert.toString());
                execute(old, "BEGIN READ ONLY"); check(integer(old, "SELECT id FROM spatial WHERE span && int4range(1,2)") == 0, "pinned GiST row");
                for (int i = 0; i < 12; i++) {
                    String update = "UPDATE spatial SET span = int4range(" + (1000+i*10) + "," + (1005+i*10) + "), room = " + (100+i) + " WHERE id = " + i;
                    if (i % 2 == 0) execute(session, update); else peer.sql("EXEC", update);
                    check(peer.sql("QUERY_I64", "SELECT id FROM spatial WHERE span && int4range(" + (1001+i*10) + "," + (1002+i*10) + ")").equals(Integer.toString(i)), "peer reads Java GiST range nodes");
                    check(integer(session, "SELECT id FROM spatial WHERE room = " + (100+i)) == i, "Java reads peer GiST scalar nodes");
                    check(integer(old, "SELECT id FROM spatial WHERE span && int4range(1,2)") == 0, "old GiST snapshot retained");
                }
                execute(old, "ROLLBACK"); peer.sql("EXEC", "DELETE FROM spatial WHERE id = 0");
                check(integer(session, "SELECT count(*) FROM spatial WHERE span && int4range(1001,1002)") == 0, "GiST removal survives peer commit");
            }
            peer.close(); peer = null; database.close(); database = null;
            try (Database reopened = Database.openExclusive(path, 65536, credential); Session session = reopened.session()) {
                check(integer(session, "SELECT id FROM indexed WHERE value = 2000") == 500, "index survives reopen");
                execute(session, "UPDATE indexed SET value = 3000 WHERE id = 500");
            }
            try (Peer reopened = new Peer(actor, "open", path, encrypted)) { check(reopened.sql("QUERY_I64", "SELECT id FROM indexed WHERE value = 3000").equals("500"), "peer opens Java rewritten index"); }
        } finally {
            if (peer != null) peer.close(); if (database != null) database.close();
            try (var files = Files.walk(directory)) { for (Path file : files.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(file); }
        }
    }
    private static void readOnlyHandoff(Path actor, boolean encrypted, boolean javaOwner) throws Exception {
        Path directory = Files.createTempDirectory("jed-java-readonly-handoff-"); Path path = directory.resolve("db.jed");
        Credential credential = encrypted ? Credential.hostKey(new byte[32]) : null;
        try (Database initial=Database.createExclusive(path,8192,65536,credential);Session session=initial.session()) { execute(session,"CREATE TABLE handed(id i32 PRIMARY KEY)"); }
        Database database=null;Peer peer=null;
        try {
            if(javaOwner){database=Database.open(path,FileOptions.builder().credential(credential).readOnly(true).yieldGraceMillis(100).shutdownGraceMillis(0).build());peer=new Peer(actor,"open",path,encrypted);peer.sql("EXEC","INSERT INTO handed VALUES (1)");}
            else{peer=new Peer(actor,"open-read-only",path,encrypted,"yield_grace=100");database=Database.open(path,FileOptions.builder().credential(credential).shutdownGraceMillis(0).build());try(Session session=database.session()){session.setLockTimeoutMillis(5000);execute(session,"INSERT INTO handed VALUES (1)");}}
            try(Session session=database.session()){check(integer(session,"SELECT id FROM handed")==1,"read-only handoff Java snapshot");}
            check(peer.sql("QUERY_I64","SELECT id FROM handed").equals("1"),"read-only handoff peer snapshot");
            check(peer.command("ROLE","").equals(javaOwner?"owner":"client"),"writable client took read-only owner role");
        } finally {if(peer!=null)peer.close();if(database!=null)database.close();try(var files=Files.walk(directory)){for(Path file:files.sorted(java.util.Comparator.reverseOrder()).toList())Files.deleteIfExists(file);}}
    }
    private static void yieldOwnership(Path actor, boolean encrypted, boolean javaOwner) throws Exception {
        Path directory = Files.createTempDirectory("jed-java-yield-"); Path path = directory.resolve("db.jed");
        Credential credential = encrypted ? Credential.hostKey(new byte[32]) : null;
        Database database = null; Peer peer = null;
        try {
            if (javaOwner) {
                database = Database.create(path, FileOptions.builder().credential(credential).ownership(FileOptions.Ownership.YIELD).yieldGraceMillis(100).shutdownGraceMillis(0).build());
                try (Session session = database.session()) { execute(session, "CREATE TABLE yielded(id i32 PRIMARY KEY)"); execute(session, "INSERT INTO yielded VALUES (1)"); }
                peer = new Peer(actor, "open", path, encrypted);
            } else {
                peer = new Peer(actor, "create", path, encrypted, "ownership=yield", "yield_grace=100");
                peer.sql("EXEC", "CREATE TABLE yielded(id i32 PRIMARY KEY)"); peer.sql("EXEC", "INSERT INTO yielded VALUES (1)");
                database = Database.open(path, FileOptions.builder().credential(credential).shutdownGraceMillis(0).build());
            }
            try (Session session = database.session()) {
                check(integer(session, "SELECT id FROM yielded")==1, "Java continues across voluntary handoff");
                check(peer.sql("QUERY_I64", "SELECT id FROM yielded").equals("1"), "peer continues across voluntary handoff");
                long until = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
                String expected = javaOwner ? "owner" : "client";
                while (!peer.command("ROLE", "").equals(expected)) { if (System.nanoTime() >= until) throw new AssertionError("yield did not transfer ownership"); Thread.sleep(10); }
                execute(session, "INSERT INTO yielded VALUES (2)"); peer.sql("EXEC", "INSERT INTO yielded VALUES (3)");
                check(integer(session, "SELECT count(*) FROM yielded")==3, "both roles write after handoff");
            }
        } finally {
            if (peer != null) peer.close(); if (database != null) database.close();
            try (var files = Files.walk(directory)) { for (Path file : files.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(file); }
        }
    }
    private static void lostAcknowledgement(Path actor, boolean encrypted, boolean crash) throws Exception {
        Path directory = Files.createTempDirectory("jed-java-resolve-"); Path path = directory.resolve("db.jed");
        Credential credential = encrypted ? Credential.hostKey(new byte[32]) : null;
        Peer owner = new Peer(actor, "create", path, encrypted);
        try (Database database = Database.open(path, FileOptions.builder().credential(credential).shutdownGraceMillis(0).build()); Session session = database.session()) {
            execute(session, "CREATE TABLE resolved(id i32 PRIMARY KEY)");
            owner.command(crash ? "CRASH_NEXT_ACK" : "HANGUP_NEXT_ACK", "");
            execute(session, "INSERT INTO resolved VALUES (1)");
            check(integer(session, "SELECT id FROM resolved")==1, "lost acknowledgement resolved as committed");
            execute(session, "INSERT INTO resolved VALUES (2)");
            check(integer(session, "SELECT count(*) FROM resolved")==2, "handle writes after acknowledgement resolution");
        } finally {
            owner.close();
            try (var files = Files.walk(directory)) { for (Path file : files.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(file); }
        }
    }
    private static void ownerLoss(Path actor, boolean encrypted) throws Exception {
        Path directory = Files.createTempDirectory("jed-java-promote-"); Path path = directory.resolve("db.jed");
        Credential credential = encrypted ? Credential.hostKey(new byte[32]) : null;
        Peer owner = new Peer(actor, "create", path, encrypted);
        try (Database database = Database.open(path, FileOptions.builder().credential(credential).shutdownGraceMillis(0).build());
             Session current = database.session(); Session old = database.session()) {
            execute(current, "CREATE TABLE promotion(id i32 PRIMARY KEY, value i32)");
            execute(current, "INSERT INTO promotion VALUES (1, 11)");
            execute(old, "BEGIN READ ONLY"); check(integer(old, "SELECT value FROM promotion")==11, "reader before owner loss");
            if (encrypted) owner.command("CHANGE_KEY", "01".repeat(32));
            owner.process.destroyForcibly(); check(owner.process.waitFor(5, TimeUnit.SECONDS), "owner killed");
            long until = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
            for (;;) {
                try { check(integer(current, "SELECT value FROM promotion")==11, "promoted snapshot"); break; }
                catch (SqlException failure) { if (!failure.sqlState().equals("08006") || System.nanoTime() >= until) throw failure; Thread.sleep(10); }
            }
            try { integer(old, "SELECT value FROM promotion"); throw new AssertionError("old transaction survived connection change"); }
            catch (SqlException failure) { check(failure.sqlState().equals("08006"), "old role classification"); }
            execute(old, "ROLLBACK"); execute(current, "UPDATE promotion SET value = 12");
            check(integer(current, "SELECT value FROM promotion")==12, "promoted owner writes");
            if (!encrypted) try (Peer client = new Peer(actor, "open", path, false)) { check(client.sql("QUERY_I64", "SELECT value FROM promotion").equals("12"), "promoted owner serves peers"); }
        } finally {
            owner.close();
            try (var files = Files.walk(directory)) { for (Path file : files.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(file); }
        }
    }
    private static final class Peer implements AutoCloseable {
        final Process process;final BufferedWriter input;final BufferedReader output;
        Peer(Path actor,String action,Path path,boolean encrypted,String... options)throws Exception{
            var command=new java.util.ArrayList<>(List.of(actor.toString(),action,path.toString(),"shutdown_grace=100"));if(encrypted)command.add("key="+"00".repeat(32));command.addAll(List.of(options));
            process=new ProcessBuilder(command).redirectError(ProcessBuilder.Redirect.INHERIT).start();input=new BufferedWriter(new OutputStreamWriter(process.getOutputStream(),StandardCharsets.UTF_8));output=new BufferedReader(new InputStreamReader(process.getInputStream(),StandardCharsets.UTF_8));
            String ready=output.readLine();if(!"READY".equals(ready))throw new AssertionError("peer open: "+ready);
        }
        String sql(String command,String sql)throws IOException{return command(command,HexFormat.of().formatHex(sql.getBytes(StandardCharsets.UTF_8)));}
        String command(String command,String argument)throws IOException{input.write(command+"\t"+argument+"\n");input.flush();String reply=output.readLine();if(reply==null||!reply.startsWith("OK\t"))throw new AssertionError(command+": "+reply);return reply.substring(3);}
        public void close()throws Exception{try{if(process.isAlive())command("CLOSE","");}finally{if(!process.waitFor(5,TimeUnit.SECONDS)){process.destroyForcibly();process.waitFor();}}}
    }
    private static void execute(Session session,String sql){try(Rows rows=session.query(sql)){while(rows.next()!=null){}}}
    private static long integer(Session session,String sql){try(Rows rows=session.query(sql)){return rows.next().values().getFirst().asInteger();}}
    private static void check(boolean value,String message){if(!value)throw new AssertionError(message);}
}
