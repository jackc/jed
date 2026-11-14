package org.jed;

import java.util.ArrayList;

/** Strict RFC 8259 scanner; decoding is deliberately optional for textual json validation. */
final class JsonInput {
    private final String text; private final boolean decode, exactNumbers; private int p;
    private JsonInput(String text, boolean decode) { this(text, decode, decode); }
    private JsonInput(String text, boolean decode, boolean exactNumbers) { this.text = SqlText.validate(text); this.decode = decode; this.exactNumbers = exactNumbers; }
    static JsonNode decodedValidation(String text) { JsonInput in = new JsonInput(text, true, false); JsonNode node = in.node(0); in.space(); if (in.p != text.length()) throw invalid(); return node; }
    static JsonNode parse(String text, boolean decode) { JsonInput in = new JsonInput(text, decode); JsonNode node = in.node(0); in.space(); if (in.p != text.length()) throw invalid(); return node; }
    private void space() { while (p < text.length() && " \r\n\t".indexOf(text.charAt(p)) >= 0) p++; }
    private boolean take(char c) { if (p < text.length() && text.charAt(p) == c) { p++; return true; } return false; }
    private JsonNode node(int depth) {
        space(); int start = p; if (p >= text.length()) throw invalid(); char c = text.charAt(p); JsonNode value;
        if (c == '{' || c == '[') {
            if (depth >= 256) throw new SqlException("54001", "json nesting depth exceeds the maximum of 256"); p++; space();
            if (c == '[') {
                var elements = new ArrayList<JsonNode>();
                if (!take(']')) { do { elements.add(node(depth + 1)); space(); } while (take(',')); if (!take(']')) throw invalid(); }
                value = JsonNode.array(elements);
            } else {
                var members = new ArrayList<JsonNode.Member>();
                if (!take('}')) { do { space(); if (!take('"')) throw invalid(); String key = string(); space(); if (!take(':')) throw invalid(); members.add(new JsonNode.Member(key, node(depth + 1))); space(); } while (take(',')); if (!take('}')) throw invalid(); }
                value = JsonNode.object(members);
            }
        } else if (take('"')) value = JsonNode.lexicalString(string());
        else if (word("null")) value = JsonNode.NULL;
        else if (word("true")) value = JsonNode.bool(true);
        else if (word("false")) value = JsonNode.bool(false);
        else {
            take('-'); if (take('0')) { if (digit()) throw invalid(); } else { if (!digit() || text.charAt(p) == '0') throw invalid(); while (digit()) p++; }
            if (take('.')) { if (!digit()) throw invalid(); while (digit()) p++; }
            if (take('e') || take('E')) { if (!take('+')) take('-'); if (!digit()) throw invalid(); while (digit()) p++; }
            value = JsonNode.number(exactNumbers ? Decimal.parse(text.substring(start, p)) : Decimal.of(0));
        }
        return value.span(start, p);
    }
    private boolean digit() { return p < text.length() && text.charAt(p) >= '0' && text.charAt(p) <= '9'; }
    private boolean word(String word) { if (!text.startsWith(word, p)) return false; p += word.length(); return true; }
    private String string() {
        StringBuilder out = new StringBuilder();
        while (p < text.length()) {
            char c = text.charAt(p++); if (c == '"') return out.toString(); if (c < 32) throw invalid();
            if (c != '\\') { out.append(c); continue; }
            if (p >= text.length()) throw invalid(); c = text.charAt(p++);
            switch (c) {
                case '"', '\\', '/' -> out.append(c);
                case 'b' -> out.append('\b'); case 'f' -> out.append('\f'); case 'n' -> out.append('\n'); case 'r' -> out.append('\r'); case 't' -> out.append('\t');
                case 'u' -> {
                    char point = hex();
                    if (decode && point == 0) throw new SqlException("22P05", "unsupported Unicode escape sequence");
                    if (decode && Character.isHighSurrogate(point)) {
                        if (!take('\\') || !take('u')) throw invalid(); char low = hex(); if (!Character.isLowSurrogate(low)) throw invalid(); out.append(point).append(low);
                    } else { if (decode && Character.isLowSurrogate(point)) throw invalid(); out.append(point); }
                }
                default -> throw invalid();
            }
        }
        throw invalid();
    }
    private char hex() { int value = 0; for (int i = 0; i < 4; i++) { if (p >= text.length()) throw invalid(); char c = text.charAt(p++); int digit = c >= '0' && c <= '9' ? c - '0' : c >= 'a' && c <= 'f' ? c - 'a' + 10 : c >= 'A' && c <= 'F' ? c - 'A' + 10 : -1; if (digit < 0) throw invalid(); value = value * 16 + digit; } return (char) value; }
    private static SqlException invalid() { return new SqlException("22P02", "invalid input syntax for type json"); }
}
