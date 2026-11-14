package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.Set;

/** Name-based JSON record projection, retaining base fields and textual decoding order. */
final class JsonRecordPlan implements RelationPlan {
    private static final Set<String> FUNCTIONS = Set.of("json_to_record","jsonb_to_record","json_to_recordset","jsonb_to_recordset","json_populate_record","jsonb_populate_record","json_populate_recordset","jsonb_populate_recordset");
    static boolean recognized(String name) { return FUNCTIONS.contains(name); }
    private final boolean populate, set;
    private final ScalarPlan.Expression base, input;
    private final List<SqlType.Field> fields;
    private final List<ScalarPlan.Binding> bindings;
    private List<SqlValue> baseValues;
    private List<JsonNode> items;
    private String source;
    private int at;
    private boolean initialized;

    JsonRecordPlan(SqlParser.TableFunction syntax, QueryEnvironment environment) {
        String name=syntax.call().name(); populate=name.contains("populate");set=name.endsWith("set");var resolver=environment.resolver();
        if(syntax.call().args().size()!=(populate?2:1))throw new SqlException("42883","JSON record function argument count does not match");
        if(populate){base=resolver.resolve(syntax.call().args().get(0),null);if(base.type().kind()==SqlType.Kind.RECORD)throw new SqlException("0A000","anonymous record base is not supported");if(base.type().kind()!=SqlType.Kind.COMPOSITE)throw new SqlException("42804","populate_record base must be a composite");if(!syntax.definitions().isEmpty())throw new SqlException("42601","column definition list is not allowed for a named composite");fields=base.type().fields();}
        else {base=null;if(syntax.definitions().isEmpty())throw new SqlException("42601","record-returning function requires a column definition list");var values=new ArrayList<SqlType.Field>();var names=new java.util.HashSet<String>();for(SqlParser.Column column:syntax.definitions()){if(!names.add(column.name()))throw new SqlException("42701","duplicate column name");SqlType type=resolver.type(column.type());values.add(new SqlType.Field(column.name(),type,false));}fields=List.copyOf(values);}
        for(SqlType.Field field:fields)if(field.type().isComposite()||field.type().kind()==SqlType.Kind.ARRAY)throw new SqlException("0A000","structured JSON record output types are not supported");
        SqlType type=name.startsWith("jsonb_")?SqlType.JSONB:SqlType.JSON;input=resolver.resolve(syntax.call().args().get(populate?1:0),type);if(input.type()!=type)throw new SqlException("42883","JSON record function input type does not match");
        if(syntax.columns().size()>fields.size())throw new SqlException("42P10","too many JSON record column aliases");String label=syntax.alias()==null?name:syntax.alias();var output=new ArrayList<ScalarPlan.Binding>();for(int i=0;i<fields.size();i++)output.add(new ScalarPlan.Binding(List.of(label),i<syntax.columns().size()?syntax.columns().get(i):fields.get(i).name(),fields.get(i).type()));bindings=List.copyOf(output);
    }
    public List<ScalarPlan.Binding> bindings(){return bindings;}
    public List<String> names(){return bindings.stream().map(ScalarPlan.Binding::name).toList();}
    public List<SqlType> types(){return fields.stream().map(SqlType.Field::type).toList();}
    public void touch(boolean[] columns){}
    private void initialize(CostMeter meter){
        initialized=true;SqlValue original=base==null?null:base.run(meter);baseValues=new ArrayList<>();for(int i=0;i<fields.size();i++)baseValues.add(original==null||original.isNull()?SqlValue.nullValue(fields.get(i).type()):original.asComposite().get(i));
        SqlValue value=input.run(meter);if(value.isNull()){items=set?List.of():java.util.Collections.singletonList(null);return;}
        source=value.type()==SqlType.JSON?value.asJson():null;JsonNode root=source==null?value.asJsonb():JsonNode.parse(source,false);JsonNode.Kind expected=set?JsonNode.Kind.ARRAY:JsonNode.Kind.OBJECT;
        if(root.kind()!=expected){if(source!=null&&root.kind()!=JsonNode.Kind.ARRAY&&root.kind()!=JsonNode.Kind.OBJECT)JsonInput.decodedValidation(source);throw wrongKind();}
        items=set?root.elements():List.of(root);
    }
    public Row next(CostMeter meter){
        if(!initialized)initialize(meter);if(at==items.size())return null;JsonNode object=items.get(at++);String raw=null;
        if(object!=null){
            if(object.kind()!=JsonNode.Kind.OBJECT){if(source!=null&&object.kind()!=JsonNode.Kind.ARRAY)JsonInput.decodedValidation(source.substring(object.start,object.end));throw wrongKind();}
            if(source!=null){raw=source.substring(object.start,object.end);object=JsonInput.decodedValidation(raw);}
        }
        ArrayList<SqlValue> values=new ArrayList<>();
        for(int i=0;i<fields.size();i++){SqlType.Field field=fields.get(i);JsonNode member=object==null?null:object.get(field.name());SqlValue value=member==null?baseValues.get(i):convert(member,raw,field.type(),meter);if(field.notNull()&&value.isNull())throw new SqlException("23502","NULL composite field");values.add(value);}
        meter.charge(Costs.GENERATED_ROW);meter.guard();return new Row(names(),values);
    }
    private static SqlValue convert(JsonNode node,String source,SqlType type,CostMeter meter){
        if(node.kind()==JsonNode.Kind.NULL)return SqlValue.nullValue(type);
        String image=source==null?node.render():source.substring(node.start,node.end);
        if(type==SqlType.JSON)return SqlValue.ofJson(image);if(type==SqlType.JSONB)return SqlValue.ofJsonb(source==null?node:JsonNode.parse(image,true));
        String text=node.kind()==JsonNode.Kind.STRING?node.string():image;return SqlCasts.literal(text,type,meter);
    }
    private static SqlException wrongKind(){return new SqlException("22023","JSON record function requires an object or an array of objects");}
}
