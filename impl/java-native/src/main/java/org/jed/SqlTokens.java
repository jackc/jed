package org.jed;

import java.util.List;
import java.util.StringJoiner;

/** The file-format token table; retained definitions are never pretty-printed from an AST. */
final class SqlTokens {
    private SqlTokens() {}

    static String render(List<Lexer.Token> tokens) {
        StringJoiner text = new StringJoiner(" ");
        for (Lexer.Token token : tokens) text.add(switch (token.kind()) {
            case WORD -> token.text();
            case NAME -> "\"" + token.text().replace("\"", "\"\"") + "\"";
            case STRING -> "'" + token.text().replace("'", "''") + "'";
            case INTEGER -> Decimal.parse(token.text()).toString();
            case DECIMAL -> decimal(token.text());
            case PARAMETER -> "$" + token.text();
            case SYMBOL -> token.text().equals("!=") ? "<>" : token.text();
            case END -> throw new IllegalArgumentException("end token in stored definition");
        });
        return text.toString();
    }

    private static String decimal(String raw) {
        // Validate exponent/size before expanding it. Preserve the lexical coefficient,
        // including its leading zeros and the distinction between .5 and 0.5.
        Decimal.parse(raw);
        int e = Math.max(raw.indexOf('e'), raw.indexOf('E'));
        String mantissa = e < 0 ? raw : raw.substring(0, e);
        int exponent;
        try { exponent = e < 0 ? 0 : Integer.parseInt(raw.substring(e + 1)); }
        catch (NumberFormatException error) { throw SqlException.numericOverflow(); }
        int dot = mantissa.indexOf('.');
        String digits = mantissa.replace(".", "");
        long scale = (dot < 0 ? 0 : mantissa.length() - dot - 1L) - exponent;
        if (scale < 0) { digits += "0".repeat((int)-scale); scale = 0; }
        if (scale > digits.length()) digits = "0".repeat((int)scale - digits.length()) + digits;
        int split = digits.length() - (int)scale;
        return digits.substring(0, split) + "." + digits.substring(split);
    }
}
