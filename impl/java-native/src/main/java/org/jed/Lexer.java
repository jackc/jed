package org.jed;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;

/** Handwritten scanner for the shared SQL grammar; keywords remain ordinary words. */
final class Lexer {
    enum Kind { END, WORD, NAME, INTEGER, DECIMAL, STRING, SYMBOL, PARAMETER }
    record Token(Kind kind, String text, int start) {
        boolean is(String spelling) {
            return (kind == Kind.WORD || kind == Kind.SYMBOL) && text.equals(spelling);
        }
    }

    private final String sql;
    private int at;

    Lexer(String sql) { this.sql = SqlText.validate(sql); }

    List<Token> scan() {
        List<Token> tokens = new ArrayList<>();
        Token token;
        do { token = next(); tokens.add(token); } while (token.kind != Kind.END);
        return List.copyOf(tokens);
    }

    private char peek(int ahead) { return at + ahead < sql.length() ? sql.charAt(at + ahead) : '\0'; }
    private static boolean digit(char c) { return c >= '0' && c <= '9'; }
    private static boolean letter(char c) { return c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c == '_'; }
    private static boolean space(char c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\u000b'; }
    private static SqlException syntax(String message) { return new SqlException("42601", message); }

    private void trivia() {
        while (at < sql.length()) {
            if (space(peek(0))) { at++; continue; }
            if (peek(0) == '-' && peek(1) == '-') {
                while (at < sql.length() && peek(0) != '\n' && peek(0) != '\r') at++;
                continue;
            }
            if (peek(0) == '/' && peek(1) == '*') {
                at += 2;
                int nesting = 1;
                while (nesting > 0) {
                    if (at == sql.length()) throw syntax("unterminated block comment");
                    if (peek(0) == '/' && peek(1) == '*') { nesting++; at += 2; }
                    else if (peek(0) == '*' && peek(1) == '/') { nesting--; at += 2; }
                    else at++;
                }
                continue;
            }
            return;
        }
    }

    private Token next() {
        trivia();
        int start = at;
        char c = peek(0);
        if (at == sql.length()) return new Token(Kind.END, "", at);
        if ((c == 'U' || c == 'u') && peek(1) == '&' && (peek(2) == '\'' || peek(2) == '"')) {
            at += 2;
            char quote = peek(0);
            String raw = quoted(quote, false);
            if (quote == '"' && raw.isEmpty()) throw syntax("zero-length delimited identifier");
            char escape = '\\';
            int after = at;
            trivia();
            int wordStart = at;
            while (letter(peek(0)) || digit(peek(0))) at++;
            if (sql.substring(wordStart, at).equalsIgnoreCase("uescape")) {
                trivia();
                if (peek(0) != '\'') throw syntax("UESCAPE must be followed by a simple string literal");
                String specified = quoted('\'', false);
                if (specified.length() != 1 || specified.charAt(0) > 127
                        || Character.digit(specified.charAt(0), 16) >= 0
                        || "+'\"".indexOf(specified.charAt(0)) >= 0 || space(specified.charAt(0))) {
                    throw syntax("invalid Unicode escape character");
                }
                escape = specified.charAt(0);
            } else at = after;
            String decoded = unicode(raw, escape);
            return new Token(quote == '"' ? Kind.NAME : Kind.STRING,
                quote == '"' ? name(decoded) : decoded, start);
        }
        if (digit(c) || c == '.' && digit(peek(1))) return number();
        if ((c == 'e' || c == 'E') && peek(1) == '\'') {
            at++;
            return new Token(Kind.STRING, quoted('\'', true), start);
        }
        if (c == '\'' || c == '"') {
            String value = quoted(c, false);
            return new Token(c == '\'' ? Kind.STRING : Kind.NAME, c == '"' ? name(value) : value, start);
        }
        if (c == '$') {
            at++;
            if (digit(peek(0))) {
                int digits = at;
                while (digit(peek(0))) at++;
                String index = sql.substring(digits, at);
                if (index.charAt(0) == '0' || index.length() > 5 || Integer.parseInt(index) > 65535) {
                    throw syntax("parameter index must be between 1 and 65535");
                }
                return new Token(Kind.PARAMETER, index, start);
            }
            if (peek(0) != '$' && !letter(peek(0))) throw syntax("invalid dollar quote");
            while (letter(peek(0)) || digit(peek(0))) at++;
            if (peek(0) != '$') throw syntax("invalid dollar quote");
            at++;
            String delimiter = sql.substring(start, at);
            int end = sql.indexOf(delimiter, at);
            if (end < 0) throw syntax("unterminated dollar quote");
            String body = sql.substring(at, end);
            at = end + delimiter.length();
            return new Token(Kind.STRING, body, start);
        }
        if (letter(c)) {
            at++;
            while (letter(peek(0)) || digit(peek(0)) || peek(0) == '$') at++;
            String value = name(sql.substring(start, at));
            StringBuilder folded = new StringBuilder(value.length());
            for (int i = 0; i < value.length(); i++) {
                char x = value.charAt(i);
                folded.append(x >= 'A' && x <= 'Z' ? (char)(x + ('a' - 'A')) : x);
            }
            return new Token(Kind.WORD, folded.toString(), start);
        }
        for (String symbol : new String[]{"-|-", "->>", "#>>", "!~*", "::", "<=", ">=", "<>", "!=", "||", "=>", "<<", ">>", "!~", "~*", "@>", "<@", "&&", "&<", "&>", "->", "#>", "#-", "?|", "?&", "@?", "@@"}) {
            if (sql.startsWith(symbol, at)) { at += symbol.length(); return new Token(Kind.SYMBOL, symbol, start); }
        }
        if ("+-*/%=<>()[],;.^&|#~:?".indexOf(c) >= 0) {
            at++;
            return new Token(Kind.SYMBOL, Character.toString(c), start);
        }
        throw syntax("unexpected character at " + (start + 1));
    }

    private Token number() {
        int start = at;
        boolean decimal = false;
        if (peek(0) == '0' && "xXoObB".indexOf(peek(1)) >= 0 && peek(1) != '\0') {
            char prefix = peek(1);
            int radix = prefix == 'x' || prefix == 'X' ? 16 : prefix == 'o' || prefix == 'O' ? 8 : 2;
            at += 2;
            if (peek(0) == '_') at++;
            digits(radix, true);
            if (letter(peek(0)) || digit(peek(0))) throw syntax("invalid radix integer");
        } else {
            if (peek(0) != '.') digits(10, true);
            if (peek(0) == '.') { decimal = true; at++; digits(10, false); }
            if (peek(0) == 'e' || peek(0) == 'E') {
                decimal = true; at++;
                if (peek(0) == '+' || peek(0) == '-') at++;
                digits(10, true);
            }
            if (letter(peek(0))) throw syntax("trailing junk after numeric literal");
        }
        return new Token(decimal ? Kind.DECIMAL : Kind.INTEGER, sql.substring(start, at).replace("_", ""), start);
    }

    private void digits(int radix, boolean required) {
        int start = at;
        while (asciiDigit(peek(0), radix)) {
            at++;
            if (peek(0) == '_') {
                at++;
                if (!asciiDigit(peek(0), radix)) throw syntax("invalid numeric separator");
            }
        }
        if (required && start == at) throw syntax("expected numeric digits");
    }

    private static boolean asciiDigit(char c, int radix) {
        return c < 128 && Character.digit(c, radix) >= 0;
    }

    private String quoted(char quote, boolean escapes) {
        at++;
        StringBuilder out = new StringBuilder();
        while (at < sql.length()) {
            char c = sql.charAt(at++);
            if (c == quote) {
                if (peek(0) == quote) { out.append(quote); at++; continue; }
                return SqlText.validate(out.toString());
            }
            if (c == '\\' && escapes) {
                if (at == sql.length()) throw syntax("unterminated escape");
                char escaped = sql.charAt(at++);
                switch (escaped) {
                    case 'b' -> out.append('\b');
                    case 'f' -> out.append('\f');
                    case 'n' -> out.append('\n');
                    case 'r' -> out.append('\r');
                    case 't' -> out.append('\t');
                    default -> out.append(escaped);
                }
            } else out.append(c);
        }
        throw syntax("unterminated quoted token");
    }

    private static String name(String name) {
        if (name.isEmpty()) throw syntax("zero-length delimited identifier");
        if (name.getBytes(StandardCharsets.UTF_8).length > 63) throw new SqlException("42622", "identifier exceeds 63 bytes");
        return name;
    }

    private static String unicode(String raw, char escape) {
        StringBuilder out = new StringBuilder();
        int pendingHigh = -1;
        for (int i = 0; i < raw.length();) {
            if (raw.charAt(i) != escape) {
                if (pendingHigh >= 0) throw syntax("invalid Unicode surrogate pair");
                out.append(raw.charAt(i++));
                continue;
            }
            i++;
            if (i < raw.length() && raw.charAt(i) == escape) {
                if (pendingHigh >= 0) throw syntax("invalid Unicode surrogate pair");
                out.append(escape); i++; continue;
            }
            int count = 4;
            if (i < raw.length() && raw.charAt(i) == '+') { count = 6; i++; }
            if (i + count > raw.length()) throw syntax("invalid Unicode escape");
            int cp = 0;
            for (int j = 0; j < count; j++) {
                char c = raw.charAt(i++);
                if (!asciiDigit(c, 16)) throw syntax("invalid Unicode escape");
                cp = cp * 16 + Character.digit(c, 16);
            }
            if (cp == 0 || cp > 0x10ffff) throw syntax("invalid Unicode escape value");
            if (pendingHigh >= 0) {
                if (cp < 0xdc00 || cp > 0xdfff) throw syntax("invalid Unicode surrogate pair");
                out.appendCodePoint(Character.toCodePoint((char)pendingHigh, (char)cp));
                pendingHigh = -1;
            } else if (cp >= 0xd800 && cp <= 0xdbff) pendingHigh = cp;
            else if (cp >= 0xdc00 && cp <= 0xdfff) throw syntax("invalid Unicode surrogate pair");
            else out.appendCodePoint(cp);
        }
        if (pendingHigh >= 0) throw syntax("invalid Unicode surrogate pair");
        return out.toString();
    }
}
