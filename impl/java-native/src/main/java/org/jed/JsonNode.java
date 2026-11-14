package org.jed;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;

/** Immutable exact JSON tree. Objects may retain source order until canonicalized. */
public final class JsonNode {
    public enum Kind { NULL, STRING, NUMBER, BOOLEAN, ARRAY, OBJECT }
    public record Member(String key, JsonNode value) {}
    public static final JsonNode NULL = new JsonNode(Kind.NULL, null);
    private final Kind kind;
    private final Object value;
    final int start, end;
    private JsonNode(Kind kind, Object value) { this(kind, value, -1, -1); }
    private JsonNode(Kind kind, Object value, int start, int end) { this.kind = kind; this.value = value; this.start = start; this.end = end; }
    JsonNode span(int start, int end) { return new JsonNode(kind, value, start, end); }
    public static JsonNode bool(boolean value) { return new JsonNode(Kind.BOOLEAN, value); }
    public static JsonNode number(Decimal value) { return new JsonNode(Kind.NUMBER, value.checked()); }
    public static JsonNode string(String value) { return new JsonNode(Kind.STRING, SqlText.validate(value)); }
    static JsonNode lexicalString(String value) { return new JsonNode(Kind.STRING, value); }
    public static JsonNode array(List<JsonNode> elements) { return new JsonNode(Kind.ARRAY, List.copyOf(elements)); }
    public static JsonNode object(List<Member> members) { return new JsonNode(Kind.OBJECT, List.copyOf(members)); }
    public Kind kind() { return kind; }
    public boolean booleanValue() { return (boolean) value; }
    public Decimal number() { return (Decimal) value; }
    public String string() { return (String) value; }
    @SuppressWarnings("unchecked") public List<JsonNode> elements() { return (List<JsonNode>) value; }
    @SuppressWarnings("unchecked") public List<Member> members() { return (List<Member>) value; }
    public static JsonNode parse(String text, boolean decode) { return JsonInput.parse(text, decode); }
    public JsonNode canonical() {
        if (kind == Kind.ARRAY) { var out = new ArrayList<JsonNode>(); for (JsonNode item : elements()) out.add(item.canonical()); return array(out); }
        if (kind != Kind.OBJECT) return this;
        var unique = new LinkedHashMap<String, JsonNode>(); for (Member member : members()) unique.put(member.key, member.value.canonical());
        var out = new ArrayList<Member>(); unique.forEach((key, item) -> out.add(new Member(key, item)));
        out.sort((a, b) -> { int c = Long.compare(SqlText.encodedLength(a.key), SqlText.encodedLength(b.key)); return c != 0 ? c : SqlText.compare(a.key, b.key); });
        return object(out);
    }
    public JsonNode get(String key) { if (kind != Kind.OBJECT) return null; for (int i = members().size() - 1; i >= 0; i--) if (members().get(i).key.equals(key)) return members().get(i).value; return null; }
    public JsonNode get(long index) { if (kind != Kind.ARRAY) return null; if (index < 0) index += elements().size(); return index < 0 || index >= elements().size() ? null : elements().get((int) index); }
    public int compare(JsonNode other) { return compare(other, true); }
    private int compare(JsonNode other, boolean root) {
        if (root && kind == Kind.ARRAY && elements().isEmpty() && other.kind != Kind.ARRAY && other.kind != Kind.OBJECT) return -1;
        if (root && other.kind == Kind.ARRAY && other.elements().isEmpty() && kind != Kind.ARRAY && kind != Kind.OBJECT) return 1;
        int order = Integer.compare(kind.ordinal(), other.kind.ordinal()); if (order != 0) return order;
        return switch (kind) {
            case NULL -> 0;
            case STRING -> SqlText.compare(string(), other.string());
            case NUMBER -> number().compareTo(other.number());
            case BOOLEAN -> Boolean.compare(booleanValue(), other.booleanValue());
            case ARRAY -> {
                int c = Integer.compare(elements().size(), other.elements().size());
                for (int i = 0; c == 0 && i < elements().size(); i++) c = elements().get(i).compare(other.elements().get(i), false);
                yield c;
            }
            case OBJECT -> {
                int c = Integer.compare(members().size(), other.members().size());
                for (int i = 0; c == 0 && i < members().size(); i++) { Member a = members().get(i), b = other.members().get(i); c = SqlText.compare(a.key, b.key); if (c == 0) c = a.value.compare(b.value, false); }
                yield c;
            }
        };
    }
    long renderedBytes() {
        return switch (kind) {
            case NULL -> 4; case BOOLEAN -> booleanValue() ? 4 : 5; case NUMBER -> number().toString().length(); case STRING -> quotedBytes(string());
            case ARRAY -> { long size = 2; for (int i = 0; i < elements().size(); i++) size = MemoryAccount.add(size, (i == 0 ? 0 : 2) + elements().get(i).renderedBytes()); yield size; }
            case OBJECT -> { long size = 2; for (int i = 0; i < members().size(); i++) { Member member = members().get(i); size = MemoryAccount.add(size, (i == 0 ? 0 : 2) + quotedBytes(member.key) + 2 + member.value.renderedBytes()); } yield size; }
        };
    }
    static long quotedBytes(String value) {
        long bytes = 2;
        for (int i = 0; i < value.length();) { int c = value.codePointAt(i); i += Character.charCount(c); bytes += c == '"' || c == '\\' || c == '\b' || c == '\f' || c == '\n' || c == '\r' || c == '\t' ? 2 : c < 32 ? 6 : c < 128 ? 1 : c < 2048 ? 2 : c < 65536 ? 3 : 4; }
        return bytes;
    }
    public String render() { StringBuilder out = new StringBuilder(); render(out); return out.toString(); }
    void render(StringBuilder out) {
        switch (kind) {
            case NULL -> out.append("null");
            case BOOLEAN -> out.append(booleanValue());
            case NUMBER -> out.append(number());
            case STRING -> quote(out, string());
            case ARRAY -> { out.append('['); for (int i = 0; i < elements().size(); i++) { if (i != 0) out.append(", "); elements().get(i).render(out); } out.append(']'); }
            case OBJECT -> { out.append('{'); for (int i = 0; i < members().size(); i++) { if (i != 0) out.append(", "); Member member = members().get(i); quote(out, member.key); out.append(": "); member.value.render(out); } out.append('}'); }
        }
    }
    static void quote(StringBuilder out, String text) {
        out.append('"');
        for (int i = 0; i < text.length(); i++) {
            char c = text.charAt(i);
            switch (c) {
                case '"' -> out.append("\\\""); case '\\' -> out.append("\\\\"); case '\b' -> out.append("\\b");
                case '\f' -> out.append("\\f"); case '\n' -> out.append("\\n"); case '\r' -> out.append("\\r"); case '\t' -> out.append("\\t");
                default -> { if (c < 32) out.append("\\u00").append(Character.forDigit(c >>> 4, 16)).append(Character.forDigit(c & 15, 16)); else out.append(c); }
            }
        }
        out.append('"');
    }
    public void checkDepth() { checkDepth(0); }
    private void checkDepth(int parent) {
        if (kind != Kind.ARRAY && kind != Kind.OBJECT) return;
        if (parent >= 256) throw new SqlException("54001", "json nesting depth exceeds the maximum of 256");
        if (kind == Kind.ARRAY) for (JsonNode child : elements()) child.checkDepth(parent + 1);
        else for (Member member : members()) member.value.checkDepth(parent + 1);
    }
    @Override public String toString() { return render(); }
    @Override public boolean equals(Object other) { return other instanceof JsonNode node && compare(node) == 0; }
    @Override public int hashCode() { return 31 * kind.hashCode() + java.util.Objects.hashCode(value); }
}
