package org.jed;

/** Bounded range literal tokenizer; endpoint grammars remain the scalar input functions. */
final class RangeInput {
    private RangeInput() {}
    static SqlRange parse(String source, SqlType element, CostMeter meter) {
        String text = source.strip();
        if (text.equalsIgnoreCase("empty")) return SqlRange.empty();
        if (text.length() < 3 || "[(".indexOf(text.charAt(0)) < 0 || ")]".indexOf(text.charAt(text.length() - 1)) < 0) throw invalid();
        String[] tokens = new String[2]; boolean[] present = new boolean[2]; int p = 1;
        for (int field = 0; field < 2; field++) {
            StringBuilder token = new StringBuilder(); boolean quoted = false, wasQuoted = false;
            while (p < text.length() - 1) {
                char c = text.charAt(p++);
                if (c == '\\') { if (p >= text.length() - 1) throw invalid(); token.append(text.charAt(p++)); }
                else if (c == '"') {
                    wasQuoted = true;
                    if (quoted && p < text.length() - 1 && text.charAt(p) == '"') { token.append('"'); p++; }
                    else quoted = !quoted;
                } else if (!quoted && c == ',') { if (field == 1) throw invalid(); p--; break; }
                else if (!quoted && "[]()".indexOf(c) >= 0) throw invalid();
                else token.append(c);
            }
            if (quoted) throw invalid();
            tokens[field] = token.toString(); present[field] = wasQuoted || !token.isEmpty();
            if (field == 0) { if (p >= text.length() - 1 || text.charAt(p++) != ',') throw invalid(); }
        }
        SqlValue lower = present[0] ? SqlCasts.literal(tokens[0], element, meter) : null;
        SqlValue upper = present[1] ? SqlCasts.literal(tokens[1], element, meter) : null;
        return SqlRange.create(element, lower, upper, text.charAt(0) == '[', text.charAt(text.length() - 1) == ']');
    }
    private static SqlException invalid() { return new SqlException("22P02", "malformed range literal"); }
}
