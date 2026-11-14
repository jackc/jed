package org.jed;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Bounded recursive array text grammar; element conversion is the ordinary typed cast seam. */
final class ArrayInput {
    private final String text;
    private final SqlType element;
    private final CostMeter meter;
    private final List<SqlValue> values = new ArrayList<>();
    private int at;
    private ArrayInput(String text, SqlType element, CostMeter meter) { this.text = text; this.element = element; this.meter = meter; }
    static SqlArray parse(String text, SqlType element, CostMeter meter) { return new ArrayInput(text, element, meter).parse(); }
    private SqlArray parse() {
        space(); List<Integer> lengths = new ArrayList<>(), lower = new ArrayList<>();
        while (take('[')) {
            if (lengths.size() >= 6) throw invalid();
            int start = number(); expect(':'); int end = number(); expect(']');
            long length = (long) end - start + 1;
            if (length <= 0) throw SqlArray.shape();
            if (length > Integer.MAX_VALUE || end == Integer.MAX_VALUE) throw bounds();
            lower.add(start); lengths.add((int) length); space();
        }
        if (!lengths.isEmpty()) expect('=');
        int[] dims = level(0); space(); if (at != text.length()) throw invalid();
        int[] bounds = new int[dims.length]; Arrays.fill(bounds, 1);
        if (!lengths.isEmpty()) {
            if (lengths.size() != dims.length) throw invalid();
            for (int d = 0; d < dims.length; d++) { if (lengths.get(d) != dims[d]) throw invalid(); bounds[d] = lower.get(d); }
        }
        return new SqlArray(dims, bounds, values);
    }
    private int[] level(int depth) {
        if (depth >= 6) throw invalid();
        space(); expect('{'); space(); if (take('}')) return new int[0];
        boolean nested = peek('{'); int[] child = null; int count = 0;
        do {
            space();
            if (nested) {
                if (!peek('{')) throw invalid();
                int[] shape = level(depth + 1);
                if (child == null) child = shape; else if (!Arrays.equals(child, shape)) throw invalid();
            } else { if (peek('{')) throw invalid(); values.add(token()); }
            count++; space();
        } while (take(','));
        expect('}');
        if (nested && child.length == 0) return new int[0];
        int[] dims = new int[1 + (nested ? child.length : 0)]; dims[0] = count;
        if (nested) System.arraycopy(child, 0, dims, 1, child.length);
        return dims;
    }
    private SqlValue token() {
        boolean quoted = take('"'), escaped = false;
        StringBuilder token = new StringBuilder();
        if (quoted) {
            boolean closed = false;
            while (at < text.length()) { char c = text.charAt(at++); if (c == '"') { closed = true; break; } if (c == '\\') { if (at == text.length()) throw invalid(); c = text.charAt(at++); } token.append(c); }
            if (!closed) throw invalid();
            space(); if (!peek(',') && !peek('}')) throw invalid();
        } else {
            int keep = 0;
            while (at < text.length() && !peek(',') && !peek('}')) {
                char c = text.charAt(at++); boolean escape = c == '\\';
                if (escape) { escaped = true; if (at == text.length()) throw invalid(); c = text.charAt(at++); }
                else if (c == '"' || c == '{') throw invalid();
                token.append(c); if (escape || !whitespace(c)) keep = token.length();
            }
            token.setLength(keep); if (token.isEmpty()) throw invalid();
        }
        String value = token.toString();
        return !quoted && !escaped && value.equalsIgnoreCase("NULL") ? SqlValue.nullValue(element) : SqlCasts.literal(value, element, meter);
    }
    private int number() {
        space(); int start = at; if (peek('-') || peek('+')) at++;
        while (at < text.length() && text.charAt(at) >= '0' && text.charAt(at) <= '9') at++;
        try { int value = Integer.parseInt(text.substring(start, at)); space(); return value; } catch (NumberFormatException e) { if (at == start || at == start + 1 && (text.charAt(start) == '-' || text.charAt(start) == '+')) throw invalid(); throw bounds(); }
    }
    private boolean peek(char c) { return at < text.length() && text.charAt(at) == c; }
    private boolean take(char c) { if (!peek(c)) return false; at++; return true; }
    private void expect(char c) { space(); if (!take(c)) throw invalid(); }
    private void space() { while (at < text.length() && whitespace(text.charAt(at))) at++; }
    private static boolean whitespace(char c) { return " \t\n\r\f\u000b".indexOf(c) >= 0; }
    private static SqlException bounds() { return new SqlException("54000", "array bound out of range"); }
    private static SqlException invalid() { return new SqlException("22P02", "malformed array literal"); }
}
