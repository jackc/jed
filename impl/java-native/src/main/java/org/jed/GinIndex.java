package org.jed;

import java.io.IOException;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.TreeSet;
import org.jed.storage.GinPostingRuns;
import org.jed.storage.PersistentTree;

/** GIN operator-class terms and bounded build/maintenance above ordinary key-only trees. */
final class GinIndex {
    private GinIndex() {}
    static void validateType(SqlType type) {
        if(type.kind()==SqlType.Kind.JSONB)return;
        if(type.kind()!=SqlType.Kind.ARRAY)throw new SqlException("42704","type has no GIN operator class: "+type);
        switch(type.element().kind()) {
            case I16,I32,I64,BOOLEAN,UUID,DATE,TIMESTAMP,TIMESTAMPTZ,F32,F64,INTERVAL,TEXT,BYTEA,DECIMAL -> {}
            default -> throw new SqlException("0A000","array element type has no supported GIN operator class: "+type.element());
        }
    }
    static Engine.Index definition(SqlParser.CreateIndex syntax,Engine.Table table,Engine.Snapshot snapshot,QueryEnvironment environment,Database database) {
        List<Engine.IndexKey>keys=new ArrayList<>();List<String>parts=new ArrayList<>();
        for(var key:syntax.keys()) {
            var expression=DefaultExpression.unwrap(key.expression());
            if(!(expression instanceof SqlParser.Name name))throw new SqlException("0A000","GIN expression key is not supported");
            if(name.parts().size()!=1)throw new SqlException("42601","qualified index column");
            int column=table.column(name.parts().getLast());SqlType type=table.columns().get(column).type();validateType(type);
            String opclass=type.kind()==SqlType.Kind.JSONB?"jsonb_ops":"array_ops";
            if(key.opclass()!=null && !key.opclass().equals(opclass))throw new SqlException("42704","unknown GIN operator class: "+key.opclass());
            if(key.explicitDirection() || key.nullsFirst()!=null)throw new SqlException("0A000","GIN does not support key ordering options");
            keys.add(new Engine.IndexKey(column,null,type,false,false));
            String part=table.columns().get(column).name(),candidate=part;for(int suffix=1;parts.contains(candidate);suffix++)candidate=part+suffix;parts.add(candidate);
        }
        if(syntax.unique())throw new SqlException("0A000","GIN does not support unique indexes");
        if(keys.size()>32)throw new SqlException("54011","cannot use more than 32 columns in an index");
        var predicate=(SqlParser.StoredExpression)syntax.where();
        if(predicate!=null) {
            var resolver=environment.resolver(table.bindings(null),new ScalarPlan.Frame()).persisted();
            Indexes.validateExpression(predicate,resolver);
            if (true && !HostDependencies.collect(predicate, resolver, true).isEmpty()) throw new SqlException("42P17", "index method cannot persist host dependencies");
            if(IndexDependencies.analyze(predicate,resolver).present())throw new SqlException("42P17","GIN predicate cannot persist timezone dependencies");
            if(resolver.resolve(predicate,SqlType.BOOLEAN).type()!=SqlType.BOOLEAN)throw SqlException.typeMismatch("index predicate must be boolean");
        }
        String name=syntax.name()==null?Sequences.autoName(snapshot,table.name(),String.join("_",parts),"idx"):syntax.name().parts().getLast();
        if(syntax.name() != null && name.startsWith("jed_"))throw new SqlException("42939","reserved index name");
        return new Engine.Index(name,keys,false,false,predicate,1,PersistentTree.empty(database.pageSize(),TableLeafCodec.index(),database.pageCodec()));
    }
    static List<byte[]> terms(SqlValue value) {
        TreeSet<byte[]>terms=new TreeSet<>(Arrays::compareUnsigned);
        if(value.isNull())return List.of();
        if(value.type().kind()==SqlType.Kind.JSONB)json(value.asJsonb(),true,terms);
        else for(SqlValue element:value.asArray().elements())if(!element.isNull())terms.add(elementTerm(element));
        return List.copyOf(terms);
    }
    static byte[] elementTerm(SqlValue value) {
        return switch(value.type().kind()) {
            case TEXT -> escaped(cut(SqlText.encode(value.asText())));
            case BYTEA -> escaped(cut(value.asBytes()));
            case DECIMAL -> cut(Encoding.key(value));
            default -> Encoding.key(value);
        };
    }
    static byte[] jsonValueTerm(JsonNode value) {
        return switch(value.kind()) {
            case NULL -> new byte[]{2};
            case BOOLEAN -> new byte[]{3,(byte)(value.booleanValue()?1:0)};
            case NUMBER -> jsonBody(4,Encoding.key(SqlValue.ofDecimal(value.number())));
            case STRING -> jsonBody(5,SqlText.encode(value.string()));
            default -> throw new IllegalArgumentException("scalar JSON term required");
        };
    }
    static byte[] jsonTextTerm(String text) {return jsonBody(1,SqlText.encode(text));}
    private static byte[] cut(byte[]bytes){return bytes.length<=128?bytes:Arrays.copyOf(bytes,128);}
    private static byte[] escaped(byte[]bytes){Encoding.Sink out=new Encoding.Sink();for(byte b:bytes){out.u8(b&255);if(b==0)out.u8(255);}out.u8(0);out.u8(1);return out.bytes();}
    private static byte[] jsonBody(int flag,byte[]bytes){Encoding.Sink out=new Encoding.Sink();out.u8(flag|(bytes.length>128?16:0));out.raw(escaped(cut(bytes)));return out.bytes();}
    private static void json(JsonNode value,boolean arrayElement,TreeSet<byte[]>terms) {
        switch(value.kind()) {
            case NULL -> terms.add(new byte[]{2});
            case BOOLEAN -> terms.add(new byte[]{3,(byte)(value.booleanValue()?1:0)});
            case NUMBER -> terms.add(jsonBody(4,Encoding.key(SqlValue.ofDecimal(value.number()))));
            case STRING -> terms.add(jsonBody(arrayElement?1:5,SqlText.encode(value.string())));
            case ARRAY -> {for(JsonNode item:value.elements())json(item,true,terms);}
            case OBJECT -> {for(var member:value.members()){terms.add(jsonTextTerm(member.key()));json(member.value(),false,terms);}}
        }
    }
    static final class Evaluator {
        final Engine.Index index;final Indexes.Evaluator base;
        Evaluator(Engine.Index index,Engine.Table table,QueryEnvironment environment){this.index=index;base=new Indexes.Evaluator(index,table,environment);}
        List<byte[]> terms(List<SqlValue>row,CostMeter meter) {
            base.frame.values=row;
            return meter.unmetered(()->{
                if(base.predicate!=null){SqlValue accepted=base.predicate.run(meter);if(accepted.isNull()||!accepted.asBoolean())return List.of();}
                List<byte[]>result=new ArrayList<>();
                for(int i=0;i<index.keys().size();i++)for(byte[]term:GinIndex.terms(row.get(index.keys().get(i).column()))) {
                    if(index.keys().size()==1)result.add(term);else{byte[]tagged=new byte[term.length+1];tagged[0]=(byte)i;System.arraycopy(term,0,tagged,1,term.length);result.add(tagged);}
                }
                return result;
            });
        }
    }
    static Engine.Index build(Engine.Index index,Engine.Table table,SqlValue[]args,QueryEnvironment environment,Database database,long workMem)throws IOException {
        Evaluator evaluator=new Evaluator(index,table,environment);CostMeter meter=environment.meter();
        PersistentTree tree=PersistentTree.empty(database.pageSize(),TableLeafCodec.index(),database.pageCodec());
        try(ExternalSorter sorter=new ExternalSorter(List.of(SqlType.BYTEA,SqlType.BYTEA),workMem,database.scratchDirectory(),meter.memory(),(a,b)->{int c=Arrays.compareUnsigned(a.getFirst().asBytes(),b.getFirst().asBytes());return c!=0?c:Arrays.compareUnsigned(a.get(1).asBytes(),b.get(1).asBytes());});
            TableScan scan=new TableScan(table,null,table.columns().stream().map(Engine.Column::name).toList(),null,args,meter,evaluator.base.touched,environment)) {
            PersistentTree.Entry entry;
            while((entry=scan.next())!=null){meter.charge(Costs.STORAGE_ROW_READ);meter.guard();for(byte[]term:evaluator.terms(Encoding.row(entry,table.types()),meter))sorter.append(List.of(SqlValue.ofBytes(term),SqlValue.ofBytes(entry.key())));}
            try(var cursor=sorter.finish()){List<SqlValue>row;while((row=cursor.next())!=null)tree=GinPostingRuns.insert(tree,row.getFirst().asBytes(),row.get(1).asBytes());}
        }
        return index.changed(tree);
    }
    static final class Changes implements AutoCloseable {
        private final Evaluator evaluator;private final RowSpool changes;private final CostMeter meter;
        Changes(Engine.Index index,Engine.Table table,QueryEnvironment environment,long workMem,Path scratch){evaluator=new Evaluator(index,table,environment);meter=environment.meter();changes=new RowSpool(List.of(SqlType.BYTEA,SqlType.BYTEA,SqlType.BYTEA),workMem,scratch,meter.memory());}
        void add(List<SqlValue>oldRow,byte[]oldKey,List<SqlValue>newRow,byte[]newKey) {
            List<byte[]>oldTerms=oldRow==null?List.of():evaluator.terms(oldRow,meter),newTerms=newRow==null?List.of():evaluator.terms(newRow,meter);int a=0,b=0;
            while(a<oldTerms.size()||b<newTerms.size()) {
                int compare=a==oldTerms.size()?1:b==newTerms.size()?-1:Arrays.compareUnsigned(oldTerms.get(a),newTerms.get(b));
                byte[]term=compare<=0?oldTerms.get(a):newTerms.get(b);
                if(compare!=0||!Arrays.equals(oldKey,newKey))changes.append(List.of(SqlValue.ofBytes(term),compare<=0?SqlValue.ofBytes(oldKey):SqlValue.nullValue(SqlType.BYTEA),compare>=0?SqlValue.ofBytes(newKey):SqlValue.nullValue(SqlType.BYTEA)));
                if(compare<=0)a++;if(compare>=0)b++;
            }
        }
        PersistentTree apply(PersistentTree tree)throws IOException {
            return apply(tree, false);
        }
        PersistentTree apply(PersistentTree tree,boolean pairedUpdates)throws IOException {
            // Non-key UPDATE removes and inserts each row's term differences before
            // visiting the next row. Both passes retain only one pending spool row.
            if(pairedUpdates)try(var removals=changes.cursor();var insertions=changes.cursor()) {
                List<SqlValue> old=removals.next(),fresh=insertions.next();
                while(old!=null) {
                    byte[] key=storageKey(old);
                    do {if(!old.get(1).isNull())tree=GinPostingRuns.remove(tree,old.getFirst().asBytes(),old.get(1).asBytes());old=removals.next();}
                    while(old!=null&&Arrays.equals(key,storageKey(old)));
                    do {if(!fresh.get(2).isNull())tree=GinPostingRuns.insert(tree,fresh.getFirst().asBytes(),fresh.get(2).asBytes());fresh=insertions.next();}
                    while(fresh!=null&&Arrays.equals(key,storageKey(fresh)));
                }
                return tree;
            }
            return insert(remove(tree));
        }
        PersistentTree remove(PersistentTree tree)throws IOException {
            try(var cursor=changes.cursor()){List<SqlValue>row;while((row=cursor.next())!=null)if(!row.get(1).isNull())tree=GinPostingRuns.remove(tree,row.getFirst().asBytes(),row.get(1).asBytes());}
            return tree;
        }
        PersistentTree insert(PersistentTree tree)throws IOException {
            try(var cursor=changes.cursor()){List<SqlValue>row;while((row=cursor.next())!=null)if(!row.get(2).isNull())tree=GinPostingRuns.insert(tree,row.getFirst().asBytes(),row.get(2).asBytes());}
            return tree;
        }
        private static byte[] storageKey(List<SqlValue>row){return (row.get(1).isNull()?row.get(2):row.get(1)).asBytes();}
        public void close(){changes.close();}
    }
}
