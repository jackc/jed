package org.jed;

import java.util.ArrayList;
import java.util.List;

/** SQL-standard pattern translation and the four gated SIMILAR substring searches. */
final class SimilarFunctions {
    private SimilarFunctions() {}
    record Translation(String text,List<String> parts) {}
    static Translation translate(String pattern,String escape) {
        int[] escapes = escape.codePoints().toArray(); if (escapes.length > 1) throw new SqlException("22025","invalid escape string"); int esc = escapes.length == 0 ? -1 : escapes[0];
        ArrayList<String> parts = new ArrayList<>(); StringBuilder part = new StringBuilder(), whole = new StringBuilder("^(?:"); boolean afterEscape = false, inClass = false; int k = 0;
        for (int c : pattern.codePoints().toArray()) {
            String emitted;
            if (afterEscape) { afterEscape = false; if (c == '"' && !inClass) { parts.add(part.toString()); part.setLength(0); if (parts.size() > 2) throw new SqlException("2200C","SQL regular expression may not contain more than two escape-double-quote separators"); whole.append(parts.size() == 1 ? "){1,1}?(" : "){1,1}(?:"); continue; } emitted = "\\" + Character.toString(c); if (inClass) k=3; }
            else if (c == esc) { afterEscape = true; continue; }
            else if (inClass) { emitted = c == '\\' ? "\\\\" : Character.toString(c); if (c == ']' && k > 2) inClass=false; else if (c == '^') k++; else k=3; }
            else if (c == '[') { emitted="["; inClass=true; k=1; }
            else if (c == '%') emitted=".*";
            else if (c == '_') emitted=".";
            else if (c == '(') emitted="(?:";
            else if (c == '\\' || c == '.' || c == '^' || c == '$') emitted="\\"+Character.toString(c);
            else emitted=Character.toString(c);
            part.append(emitted); whole.append(emitted);
        }
        parts.add(part.toString()); whole.append(")$"); return new Translation(whole.toString(),List.copyOf(parts));
    }
    static ScalarPlan.Expression binary(SqlParser.Binary syntax,ScalarPlan.Resolver resolver) {
        if (!syntax.op().equals("similar") && !syntax.op().equals("not similar")) return null;
        var left=resolver.resolve(syntax.left(),SqlType.TEXT); SqlParser.Call translation=(SqlParser.Call)syntax.right(); var pattern=resolver.resolve(translation.args().get(0),SqlType.TEXT); var escape=translation.args().size()==1 ? ScalarPlan.constant(SqlValue.ofText("\\")) : resolver.resolve(translation.args().get(1),SqlType.TEXT);
        if(left.type().kind()!=SqlType.Kind.TEXT || pattern.type().kind()!=SqlType.Kind.TEXT || escape.type().kind()!=SqlType.Kind.TEXT) throw new SqlException("42804","SIMILAR TO operands must be text");
        String p=literal(translation.args().get(0)), e=translation.args().size()==1?"\\":literal(translation.args().get(1)); Translation constant=p!=null && e!=null?translate(p,e):null;
        RegexFunctions.ProgramSite site=new RegexFunctions.ProgramSite(syntax,constant==null?null:constant.text,"s",pattern.constant()&&escape.constant(),false);
        return new ScalarPlan.Expression(SqlType.BOOLEAN,meter->{meter.operation(); SqlValue source=left.run(meter), pv=pattern.run(meter), ev=escape.run(meter); if(pv.isNull()||ev.isNull()) return SqlValue.nullValue(SqlType.BOOLEAN); Translation translated=constant==null?translate(pv.asText(),ev.asText()):constant; if(constant==null) { meter.operation(); if(!pattern.constant()||!escape.constant()) meter.scalarInput(SqlText.encodedLength(translated.text)); } if(source.isNull()) return SqlValue.nullValue(SqlType.BOOLEAN); boolean matched=site.program(translated.text,"s",meter).search(source.asText(),0,meter)!=null; return SqlValue.ofBoolean(matched != syntax.op().startsWith("not"));},left.constant()&&pattern.constant()&&escape.constant());
    }
    static ScalarPlan.Expression resolve(SqlParser.Call syntax,ScalarPlan.Resolver resolver) {
        if(syntax.name().equals("similar_to_escape")) {
            if(syntax.args().size()<1||syntax.args().size()>2) throw missing(); var p=resolver.resolve(syntax.args().get(0),SqlType.TEXT); var e=syntax.args().size()==1?ScalarPlan.constant(SqlValue.ofText("\\")):resolver.resolve(syntax.args().get(1),SqlType.TEXT); if(p.type().kind()!=SqlType.Kind.TEXT || e.type().kind()!=SqlType.Kind.TEXT) throw missing();
            return new ScalarPlan.Expression(SqlType.TEXT,meter->{meter.operation();SqlValue pv=p.run(meter),ev=e.run(meter);if(pv.isNull()||ev.isNull())return SqlValue.nullValue(SqlType.TEXT);String text=translate(pv.asText(),ev.asText()).text;if(!p.constant()||!e.constant())meter.scalarInput(SqlText.encodedLength(text));return SqlValue.ofText(text);},p.constant()&&e.constant());
        }
        if(!syntax.name().equals("substring")||syntax.args().size()!=3)return null;
        var second=resolver.resolve(syntax.args().get(1),SqlType.TEXT); if(second.type().isInteger())return null;
        var source=resolver.resolve(syntax.args().get(0),SqlType.TEXT);var escape=resolver.resolve(syntax.args().get(2),SqlType.TEXT);if(source.type().kind()!=SqlType.Kind.TEXT||second.type().kind()!=SqlType.Kind.TEXT||escape.type().kind()!=SqlType.Kind.TEXT)throw missing();
        for(SqlParser.Expr arg:syntax.args()) if(arg instanceof SqlParser.Literal literal && literal.text().equalsIgnoreCase("null") && literal.kind()==Lexer.Kind.WORD)return ScalarPlan.constant(SqlValue.nullValue(SqlType.TEXT));
        String p=literal(syntax.args().get(1)),e=literal(syntax.args().get(2)); SlicePrograms initial=p!=null&&e!=null?new SlicePrograms(translate(p,e)):null; boolean fixed=second.constant()&&escape.constant(); final SlicePrograms[] cached={initial};
        return new ScalarPlan.Expression(SqlType.TEXT,meter->{meter.operation();SqlValue s=source.run(meter),pv=second.run(meter),ev=escape.run(meter);if(pv.isNull()||ev.isNull())return SqlValue.nullValue(SqlType.TEXT);SlicePrograms programs=cached[0];if(programs==null||!fixed){Translation t=translate(pv.asText(),ev.asText());if(!fixed)meter.scalarInput(SqlText.encodedLength(t.text));programs=new SlicePrograms(t);if(fixed)cached[0]=programs;}if(s.isNull())return SqlValue.nullValue(SqlType.TEXT);programs.charge(meter,fixed);String result=programs.search(s.asText(),meter);return result==null?SqlValue.nullValue(SqlType.TEXT):SqlValue.ofText(result);},source.constant()&&fixed);
    }
    static final class SlicePrograms {
        final List<RegexProgram> programs; final boolean simple;
        SlicePrograms(Translation translation) {
            List<String> parts=translation.parts; simple=parts.size()==1;
            if(simple)programs=List.of(RegexProgram.compile(translation.text,"s",false));
            else {String first=parts.get(0),middle=parts.get(1),last=parts.size()==3?parts.get(2):"";var values=new ArrayList<RegexProgram>();values.add(RegexProgram.gated("(?:"+first+")",false));values.add(RegexProgram.gated("(?:"+middle+")"+(parts.size()==3?"(?:"+last+")":"")+"$",false));if(parts.size()==3){values.add(RegexProgram.gated("(?:"+middle+")",false));values.add(RegexProgram.gated("(?:"+last+")$",true));}programs=List.copyOf(values);}
        }
        void charge(CostMeter meter,boolean fixed) { for(RegexProgram program:programs)if(fixed)meter.regexCompile(program,program);else program.chargeCompile(meter); }
        int size(){int size=0;for(RegexProgram program:programs)size+=program.size();return size;}
        String search(String subject,CostMeter meter) { int[] input=subject.codePoints().toArray();if(simple)return programs.getFirst().search(input,0,meter)==null?null:subject;boolean[] gates=new boolean[input.length+1];gates[0]=true;boolean[] first=programs.get(0).ends(input,gates,meter);RegexProgram.Match left=programs.get(1).gatedSearch(input,first,meter);if(left==null)return null;int start=left.start(0),end=input.length;if(programs.size()==4){gates[0]=false;gates[start]=true;boolean[] middle=programs.get(2).ends(input,gates,meter);RegexProgram.Match right=programs.get(3).gatedSearch(input,middle,meter);if(right==null)return null;end=right.start(0);}return new String(input,start,end-start);}
    }
    private static String literal(SqlParser.Expr expr){if(expr instanceof SqlParser.Literal literal && literal.kind()==Lexer.Kind.STRING)return literal.text();if(expr instanceof SqlParser.Cast cast && cast.target().name().equals("text"))return literal(cast.value());return null;}
    private static SqlException missing(){return new SqlException("42883","SIMILAR function argument types do not match");}
}
