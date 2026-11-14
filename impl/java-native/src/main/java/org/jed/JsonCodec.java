package org.jed;

import java.util.ArrayList;

/** Format53 tagged JSON nodes, rejecting reserved dictionary tags and noncanonical objects. */
final class JsonCodec {
    private JsonCodec() {}
    static void key(Encoding.Sink out, JsonNode node, boolean root) {
        if (root && node.kind() == JsonNode.Kind.ARRAY && node.elements().isEmpty()) { out.u8(0); return; }
        int rank = switch (node.kind()) { case NULL -> 1; case STRING -> 2; case NUMBER -> 3; case BOOLEAN -> node.booleanValue() ? 5 : 4; case ARRAY -> 6; case OBJECT -> 7; };
        out.u8(rank);
        switch (node.kind()) {
            case STRING -> out.raw(Encoding.key(SqlValue.ofText(node.string())));
            case NUMBER -> out.raw(Encoding.key(SqlValue.ofDecimal(node.number())));
            case ARRAY -> { out.unsigned(node.elements().size(), 4); for (JsonNode child : node.elements()) key(out, child, false); }
            case OBJECT -> { out.unsigned(node.members().size(), 4); for (JsonNode.Member member : node.members()) { out.raw(Encoding.key(SqlValue.ofText(member.key()))); key(out, member.value(), false); } }
            default -> { }
        }
    }
    static void write(Encoding.Sink out, JsonNode node) {
        switch (node.kind()) {
            case NULL -> out.u8(0);
            case BOOLEAN -> out.u8(node.booleanValue() ? 2 : 1);
            case NUMBER -> { out.u8(3); Encoding.body(out, SqlValue.ofDecimal(node.number())); }
            case STRING -> { out.u8(4); byte[] text = SqlText.encode(node.string()); varint(out, text.length); out.raw(text); }
            case ARRAY -> { out.u8(6); varint(out, node.elements().size()); for (JsonNode element : node.elements()) write(out, element); }
            case OBJECT -> { out.u8(7); varint(out, node.members().size()); for (JsonNode.Member member : node.members()) { write(out, JsonNode.string(member.key())); write(out, member.value()); } }
        }
    }
    static JsonNode read(Encoding.Reader in, int depth) {
        int tag = in.u8();
        if ((tag == 6 || tag == 7) && depth >= 256) throw Encoding.corrupt("jsonb nesting depth exceeds the maximum of 256");
        return switch (tag) {
            case 0 -> JsonNode.NULL;
            case 1, 2 -> JsonNode.bool(tag == 2);
            case 3 -> JsonNode.number(Encoding.readBody(in, SqlType.DECIMAL).asDecimal());
            case 4 -> JsonNode.string(SqlText.decode(in.raw(varint(in))));
            case 6 -> { int count = varint(in); if (count > in.remaining()) throw Encoding.corrupt("truncated jsonb array"); var items = new ArrayList<JsonNode>(); for (int i = 0; i < count; i++) items.add(read(in, depth + 1)); yield JsonNode.array(items); }
            case 7 -> {
                int count = varint(in); if (count > in.remaining() / 3) throw Encoding.corrupt("truncated jsonb object"); var members = new ArrayList<JsonNode.Member>(); String previous = null;
                for (int i = 0; i < count; i++) {
                    JsonNode key = read(in, depth + 1); if (key.kind() != JsonNode.Kind.STRING) throw Encoding.corrupt("invalid jsonb key");
                    String name = key.string();
                    if (previous != null) { int c = Long.compare(SqlText.encodedLength(previous), SqlText.encodedLength(name)); if (c > 0 || c == 0 && SqlText.compare(previous, name) >= 0) throw Encoding.corrupt("noncanonical jsonb key order"); }
                    members.add(new JsonNode.Member(name, read(in, depth + 1))); previous = name;
                }
                yield JsonNode.object(members);
            }
            default -> throw Encoding.corrupt("reserved jsonb tag");
        };
    }
    private static void varint(Encoding.Sink out, int value) { while (value >= 128) { out.u8((value & 127) | 128); value >>>= 7; } out.u8(value); }
    private static int varint(Encoding.Reader in) {
        int value = 0;
        for (int shift = 0; shift <= 28; shift += 7) { int b = in.u8(); if (shift == 28 && b > 7) throw Encoding.corrupt("oversized jsonb varint"); value |= (b & 127) << shift; if ((b & 128) == 0) { if (shift > 0 && b == 0) throw Encoding.corrupt("noncanonical jsonb varint"); return value; } }
        throw Encoding.corrupt("oversized jsonb varint");
    }
    // Textual JSON keeps its spelling and accepts values outside the jsonb input
    // domain. Measure its retained node shape without performing a JSONB cast.
    static long textMemory(String text) { return textMemory(JsonNode.parse(text, false), text); }
    private static long textMemory(JsonNode node, String text) {
        long bytes = MemorySchedule.VALUE;
        return switch (node.kind()) {
            case NULL, BOOLEAN -> bytes;
            case STRING -> MemoryAccount.add(bytes, SqlText.encodedLength(node.string()));
            case NUMBER -> MemoryAccount.add(bytes, numberMemory(text, node.start, node.end));
            case ARRAY -> { for (JsonNode child : node.elements()) bytes = MemoryAccount.add(bytes, textMemory(child, text)); yield bytes; }
            case OBJECT -> { for (JsonNode.Member member : node.members()) { bytes = MemoryAccount.add(bytes, SqlText.encodedLength(member.key())); bytes = MemoryAccount.add(bytes, textMemory(member.value(), text)); } yield bytes; }
        };
    }
    private static long numberMemory(String text, int start, int end) {
        long digits = 0, fractional = 0, exponent = 0; boolean point = false, nonzero = false;
        int at = start; if (text.charAt(at) == '-') at++;
        while (at < end && text.charAt(at) != 'e' && text.charAt(at) != 'E') {
            char c = text.charAt(at++);
            if (c == '.') { point = true; continue; }
            if (point) fractional++;
            nonzero |= c != '0'; if (nonzero) digits++;
        }
        if (!nonzero) return 0;
        boolean negative = false;
        if (at < end) {
            at++; if (at < end && (text.charAt(at) == '+' || text.charAt(at) == '-')) negative = text.charAt(at++) == '-';
            while (at < end) { int digit = text.charAt(at++) - '0'; exponent = exponent > (Long.MAX_VALUE - digit) / 10 ? Long.MAX_VALUE : exponent * 10 + digit; }
        }
        if (!negative && exponent > fractional) digits = MemoryAccount.add(digits, exponent - fractional);
        long groups = digits / 4 + (digits % 4 == 0 ? 0 : 1);
        if (groups > Long.MAX_VALUE / MemorySchedule.DECIMAL_GROUP) throw MemoryAccount.exceeded();
        return groups * MemorySchedule.DECIMAL_GROUP;
    }
    static long memory(JsonNode node) {
        long bytes = MemorySchedule.VALUE;
        return switch (node.kind()) {
            case NULL, BOOLEAN -> bytes;
            case NUMBER -> MemoryAccount.add(bytes, MemoryAccount.payload(SqlValue.ofDecimal(node.number())));
            case STRING -> MemoryAccount.add(bytes, SqlText.encodedLength(node.string()));
            case ARRAY -> { for (JsonNode child : node.elements()) bytes = MemoryAccount.add(bytes, memory(child)); yield bytes; }
            case OBJECT -> { for (JsonNode.Member member : node.members()) { bytes = MemoryAccount.add(bytes, SqlText.encodedLength(member.key())); bytes = MemoryAccount.add(bytes, memory(member.value())); } yield bytes; }
        };
    }
}
