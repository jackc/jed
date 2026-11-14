package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import org.jed.storage.*;

/** Whole-GiST replacement preserves old pins and exact live-page accounting. */
public final class GistStorageTest {
    public static void main(String[] args) throws Exception {
        for (boolean encrypted : new boolean[]{false,true}) run(encrypted);
        System.out.println("GistStorageTest passed (plaintext/encrypted live pages, pins, rewrite, drop and reopen)");
    }
    private static void run(boolean encrypted) throws Exception {
        Path directory=Files.createTempDirectory("jed-gist-storage-");Path path=directory.resolve("db.jed");Credential credential=encrypted?Credential.hostKey(new byte[32]):null;
        try {
            try(FileDatabase file=FileDatabase.createExclusive(path,1024,2048,credential,Environment.empty())) {
                var rows=PersistentTree.empty(1024,new TableLeafCodec(List.of(SqlType.I32,SqlType.I32),new int[]{0}),file.pageCodec());
                var entries=PersistentTree.empty(1024,TableLeafCodec.index(),file.pageCodec());
                for(int i=0;i<80;i++){rows=rows.put(key(i),Encoding.row(List.of(integer(i),integer(i))));entries=entries.put(entry(i,i),new byte[0]);}
                var index=new Engine.Index("t_n_gist",List.of(new Engine.IndexKey(1,null,SqlType.I32,false,false)),false,false,null,2,entries);
                var table=new Engine.Table("t",List.of(new Engine.Column("id",SqlType.I32,true,null),new Engine.Column("n",SqlType.I32,false,null)),new int[]{0},List.of(),rows,0,List.of(),List.of(index));
                var state=file.commit(file.snapshot().put(table));var old=state;file.pin(old);
                for(int i=0;i<12;i++) {
                    table=state.tables().get("t");index=table.indexes().getFirst();
                    index=index.changed(index.entries().remove(entry(i,i)).put(entry(i,100+i),new byte[0]));
                    table=table.changed(table.rows().put(key(i),Encoding.row(List.of(integer(i),integer(100+i)))),0).withIndexes(List.of(index));
                    state=file.commit(state.put(table));checkLive(file,state);
                }
                check(old.tables().get("t").indexes().getFirst().entries().getEntry(entry(0,0))!=null,"old GiST index changed");file.unpin(old);
            }
            try(FileDatabase file=FileDatabase.openExclusive(path,1024,credential,Environment.empty())) {
                var state=file.snapshot();var table=state.tables().get("t");var index=table.indexes().getFirst();
                check(index.entries().getEntry(entry(0,100))!=null,"GiST update lost on reopen");checkLive(file,state);
                state=file.commit(state.put(table.withIndexes(List.of())));checkLive(file,state);
                state=file.commit(state.drop("t"));checkLive(file,state);
            }
        }finally{try(var files=Files.walk(directory)){for(Path file:files.sorted(java.util.Comparator.reverseOrder()).toList())Files.deleteIfExists(file);}}
    }
    private static void checkLive(FileDatabase file,Engine.Snapshot snapshot)throws Exception {
        var meta=file.serviceMeta();long count=0;
        // FileDatabase's pager is deliberately private; the already-resolved roots expose their page sets.
        for(var table:snapshot.tables().values()){count+=table.rows().physicalPages().size();for(var index:table.indexes()){
            count+=index.physicalPages().size();
            if(index.kind()==2){var facts=index.gist().structuralFacts();check(facts.nodeCount()==index.physicalPages().size()&&facts.leafCount()>0&&facts.height()>0,"resident GiST estimator facts");}
        }}
        // Tiny schema fits one catalog page, including the empty catalog after its drop.
        count++;
        check(count==meta.livePages(),"GiST exact live-page count");
    }
    private static SqlValue integer(int value){return SqlValue.ofInteger(SqlType.I32,value);}
    private static byte[] key(int value){return Encoding.key(integer(value));}
    private static byte[] entry(int id,int value){return GistTree.leafKey(List.of(integer(value)),key(id));}
    private static void check(boolean value,String message){if(!value)throw new AssertionError(message);}
}
