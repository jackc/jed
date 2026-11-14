package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.Set;

/** SQL regex functions share the same metered program and non-overlapping search iterator. */
final class RegexFunctions {
    private RegexFunctions() {}
    static final Set<String> NAMES = Set.of("regexp_like", "regexp_count", "regexp_instr", "regexp_substr", "regexp_match", "regexp_replace", "regexp_split_to_array", "regexp_matches", "regexp_split_to_table");
    static ScalarPlan.Expression binary(SqlParser.Binary syntax, ScalarPlan.Resolver resolver) {
        ScalarPlan.Expression similar = SimilarFunctions.binary(syntax,resolver); if (similar != null) return similar;
        if (!Set.of("~", "~*", "!~", "!~*").contains(syntax.op())) return null;
        var left = resolver.resolve(syntax.left(),SqlType.TEXT); var right = resolver.resolve(syntax.right(),SqlType.TEXT);
        if (left.type().kind() != SqlType.Kind.TEXT || right.type().kind() != SqlType.Kind.TEXT) throw new SqlException("42804","regular expression operands must be text");
        String flags = syntax.op().endsWith("*") ? "i" : ""; ProgramSite site = new ProgramSite(syntax, literal(syntax.right()),flags,right.constant(),false);
        return new ScalarPlan.Expression(SqlType.BOOLEAN,meter -> { meter.operation(); SqlValue a = left.run(meter), b = right.run(meter); if (a.isNull() || b.isNull()) return SqlValue.nullValue(SqlType.BOOLEAN); RegexProgram program = site.program(b.asText(),flags,meter); boolean found = program.search(a.asText(),0,meter) != null; return SqlValue.ofBoolean(found != syntax.op().startsWith("!")); },left.constant() && right.constant());
    }
    static ScalarPlan.Expression resolve(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        String name = call.name();
        ScalarPlan.Expression similar = SimilarFunctions.resolve(call,resolver); if (similar != null) return similar;
        if (name.equals("substring") && call.args().size() == 2) { ScalarPlan.Expression second = resolver.resolve(call.args().get(1),SqlType.TEXT); if (second.type().kind() != SqlType.Kind.TEXT && second.type() != SqlType.UNKNOWN) return null; }
        else if (!NAMES.contains(name) || name.equals("regexp_matches") || name.equals("regexp_split_to_table")) return null;
        Invocation invocation = invocation(call,resolver);
        return new ScalarPlan.Expression(invocation.type,meter -> invocation.evaluate(meter),invocation.constant);
    }
    static Invocation invocation(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        String name = call.name(); int count = call.args().size();
        int max = switch (name) { case "regexp_replace" -> 4; case "regexp_count" -> 4; case "regexp_substr" -> 6; case "regexp_instr" -> 7; case "substring" -> 2; default -> 3; };
        if (count < (name.equals("regexp_replace") ? 3 : 2) || count > max) throw missing();
        int flagIndex = switch (name) { case "regexp_replace" -> 3; case "regexp_count" -> 3; case "regexp_substr" -> 4; case "regexp_instr" -> 5; case "substring" -> -1; default -> 2; };
        ArrayList<ScalarPlan.Expression> arguments = new ArrayList<>();
        for (int i = 0; i < count; i++) { boolean text = i < 2 || i == flagIndex || name.equals("regexp_replace"); var expression = resolver.resolve(call.args().get(i),text ? SqlType.TEXT : SqlType.I32); if (text ? expression.type().kind() != SqlType.Kind.TEXT : !expression.type().isInteger()) throw missing(); arguments.add(expression); }
        String flags = flagIndex < 0 || flagIndex >= count ? "" : literal(call.args().get(flagIndex));
        boolean fixed = arguments.get(1).constant() && (flagIndex < 0 || flagIndex >= count || arguments.get(flagIndex).constant());
        ProgramSite site = new ProgramSite(call, literal(call.args().get(1)),flags,fixed,false);
        SqlType type = switch (name) { case "regexp_like" -> SqlType.BOOLEAN; case "regexp_count", "regexp_instr" -> SqlType.I32; case "regexp_match", "regexp_matches", "regexp_split_to_array" -> SqlType.array(SqlType.TEXT); default -> SqlType.TEXT; };
        return new Invocation(name,List.copyOf(arguments),flagIndex,site,type,arguments.stream().allMatch(ScalarPlan.Expression::constant));
    }
    static final class Invocation {
        final String name; final List<ScalarPlan.Expression> args; final int flagIndex; final ProgramSite site; final SqlType type; final boolean constant;
        Invocation(String name,List<ScalarPlan.Expression> args,int flagIndex,ProgramSite site,SqlType type,boolean constant) { this.name=name; this.args=args; this.flagIndex=flagIndex; this.site=site; this.type=type; this.constant=constant; }
        SqlValue evaluate(CostMeter meter) { List<SqlValue> rows = execute(meter); if (rows == null) return SqlValue.nullValue(type); if (name.equals("regexp_split_to_array")) return SqlValue.ofArray(SqlType.array(SqlType.TEXT),SqlArray.vector(rows)); return rows.isEmpty() ? SqlValue.nullValue(type) : rows.getFirst(); }
        List<SqlValue> rows(CostMeter meter) { List<SqlValue> rows = execute(meter); return rows == null ? List.of() : rows; }
        private List<SqlValue> execute(CostMeter meter) {
            meter.operation(); ArrayList<SqlValue> values = new ArrayList<>(); for (var argument : args) { SqlValue value = argument.run(meter); if (value.isNull()) return null; values.add(value); }
            String subject = values.get(0).asText(), pattern = values.get(1).asText(), flags = flagIndex >= 0 && flagIndex < values.size() ? values.get(flagIndex).asText() : "";
            validateFlags(name,flags);
            long start = 1, occurrence = 1, group = 0, endOption = 0;
            if (Set.of("regexp_count","regexp_substr","regexp_instr").contains(name)) { if (values.size() > 2) start = values.get(2).asInteger(); positive("start",start); }
            if (name.equals("regexp_substr") || name.equals("regexp_instr")) { if (values.size() > 3) occurrence = values.get(3).asInteger(); positive("n",occurrence); }
            if (name.equals("regexp_instr") && values.size() > 4) { endOption = values.get(4).asInteger(); if (endOption != 0 && endOption != 1) throw parameter("endoption",endOption); }
            int groupIndex = name.equals("regexp_instr") ? 6 : name.equals("regexp_substr") ? 5 : -1;
            if (groupIndex >= 0 && values.size() > groupIndex) { group = values.get(groupIndex).asInteger(); if (group < 0) throw parameter("subexpr",group); }
            RegexProgram program = site.program(pattern,flags,meter); int[] original = subject.codePoints().toArray(), input = program.input(subject); long[] byteOffsets = byteOffsets(original); int at = start > Integer.MAX_VALUE ? Integer.MAX_VALUE : (int)start - 1;
            if (name.equals("substring")) group = program.groups() == 0 ? 0 : 1;
            if (group == 1 && program.groups() == 0) group = 0;
            ArrayList<RegexProgram.Match> matches = new ArrayList<>(); long found = 0;
            boolean all = name.equals("regexp_count") || name.startsWith("regexp_split") || (name.equals("regexp_replace") || name.equals("regexp_matches")) && flags.indexOf('g') >= 0;
            long captureBytes = 0;
            while (at <= input.length) {
                RegexProgram.Match match = program.search(input,at,meter); if (match == null) break; found++;
                if (name.equals("regexp_count")) { }
                else if (all || found == occurrence) {
                    if (name.equals("regexp_match") || name.equals("regexp_matches")) { long bytes = captureBytes(program,match,byteOffsets); meter.scalarInput(bytes); captureBytes = CostMeter.saturatingAdd(captureBytes,bytes); }
                    matches.add(match);
                }
                if (!all && found == occurrence) break;
                int end = match.end(0); at = end == match.start(0) ? end + 1 : end;
            }
            if (name.equals("regexp_count")) return List.of(SqlValue.ofInteger(SqlType.I32,found));
            if (name.equals("regexp_like")) return List.of(SqlValue.ofBoolean(!matches.isEmpty()));
            if (name.equals("regexp_match") || name.equals("regexp_matches")) { meter.scalarReserve(captureBytes); ArrayList<SqlValue> output = new ArrayList<>(); for (var match : matches) output.add(captures(program,match,original)); return output; }
            if (name.startsWith("regexp_split")) { ArrayList<SqlValue> pieces = new ArrayList<>(); int previous = 0, pieceStart = 0; for (var match : matches) { int begin = match.start(0), end = match.end(0); if (begin != original.length && end > previous) { pieces.add(SqlValue.ofText(slice(original,pieceStart,begin))); pieceStart=end; } previous=end; } pieces.add(SqlValue.ofText(slice(original,pieceStart,original.length))); return pieces; }
            if (name.equals("regexp_replace")) return List.of(replace(original,byteOffsets,matches,values.get(2).asText(),meter));
            RegexProgram.Match match = matches.isEmpty() ? null : matches.getFirst(); int begin = match == null || group > program.groups() ? -1 : match.start((int)group), end = begin < 0 ? -1 : match.end((int)group);
            if (name.equals("regexp_instr")) return List.of(SqlValue.ofInteger(SqlType.I32,begin < 0 ? 0 : (endOption == 0 ? begin : end) + 1));
            return begin < 0 ? List.of() : List.of(SqlValue.ofText(slice(original,begin,end)));
        }
    }
    private static long captureBytes(RegexProgram program,RegexProgram.Match match,long[] byteOffsets) { long bytes = MemorySchedule.ARRAY_DIM; int first = program.groups() == 0 ? 0 : 1, last = Math.max(first,program.groups()); for (int group = first; group <= last; group++) { bytes = CostMeter.saturatingAdd(bytes,MemorySchedule.VALUE); int begin = match.start(group), end = match.end(group); if (begin >= 0) bytes = CostMeter.saturatingAdd(bytes,byteOffsets[end] - byteOffsets[begin]); } return bytes; }
    private static SqlValue captures(RegexProgram program,RegexProgram.Match match,int[] source) { ArrayList<SqlValue> output = new ArrayList<>(); int first = program.groups() == 0 ? 0 : 1, last = Math.max(first,program.groups()); for (int group = first; group <= last; group++) output.add(match.start(group) < 0 ? SqlValue.nullValue(SqlType.TEXT) : SqlValue.ofText(slice(source,match.start(group),match.end(group)))); return SqlValue.ofArray(SqlType.array(SqlType.TEXT),SqlArray.vector(output)); }
    private static SqlValue replace(int[] source,long[] byteOffsets,List<RegexProgram.Match> matches,String template,CostMeter meter) {
        int[] replacement = template.codePoints().toArray(); long bytes = 0; int previous = 0;
        for (var match : matches) { meter.scalarInput(SqlText.encodedLength(template)); bytes = CostMeter.saturatingAdd(bytes,byteOffsets[match.start(0)] - byteOffsets[previous]); bytes = CostMeter.saturatingAdd(bytes,template(replacement,source,byteOffsets,match,null)); previous=match.end(0); }
        bytes = CostMeter.saturatingAdd(bytes,byteOffsets[source.length] - byteOffsets[previous]); meter.scalarOutput(bytes);
        StringBuilder output = new StringBuilder(); previous=0; for (var match : matches) { output.append(slice(source,previous,match.start(0))); template(replacement,source,byteOffsets,match,output); previous=match.end(0); } output.append(slice(source,previous,source.length)); return SqlValue.ofText(output.toString());
    }
    private static long template(int[] template,int[] source,long[] byteOffsets,RegexProgram.Match match,StringBuilder out) { long bytes=0; for (int i=0;i<template.length;i++) { int c=template[i]; if (c=='\\' && i+1<template.length) { int next=template[i+1]; if (next=='&' || next>='1' && next<='9') { i++; int group=next=='&'?0:next-'0', start=match.start(group), end=match.end(group); if (start>=0) { bytes=CostMeter.saturatingAdd(bytes,byteOffsets[end] - byteOffsets[start]); if(out!=null) out.append(slice(source,start,end)); } continue; } if(next=='\\') { i++; c='\\'; } } bytes=CostMeter.saturatingAdd(bytes,width(c)); if(out!=null) out.appendCodePoint(c); } return bytes; }
    private static int width(int c) { return c < 0x80 ? 1 : c < 0x800 ? 2 : c < 0x10000 ? 3 : 4; }
    private static long[] byteOffsets(int[] points) { long[] offsets = new long[points.length + 1]; for (int i = 0; i < points.length; i++) offsets[i + 1] = offsets[i] + width(points[i]); return offsets; }
    private static String slice(int[] points,int begin,int end) { return new String(points,begin,end-begin); }
    static void validateFlags(String name,String flags) { for (int c : flags.codePoints().toArray()) if(c!='i' && c!='g') throw RegexProgram.invalid("invalid option: " + Character.toString(c)); if(flags.indexOf('g')>=0 && !name.equals("regexp_replace") && !name.equals("regexp_matches")) throw new SqlException("22023",name+"() does not support the \"global\" option"); }
    private static void positive(String name,long value) { if(value<1) throw parameter(name,value); }
    private static SqlException parameter(String name,long value) { return new SqlException("22023","invalid value for parameter \""+name+"\": "+value); }
    private static SqlException missing() { return new SqlException("42883","regular expression function argument types do not match"); }
    private static String literal(SqlParser.Expr expression) { if(expression instanceof SqlParser.Literal literal && literal.kind()==Lexer.Kind.STRING) return literal.text(); if(expression instanceof SqlParser.Cast cast && cast.target().name().equals("text")) return literal(cast.value()); return null; }
    static final class ProgramSite {
        private RegexProgram program; private final boolean fixed, jsonPath; private final Object node;
        ProgramSite(Object node,String pattern,String flags,boolean fixed,boolean jsonPath) { this.node=node; this.fixed=fixed; this.jsonPath=jsonPath; if(pattern!=null && flags!=null) program=RegexProgram.compile(pattern,flags,jsonPath); }
        RegexProgram program(String pattern,String flags,CostMeter meter) { if(program==null || !fixed) { RegexProgram compiled=RegexProgram.compile(pattern,flags,jsonPath); if(fixed) program=compiled; else { compiled.chargeCompile(meter); return compiled; } } meter.regexCompile(node,program); return program; }
    }
}
