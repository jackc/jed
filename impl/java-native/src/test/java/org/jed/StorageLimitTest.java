package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;

/** Host budget setters, pre-write failure, packed memory reuse and snapshot retention. */
public final class StorageLimitTest {
    public static void main(String[] args) throws Exception {
        memory(); file();
        System.out.println("StorageLimitTest passed (host limits, pre-write rejection, memory reuse and retained readers)");
    }
    private static void memory() {
        try(Database db=Database.memory();Session session=db.session()) {
            run(session,"CREATE TABLE t (id i32 PRIMARY KEY, v text)");db.setMaxStorageBytes(163840);
            for(int start=0;start<100;start+=10)run(session,batch(start,1015));
            state(session,batch(100,1015),"54P06");value(session,"SELECT count(*) FROM t",100);
            run(session,"BEGIN");run(session,batch(100,1015));state(session,"COMMIT","54P06");value(session,"SELECT count(*) FROM t",100);
            run(session,"DELETE FROM t WHERE id >= 50");
            for(int start=50;start<110;start+=10)run(session,batch(start,1015));
            state(session,batch(110,1015),"54P06");db.setMaxStorageBytes(-1);run(session,batch(110,1015));
            check(db.maxStorageBytes()==0,"negative means unlimited");
            try(Database.Pin old=db.pinSnapshot()) {
                long oldSize=old.snapshot().tables().get("t").rows().size();
                run(session,"DELETE FROM t");for(int i=0;i<20;i++)run(session,"INSERT INTO t VALUES ("+i+", 'new')");
                check(old.snapshot().tables().get("t").rows().size()==oldSize,"old row count");
                try(var cursor=old.snapshot().tables().get("t").rows().cursor()){check(cursor.next()!=null,"old packed row survives writes");}
                catch(java.io.IOException failure){throw new AssertionError(failure);}
            }
        }
        try(Database db=Database.memory(Environment.empty(),1024,4096);Session session=db.session()) {
            check(db.storageBytes()==3072,"empty memory catalog");run(session,"CREATE TABLE small (id i32 PRIMARY KEY)");check(db.storageBytes()==4096,"custom memory page size");
        }
        try(Database db=Database.memory(1);Session session=db.session()) {
            state(session,"CREATE TABLE first (id i32)","54P06");check(db.storageBytes()==24576,"failed first commit retains initial pages");
        }
    }
    private static void file()throws Exception {
        Path directory=Files.createTempDirectory("jed-storage-limit-");Path path=directory.resolve("db.jed");
        try {
            try(Database db=Database.createExclusive(path,8192,8192);Session session=db.session()) {
                run(session,"CREATE TABLE t (id i32 PRIMARY KEY,v text)");run(session,batch(0,1002));
                db.setMaxStorageBytes(db.storageBytes());byte[] before=Files.readAllBytes(path);
                state(session,batch(10,1002),"54P06");check(Arrays.equals(before,Files.readAllBytes(path)),"rejected commit wrote bytes");
                value(session,"SELECT count(*) FROM t",10);db.setMaxStorageBytes(0);run(session,batch(10,1002));
                db.setMaxStorageBytes(1);run(session,"DELETE FROM t");run(session,"DROP TABLE t");check(db.storageBytes()==8192,"empty file retains catalog live page");
            }
            try(Database db=Database.openExclusive(path,8192)){check(db.maxStorageBytes()==0,"limit must not persist");check(db.storageBytes()==8192,"live count persists");}
        }finally{try(var files=Files.walk(directory)){for(Path file:files.sorted(java.util.Comparator.reverseOrder()).toList())Files.deleteIfExists(file);}}
    }
    private static String batch(int start,int width){return "INSERT INTO t SELECT g, rpad(g::text, "+width+", 'x') FROM generate_series("+start+", "+(start+9)+") g";}
    private static void run(Session session,String sql){try(Rows rows=session.query(sql)){while(rows.next()!=null){}}}
    private static void value(Session session,String sql,long expected){try(Rows rows=session.query(sql)){check(rows.next().get(0).asInteger()==expected,sql);}}
    private static void state(Session session,String sql,String expected){try{run(session,sql);throw new AssertionError("expected "+expected+": "+sql);}catch(SqlException failure){if(!failure.sqlState().equals(expected))throw failure;}}
    private static void check(boolean value,String message){if(!value)throw new AssertionError(message);}
}
