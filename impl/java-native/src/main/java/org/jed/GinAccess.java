package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.TreeSet;
import org.jed.storage.GinPostingRuns;
import org.jed.storage.PersistentTree;

/** Constant GIN bounds, canonical posting gathers, and leaf-group skip-scan costs. */
final class GinAccess {
    record Bound(Engine.Index index,List<byte[]>terms,boolean any,List<Bound> arms,boolean fullScan,SqlParser.Expr predicate) {
        Bound(Engine.Index index,List<byte[]>terms,boolean any,List<Bound>arms,boolean fullScan) { this(index,terms,any,arms,fullScan,null); }
        Bound withPredicate(SqlParser.Expr expression) { return new Bound(index,terms,any,arms,fullScan,expression); }
        Bound(Engine.Index index,List<byte[]>terms,boolean any,List<Bound>arms) { this(index,terms,any,arms,false); }
        Bound(Engine.Index index,List<byte[]>terms,boolean any) { this(index,terms,any,List.of()); }
    }
    private GinAccess() {}
    static Bound find(Engine.Table table,List<String>names,String alias,SqlParser.Expr predicate,ScalarPlan.Resolver constants,CostMeter meter) {
        return find(table,names,alias,predicate,constants,meter,false);
    }
    static Bound find(Engine.Table table,List<String>names,String alias,SqlParser.Expr predicate,ScalarPlan.Resolver constants,CostMeter meter,boolean knownOnly) {
        return find(table,names,alias,predicate,constants,meter,null,true,knownOnly);
    }
    private static Bound find(Engine.Table table,List<String>names,String alias,SqlParser.Expr predicate,ScalarPlan.Resolver constants,CostMeter meter,Integer onlyColumn,boolean disjunction,boolean knownOnly) {
        List<SqlParser.Expr>conjuncts=FilterRules.terms(predicate);
        for(Engine.Index index:table.indexes().stream().filter(value->value.kind()==1 && StoredDefinitions.parsed(value)).sorted((a,b)->SqlText.compare(a.name(),b.name())).toList()) {
            if(onlyColumn==null && index.predicate()!=null && FilterRules.terms(index.predicate().expression()).stream().anyMatch(required->conjuncts.stream().noneMatch(required::equals)))continue;
            for(int k=0;k<index.keys().size();k++) {
                if(onlyColumn!=null && onlyColumn!=k)continue;
                var key=index.keys().get(k);String name=names.get(key.column());
                for(var conjunct:conjuncts) {
                    if(knownOnly && !GinEstimate.knownOperand(conjunct,names,alias))continue;
                    String strategy=null;SqlParser.Expr operand=null;
                    if(conjunct instanceof SqlParser.Quantified quantified && quantified.operator().equals("=") && quantified.quantifier().equals("any") && quantified.query()==null && column(quantified.array(),name,alias) && key.type().kind()==SqlType.Kind.ARRAY){strategy="member";operand=quantified.subject();}
                    if(conjunct instanceof SqlParser.Binary binary) {
                        if(column(binary.left(),name,alias) && List.of("@>","&&","=","?","?|","?&","@?","@@").contains(binary.op())){strategy=binary.op();operand=binary.right();}
                        else if(column(binary.right(),name,alias) && (List.of("&&","=").contains(binary.op()) || binary.op().equals("<@") && key.type().kind()==SqlType.Kind.JSONB)){strategy=binary.op().equals("<@")?"@>":binary.op();operand=binary.left();}
                    }
                    if(strategy==null)continue;
                    boolean json=key.type().kind()==SqlType.Kind.JSONB;
                    if(json && List.of("=","&&","member").contains(strategy) || !json && List.of("?","?|","?&","@?","@@").contains(strategy))continue;
                    SqlType expected=List.of("@?","@@").contains(strategy)?SqlType.JSONPATH:strategy.equals("member")?key.type().element():strategy.equals("?")?SqlType.TEXT:List.of("?|","?&").contains(strategy)?SqlType.array(SqlType.TEXT):key.type();
                    ScalarPlan.Expression expression;
                    try {expression=constants.resolve(operand,expected);}catch(SqlException failure){if(List.of("42703","42P01").contains(failure.sqlState()))continue;throw failure;}
                    if(!expression.constant() && !(operand instanceof SqlParser.Name))continue;
                    SqlValue value=meter.unmetered(()->expression.run(meter));boolean any=List.of("&&","?|").contains(strategy);
                    if(List.of("@?","@@").contains(strategy) && !value.isNull()) {
                        GinJsonPath.Node extracted=GinJsonPath.extract(value.asJsonPath(),strategy.equals("@?"));
                        if(extracted==null)return new Bound(index,List.of(),false,List.of(),true).withPredicate(conjunct);return GinJsonPath.bound(index,k,extracted).withPredicate(conjunct);
                    }
                    List<byte[]>terms;
                    if(value.isNull())terms=List.of();
                    else if(json) {
                        if(strategy.equals("@>"))terms=GinIndex.terms(value);
                        else if(strategy.equals("?"))terms=List.of(GinIndex.jsonTextTerm(value.asText()));
                        else {TreeSet<byte[]>set=new TreeSet<>(Arrays::compareUnsigned);for(var element:value.asArray().elements())if(!element.isNull())set.add(GinIndex.jsonTextTerm(element.asText()));terms=List.copyOf(set);}
                        if(terms.isEmpty() && !any)return new Bound(index,List.of(),false,List.of(),true).withPredicate(conjunct);
                    } else {
                        TreeSet<byte[]>set=new TreeSet<>(Arrays::compareUnsigned);boolean impossible=false;
                        List<SqlValue>elements=strategy.equals("member")?List.of(value):value.asArray().elements();
                        for(SqlValue element:elements) {
                            if(element.isNull()){if(strategy.equals("@>"))impossible=true;continue;}
                            SqlValue converted=convert(element,key.type().element());
                            if(converted==null){if(!any)impossible=true;continue;}set.add(GinIndex.elementTerm(converted));
                        }
                        terms=impossible?List.of():List.copyOf(set);
                        if(!impossible && terms.isEmpty() && !any && !strategy.equals("member"))return new Bound(index,List.of(),false,List.of(),true).withPredicate(conjunct);
                    }
                    if(index.keys().size()>1){List<byte[]>tagged=new ArrayList<>();for(byte[]term:terms){byte[]bytes=new byte[term.length+1];bytes[0]=(byte)k;System.arraycopy(term,0,bytes,1,term.length);tagged.add(bytes);}terms=tagged;}
                    return new Bound(index,terms,any).withPredicate(conjunct);
                }
            }
        }
        if(disjunction)for(Engine.Index index:table.indexes().stream().filter(value->value.kind()==1 && StoredDefinitions.parsed(value)).sorted((a,b)->SqlText.compare(a.name(),b.name())).toList()) {
            if(index.predicate()!=null && FilterRules.terms(index.predicate().expression()).stream().anyMatch(required->conjuncts.stream().noneMatch(required::equals)))continue;
            for(int k=0;k<index.keys().size();k++)for(var conjunct:conjuncts) {
                if(knownOnly && !GinEstimate.knownOperand(conjunct,names,alias))continue;
                List<SqlParser.Expr>branches=new ArrayList<>();
                if(conjunct instanceof SqlParser.Binary binary && binary.op().equals("or"))flattenOr(conjunct,branches);
                else if(conjunct instanceof SqlParser.In in && !in.negated() && in.query()==null && column(in.subject(),names.get(index.keys().get(k).column()),alias))for(var value:in.values())branches.add(new SqlParser.Binary("=",in.subject(),value));
                else continue;
                List<Bound>arms=new ArrayList<>();boolean usable=true;
                for(var branch:branches){Bound arm=find(table.withIndexes(List.of(index)),names,alias,branch,constants,meter,k,false,knownOnly);if(arm==null){usable=false;break;}arms.add(arm);}
                if(usable)return new Bound(index,List.of(),true,arms,arms.stream().anyMatch(Bound::fullScan)).withPredicate(conjunct);
            }
        }
        return null;
    }
    private static void flattenOr(SqlParser.Expr expression,List<SqlParser.Expr>result){if(expression instanceof SqlParser.Binary binary && binary.op().equals("or")){flattenOr(binary.left(),result);flattenOr(binary.right(),result);}else result.add(expression);}
    private static SqlValue convert(SqlValue value,SqlType target) {
        if(value.type().equals(target))return value;
        if(target.kind()==SqlType.Kind.F32 && value.type().kind()==SqlType.Kind.F64 && !Double.isNaN(value.asFloat()) && (double)(float)value.asFloat()!=value.asFloat())return null;
        if(target.isInteger() && value.type().kind()==SqlType.Kind.DECIMAL && value.asDecimal().compareTo(value.asDecimal().truncate(0))!=0)return null;
        try{return SqlCasts.cast(value,target);}catch(SqlException failure){if(failure.sqlState().equals("22003"))return null;throw failure;}
    }
    private static boolean column(SqlParser.Expr expression,String name,String alias){return expression instanceof SqlParser.Name ref && ref.parts().getLast().equals(name) && (ref.parts().size()==1||ref.parts().size()==2&&ref.parts().getFirst().equals(alias));}
    private record Term(byte[]key,long span,long records,long bytes,boolean peeked) {}
    static final class Candidates implements AutoCloseable {
        private final CostMeter meter;private final TreeSet<byte[]>keys=new TreeSet<>(Arrays::compareUnsigned);private long retained;
        Candidates(CostMeter meter){this.meter=meter;}
        void add(byte[]key){if(keys.contains(key))return;long bytes=MemorySchedule.ROW+MemorySchedule.VALUE+key.length;meter.memory().reserve(bytes);retained+=bytes;keys.add(key);}
        void remove(byte[]key){if(keys.remove(key)){long bytes=MemorySchedule.ROW+MemorySchedule.VALUE+key.length;retained-=bytes;meter.memory().release(bytes);}}
        List<byte[]>keys(){return new ArrayList<>(keys);}
        public void close(){meter.memory().release(retained);retained=0;keys.clear();}
    }
    static Candidates gather(Bound bound,CostMeter meter)throws IOException {
        Candidates result=new Candidates(meter);PersistentTree tree=bound.index().entries();
        try {
            if(!bound.arms().isEmpty()) {
                boolean initialized=!bound.terms().isEmpty();
                if(initialized)try(Candidates leaves=gather(new Bound(bound.index(),bound.terms(),bound.any()),meter)){for(byte[]key:leaves.keys)result.add(key);}
                for(Bound arm:bound.arms()) {
                    if(initialized && !bound.any() && result.keys.isEmpty())break;
                    try(Candidates candidates=gather(arm,meter)) {
                        if(!initialized || bound.any())for(byte[]key:candidates.keys)result.add(key);
                        else for(byte[]key:result.keys())if(!candidates.keys.contains(key))result.remove(key);
                    }
                    initialized=true;
                }
                return result;
            }
            if(bound.terms().isEmpty())return result;
            if(bound.any() || bound.terms().size()==1) {
                for(byte[]term:bound.terms()){charge(meter,tree.rangePageCount(term,true,Indexes.after(term),false),Costs.PAGE_READ);read(tree,term,result,meter);}return result;
            }
            int height=tree.height();List<Term>terms=new ArrayList<>();
            for(byte[]term:bound.terms()) {
                long span=tree.rangePageCount(term,true,Indexes.after(term),false),records=Long.MAX_VALUE,bytes=Long.MAX_VALUE;boolean peeked=span==height;
                if(peeked){charge(meter,span,Costs.PAGE_READ);records=0;bytes=0;try(var cursor=tree.cursor(term,true,Indexes.after(term),false)){PersistentTree.Entry entry;while((entry=cursor.next())!=null){records++;bytes+=entry.key().length;}}}
                terms.add(new Term(term,span,records,bytes,peeked));
            }
            terms.sort((a,b)->{int c=Long.compare(a.span(),b.span());if(c==0)c=Long.compare(a.records(),b.records());if(c==0)c=Long.compare(a.bytes(),b.bytes());return c!=0?c:Arrays.compareUnsigned(a.key(),b.key());});
            Term driver=terms.getFirst();if(!driver.peeked())charge(meter,driver.span(),Costs.PAGE_READ);read(tree,driver.key(),result,meter);
            for(int termAt=1;termAt<terms.size()&&!result.keys.isEmpty();termAt++) {
                byte[]term=terms.get(termAt).key();List<byte[]>candidates=result.keys();int start=0;
                while(start<candidates.size()) {
                    int end=start;byte[]lower=join(term,candidates.get(start));
                    while(end+1<candidates.size() && tree.rangePageCount(lower,true,join(term,candidates.get(end+1)),true)==height)end++;
                    byte[]rangeEnd=join(term,candidates.get(end));
                    for(int i=start;i<=end;i++) {
                        byte[]key=candidates.get(i);PersistentTree.Entry run;
                        try(var cursor=tree.cursor(join(term,key),true,Indexes.after(term),false)){run=cursor.next();}
                        if(i==end && run!=null)rangeEnd=run.key();
                        charge(meter,1,Costs.GIN_ENTRY);
                        if(run==null || java.util.Collections.binarySearch(GinPostingRuns.decode(run.key(),term),key,Arrays::compareUnsigned)<0)result.remove(key);
                    }
                    charge(meter,tree.rangePageCount(lower,true,rangeEnd,true),Costs.PAGE_READ);start=end+1;
                }
            }
            return result;
        } catch(IOException|RuntimeException failure){result.close();throw failure;}
    }
    private static void read(PersistentTree tree,byte[]term,Candidates result,CostMeter meter)throws IOException{try(var cursor=new GinPostingRuns.Cursor(tree,term)){byte[]key;while((key=cursor.next())!=null){charge(meter,1,Costs.GIN_ENTRY);result.add(key);}}}
    private static void charge(CostMeter meter,long count,long cost){meter.charge(count*cost);meter.guard();}
    private static byte[]join(byte[]a,byte[]b){byte[]bytes=Arrays.copyOf(a,a.length+b.length);System.arraycopy(b,0,bytes,a.length,b.length);return bytes;}
}
