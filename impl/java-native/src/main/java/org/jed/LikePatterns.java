package org.jed;

import java.util.ArrayList;
import java.util.Set;

/** Unicode-code-point LIKE matching with a bounded, handwritten wildcard walk. */
final class LikePatterns {
    private LikePatterns() {}
    static ScalarPlan.Expression binary(SqlParser.Binary syntax, ScalarPlan.Resolver resolver) {
        if (!Set.of("like", "not like", "ilike", "not ilike").contains(syntax.op())) return null;
        ScalarPlan.Expression left = resolver.resolve(syntax.left(), SqlType.TEXT), right = resolver.resolve(syntax.right(), SqlType.TEXT);
        if (left.type().kind() != SqlType.Kind.TEXT || right.type().kind() != SqlType.Kind.TEXT) throw new SqlException("42804", "LIKE requires text operands");
        return new ScalarPlan.Expression(SqlType.BOOLEAN, meter -> {
            meter.operation(); SqlValue a = left.run(meter), b = right.run(meter); if (a.isNull() || b.isNull()) return SqlValue.nullValue(SqlType.BOOLEAN);
            boolean insensitive = syntax.op().endsWith("ilike");
            boolean match = matches(insensitive ? meter.environment().caseText(a.asText(), false) : a.asText(), insensitive ? meter.environment().caseText(b.asText(), false) : b.asText(), false);
            return SqlValue.ofBoolean(match != syntax.op().startsWith("not "));
        }, left.constant() && right.constant());
    }
    static ScalarPlan.Expression resolve(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        if (!call.name().equals("like_escape")) return null;
        if (call.args().size() != 2) throw new SqlException("42883", "like_escape requires two arguments");
        var pattern = resolver.resolve(call.args().get(0), SqlType.TEXT); var escape = resolver.resolve(call.args().get(1), SqlType.TEXT);
        if (pattern.type().kind() != SqlType.Kind.TEXT || escape.type().kind() != SqlType.Kind.TEXT) throw new SqlException("42883", "like_escape requires text arguments");
        return new ScalarPlan.Expression(SqlType.TEXT, meter -> {
            meter.operation(); SqlValue p = pattern.run(meter), e = escape.run(meter); if (p.isNull() || e.isNull()) return SqlValue.nullValue(SqlType.TEXT);
            int[] esc = e.asText().codePoints().toArray(); if (esc.length > 1) throw new SqlException("22025", "invalid escape string");
            int[] points = p.asText().codePoints().toArray(); StringBuilder out = new StringBuilder();
            for (int i = 0; i < points.length; i++) { int c = points[i]; if (esc.length == 1 && c == esc[0]) { out.append('\\'); if (++i == points.length) break; out.appendCodePoint(points[i]); } else { if (c == '\\') out.append('\\'); out.appendCodePoint(c); } }
            String result = out.toString(); if (!pattern.constant() || !escape.constant()) meter.scalarInput(SqlText.encodedLength(result));
            return SqlValue.ofText(result);
        }, pattern.constant() && escape.constant());
    }
    static boolean matches(String text, String pattern, boolean insensitive) {
        int[] source = text.codePoints().toArray(), raw = pattern.codePoints().toArray(); var tokens = new ArrayList<Integer>();
        for (int i = 0; i < raw.length; i++) {
            int c = raw[i]; if (c == '\\') { if (++i == raw.length) { tokens.add(-3); break; } tokens.add(raw[i]); }
            else tokens.add(c == '%' ? -1 : c == '_' ? -2 : c);
        }
        int at = 0, p = 0, star = -1, retry = 0;
        while (at < source.length) {
            if (p < tokens.size() && tokens.get(p) == -3) throw new SqlException("22025", "LIKE pattern must not end with escape character");
            if (p < tokens.size() && (tokens.get(p) == -2 || equal(tokens.get(p), source[at], insensitive))) { at++; p++; }
            else if (p < tokens.size() && tokens.get(p) == -1) { star = p++; retry = at; }
            else if (star >= 0) { p = star + 1; at = ++retry; }
            else return false;
        }
        while (p < tokens.size() && tokens.get(p) == -1) p++;
        return p == tokens.size();
    }
    private static boolean equal(int a, int b, boolean insensitive) { return a == b || insensitive && fold(a) == fold(b); }
    private static int fold(int c) { return c >= 'A' && c <= 'Z' ? c + 32 : c; }
}
