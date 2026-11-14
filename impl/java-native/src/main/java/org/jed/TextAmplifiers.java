package org.jed;

/** Exact pre-allocation output sizing and accounting for text amplifiers. */
final class TextAmplifiers {
    private TextAmplifiers() {}
    private static final long MAX_RESULT = 0x3fffffffL;

    static String repeat(String text, long count, CostMeter meter) {
        if (count <= 0 || text.isEmpty()) return "";
        long input = SqlText.encodedLength(text);
        if (count > MAX_RESULT || SqlText.length(text) > MAX_RESULT / count) throw tooLarge();
        meter.scalarInput(input);
        if (input > MAX_RESULT / count) throw tooLarge();
        long size = input * count;
        meter.scalarOutput(size);
        return text.repeat((int) count);
    }

    static String pad(String text, long requested, String fill, boolean left, CostMeter meter) {
        if (requested <= 0) return "";
        if (requested > MAX_RESULT) throw tooLarge();
        meter.scalarInput(SqlText.encodedLength(text) + SqlText.encodedLength(fill));
        int have = SqlText.length(text);
        int kept = (int) Math.min(requested, have);
        int keptUnits = text.offsetByCodePoints(0, kept);
        long keptBytes = prefixBytes(text, keptUnits);
        long missing = Math.max(0, requested - have);
        int fillCharacters = SqlText.length(fill);
        if (fillCharacters == 0 || missing == 0) {
            meter.scalarOutput(keptBytes);
            return text.substring(0, keptUnits);
        }
        long cycles = missing / fillCharacters;
        int tailCharacters = (int) (missing % fillCharacters);
        int tailUnits = fill.offsetByCodePoints(0, tailCharacters);
        long size = keptBytes + cycles * SqlText.encodedLength(fill) + prefixBytes(fill, tailUnits);
        if (size > MAX_RESULT) throw tooLarge();
        meter.scalarOutput(size);
        StringBuilder output = new StringBuilder((int) size);
        if (!left) output.append(text, 0, keptUnits);
        for (long i = 0; i < cycles; i++) output.append(fill);
        output.append(fill, 0, tailUnits);
        if (left) output.append(text, 0, keptUnits);
        return output.toString();
    }

    static String replace(String text, String from, String to, CostMeter meter) {
        long input = SqlText.encodedLength(text);
        meter.scalarInput(input);
        long count = 0;
        if (!from.isEmpty()) {
            for (int i = 0; (i = text.indexOf(from, i)) >= 0; i += from.length()) count++;
        }
        long difference = SqlText.encodedLength(to) - SqlText.encodedLength(from);
        if (count > 0 && difference > 0 && difference > (MAX_RESULT - input) / count) throw tooLarge();
        long size = input + count * difference;
        if (size > MAX_RESULT) throw tooLarge();
        meter.scalarOutput(size);
        if (count == 0) return text;
        StringBuilder output = new StringBuilder((int) size);
        int start = 0, match;
        while ((match = text.indexOf(from, start)) >= 0) {
            output.append(text, start, match).append(to);
            start = match + from.length();
        }
        output.append(text, start, text.length());
        return output.toString();
    }

    private static long prefixBytes(String text, int end) {
        long count = 0;
        for (int i = 0; i < end;) {
            int c = text.codePointAt(i); i += Character.charCount(c);
            count += c < 128 ? 1 : c < 2048 ? 2 : c < 65536 ? 3 : 4;
        }
        return count;
    }
    private static SqlException tooLarge() { return new SqlException("54000", "requested length too large"); }
}
