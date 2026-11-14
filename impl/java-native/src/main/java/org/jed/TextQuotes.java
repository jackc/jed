package org.jed;

/** SQL text quoting shared by scalar quoting functions and formatting. */
final class TextQuotes {
    private TextQuotes() {}
    static String literal(String text) {
        boolean escape = text.indexOf('\\') >= 0;
        StringBuilder output = new StringBuilder(text.length() + 3);
        if (escape) output.append('E');
        output.append('\'');
        for (int i = 0; i < text.length(); i++) {
            char c = text.charAt(i);
            if (c == '\'' || c == '\\') output.append(c);
            output.append(c);
        }
        return output.append('\'').toString();
    }
    static String identifier(String text) {
        boolean safe = !text.isEmpty();
        for (int i = 0; safe && i < text.length(); i++) {
            char c = text.charAt(i);
            safe = c >= 'a' && c <= 'z' || c == '_' || i > 0 && c >= '0' && c <= '9';
        }
        if (safe && !QuotedKeywords.WORDS.contains(text)) return text;
        return "\"" + text.replace("\"", "\"\"") + "\"";
    }
}
