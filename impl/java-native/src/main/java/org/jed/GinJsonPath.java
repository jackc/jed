package org.jed;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.TreeSet;

/** Sound jsonb_ops term-tree extraction; residual jsonpath always verifies candidates. */
final class GinJsonPath {
    record Node(boolean any,List<byte[]>terms,List<Node>children) {}
    private record Context(List<String>keys,String last) {}
    private record Walk(Context context,Node filters,boolean complete) {}
    private final boolean strict;
    private GinJsonPath(boolean strict){this.strict=strict;}
    static Node extract(JsonPath path,boolean exists) {
        GinJsonPath extractor=new GinJsonPath(path.strict());Context root=new Context(List.of(),"root");
        return exists ? path.root().predicate()?null:extractor.walk(path.root(),root).filters()
            : path.root().predicate()?extractor.predicate(path.root(),root,false):null;
    }
    private Node predicate(JsonPath.Node node,Context context,boolean negate) {
        if(node.kind().equals("not"))return predicate(node.args().getFirst(),context,!negate);
        if(node.kind().equals("and")||node.kind().equals("or"))return combine(node.kind().equals("or")^negate,predicate(node.args().get(0),context,negate),predicate(node.args().get(1),context,negate));
        if(negate)return null;
        if(node.kind().equals("exists"))return walk(node.args().getFirst(),context).filters();
        if(!node.kind().equals("compare")||!node.text().equals("=="))return null;
        JsonNode scalar=scalar(node.args().getFirst());JsonPath.Node access=node.args().get(1);
        if(scalar==null){scalar=scalar(node.args().get(1));access=node.args().getFirst();}if(scalar==null)return null;
        Walk walked=walk(access,context);if(!walked.complete())return walked.filters();
        List<Node>parts=new ArrayList<>();parts.add(walked.filters());
        for(String key:walked.context().keys())parts.add(leaf(GinIndex.jsonTextTerm(key)));
        if(scalar.kind()==JsonNode.Kind.STRING) {
            Node key=leaf(GinIndex.jsonTextTerm(scalar.string())),value=leaf(GinIndex.jsonValueTerm(scalar));
            String last=walked.context().last();parts.add(!strict?combine(true,key,value):List.of("member","members").contains(last)?value:List.of("elements","index").contains(last)?key:combine(true,key,value));
        } else parts.add(leaf(GinIndex.jsonValueTerm(scalar)));
        return combine(false,parts.toArray(Node[]::new));
    }
    private Walk walk(JsonPath.Node node,Context context) {
        if(node.kind().equals("root"))return new Walk(new Context(List.of(),"root"),null,true);
        if(node.kind().equals("current"))return new Walk(context,null,true);
        if(!List.of("member","members","elements","any","index","filter","method").contains(node.kind()))return new Walk(context,null,false);
        Walk base=walk(node.args().getFirst(),context);
        if(!base.complete())return base;
        if(node.kind().equals("filter"))return new Walk(base.context(),combine(false,base.filters(),predicate(node.args().get(1),base.context(),false)),base.complete());
        if(!base.complete()||node.kind().equals("method"))return new Walk(base.context(),base.filters(),false);
        List<String>keys=new ArrayList<>(base.context().keys());if(node.kind().equals("member"))keys.add(node.text());
        return new Walk(new Context(keys,node.kind()),base.filters(),true);
    }
    private static JsonNode scalar(JsonPath.Node node) {
        return switch(node.kind()) {
            case "string" -> JsonNode.string(node.text());
            case "number" -> JsonNode.number(Decimal.parse(node.text()));
            case "literal" -> switch(node.text()){case "null"->JsonNode.NULL;case "true"->JsonNode.bool(true);case "false"->JsonNode.bool(false);default->null;};
            default -> null;
        };
    }
    private static Node leaf(byte[]term){return new Node(false,List.of(term),List.of());}
    private static Node combine(boolean any,Node...items) {
        TreeSet<byte[]>terms=new TreeSet<>(Arrays::compareUnsigned);List<Node>children=new ArrayList<>();
        for(Node node:items){if(node==null){if(any)return null;continue;}if(node.children().isEmpty()&&node.terms().size()==1 || node.any()==any){terms.addAll(node.terms());children.addAll(node.children());}else children.add(node);}
        if(terms.isEmpty()&&children.isEmpty())return null;if(terms.isEmpty()&&children.size()==1)return children.getFirst();
        return new Node(any,List.copyOf(terms),List.copyOf(children));
    }
    static GinAccess.Bound bound(Engine.Index index,int key,Node node) {
        List<byte[]>terms=new ArrayList<>();for(byte[]term:node.terms()){if(index.keys().size()==1)terms.add(term);else{byte[]tagged=new byte[term.length+1];tagged[0]=(byte)key;System.arraycopy(term,0,tagged,1,term.length);terms.add(tagged);}}
        return new GinAccess.Bound(index,terms,node.any(),node.children().stream().map(child->bound(index,key,child)).toList());
    }
}
