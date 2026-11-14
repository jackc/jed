package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.List;
import org.jed.storage.*;

/** Independent root allocation, unknown counts, pins and reclamation for key-only trees. */
public final class IndexStorageTest {
    public static void main(String[] args) throws Exception {
        for (boolean encrypted : new boolean[]{false, true}) run(encrypted);
        System.out.println("IndexStorageTest: passed plaintext/encrypted index COW, pins, rename, drop and reopen");
    }
    private static void run(boolean encrypted) throws Exception {
        Path directory = Files.createTempDirectory("jed-index-storage-"); Path path = directory.resolve("db.jed");
        Credential credential = encrypted ? Credential.hostKey(new byte[32]) : null;
        try {
            try (FileDatabase file = FileDatabase.createExclusive(path, 1024, 2048, credential, Environment.empty())) {
                var columns = List.of(new Engine.Column("id",SqlType.I32,true,null),new Engine.Column("n",SqlType.I32,false,null));
                var rows = PersistentTree.empty(1024,new TableLeafCodec(List.of(SqlType.I32,SqlType.I32),new int[]{0}),file.pageCodec());
                var entries = PersistentTree.empty(1024,TableLeafCodec.index(),file.pageCodec());
                for(int i=0;i<300;i++){rows=rows.put(key(i),Encoding.row(List.of(integer(i),integer(i%20))));entries=entries.put(entry(i,i%20),new byte[0]);}
                var index = new Engine.Index("t_n_idx",List.of(new Engine.IndexKey(1,null,SqlType.I32,false,false)),false,false,null,0,entries);
                var table = new Engine.Table("t",columns,new int[]{0},List.of(),rows,0,List.of(),List.of(index));
                var state = file.commit(file.snapshot().put(table)); var pinned = state; file.pin(pinned);
                for(int i=0;i<30;i++) {
                    table=state.tables().get("t"); index=table.indexes().getFirst();
                    var changed=index.entries().remove(entry(i,i%20)).put(entry(i,100+i),new byte[0]);
                    table=table.changed(table.rows().put(key(i),Encoding.row(List.of(integer(i),integer(100+i)))),0).withIndexes(List.of(index.changed(changed)));
                    state=file.commit(state.put(table));
                }
                check(pinned.tables().get("t").indexes().getFirst().entries().getEntry(entry(0,0))!=null,"old index pin overwritten");
                file.unpin(pinned); checkCount(state.tables().get("t").indexes().getFirst().entries(),300); checkLive(path,file,state);
                table=state.tables().get("t");
                var renamed=new Engine.Table("renamed",table.columns(),table.primaryKey(),table.unique(),table.rows(),table.nextRowId(),table.checks(),table.indexes());
                state=file.commit(state.drop("t").put(renamed));
                checkCount(state.tables().get("renamed").indexes().getFirst().entries(),300); checkLive(path,file,state);
            }
            try(FileDatabase file=FileDatabase.openExclusive(path,1024,credential,Environment.empty())) {
                var state=file.snapshot();var table=state.tables().get("renamed");
                check(table.indexes().getFirst().entries().knownSize().isEmpty(),"loaded index eagerly counted");
                checkCount(table.indexes().getFirst().entries(),300);
                state=file.commit(state.put(table.withIndexes(List.of())));
                check(state.tables().get("renamed").indexes().isEmpty(),"index drop not published"); checkLive(path,file,state);
                file.commit(state.drop("renamed"));
            }
            try(FileDatabase file=FileDatabase.openExclusive(path,1024,credential,Environment.empty())){check(file.snapshot().tables().isEmpty(),"dropped trees remain in catalog");}
        } finally {try(var files=Files.walk(directory)){for(Path file:files.sorted(java.util.Comparator.reverseOrder()).toList())Files.deleteIfExists(file);}}
    }
    private static void checkLive(Path path,FileDatabase file,Engine.Snapshot snapshot)throws Exception {
        var meta=file.serviceMeta(); long live=0;
        try(var store=FileBlockStore.open(path,false);var pager=new Pager(store,1024,1024,false,file.pageCodec());var catalog=CatalogChain.cursor(pager,meta.root(),meta.pageCount(),meta.rootTag())) {
            while(catalog.next()!=null){}live+=catalog.visitedPages().size();
        }
        for(var table:snapshot.tables().values()){live+=table.rows().physicalPages().size();for(var index:table.indexes())live+=index.entries().physicalPages().size();}
        check(live==meta.livePages(),"exact live count after index or rename mutation");
    }
    private static void checkCount(PersistentTree tree,int expected)throws Exception{int count=0;byte[] prior=null;try(var cursor=tree.cursor()){PersistentTree.Entry row;while((row=cursor.next())!=null){check(row.value().length==0,"index stores a value payload");check(prior==null||Arrays.compareUnsigned(prior,row.key())<0,"index ordering");prior=row.key();count++;}}check(count==expected,"index cardinality");}
    private static SqlValue integer(int value){return SqlValue.ofInteger(SqlType.I32,value);}
    private static byte[] key(int value){return Encoding.key(integer(value));}
    private static byte[] entry(int id,int value){byte[] key=new byte[9];System.arraycopy(key(value),0,key,1,4);System.arraycopy(key(id),0,key,5,4);return key;}
    private static void check(boolean value,String message){if(!value)throw new AssertionError(message);}
}
