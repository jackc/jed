package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.Set;

/** Handwritten SQL/JSON path compiler. The immutable program contains no host callbacks. */
public final class JsonPath {
    record Node(String kind, String text, List<Node> args) {
        Node(String kind, String text, Node... args) { this(kind, text, List.of(args)); }
        boolean predicate() { return Set.of("compare", "and", "or", "not", "exists", "unknown", "starts", "regex").contains(kind); }
    }
    private final boolean strict;
    private final Node root;
    private final String text;
    private JsonPath(boolean strict, Node root) { this.strict = strict; this.root = root; this.text = (strict ? "strict " : "") + render(root, -1); }
    public static JsonPath compile(String text) {
        Parser parser = new Parser(text); JsonPath path = parser.parse();
        // Parentheses inserted by canonical rendering must obey the same nesting limit.
        if (!path.text.equals(text)) new Parser(path.text).parse();
        return path;
    }
    boolean strict() { return strict; }
    Node root() { return root; }
    @Override public String toString() { return text; }
    @Override public boolean equals(Object other) { return other instanceof JsonPath path && text.equals(path.text); }
    @Override public int hashCode() { return text.hashCode(); }

    private static int priority(Node node) {
        return switch (node.kind) { case "or" -> 0; case "and" -> 1; case "compare", "starts", "regex" -> 2; case "binary" -> node.text.equals("+") || node.text.equals("-") ? 3 : 4; case "unary" -> 5; default -> 9; };
    }
    private static String render(Node n, int parent) {
        if (Set.of("member", "members", "elements", "any", "method", "index", "filter").contains(n.kind)) return renderAccess(n);
        String body;
        int rank = priority(n);
        switch (n.kind) {
            case "root" -> body = "$";
            case "current" -> body = "@";
            case "last" -> body = "last";
            case "variable" -> body = "$" + quote(n.text);
            case "string" -> body = quote(n.text);
            case "number", "literal" -> body = n.text;
            case "member" -> body = accessBase(n.args.get(0)) + "." + quote(n.text);
            case "members" -> body = accessBase(n.args.get(0)) + ".*";
            case "elements" -> body = accessBase(n.args.get(0)) + "[*]";
            case "any" -> body = accessBase(n.args.get(0)) + ".**" + n.text;
            case "method" -> {
                StringBuilder b = new StringBuilder(accessBase(n.args.get(0))).append('.').append(n.text).append('(');
                for (int i = 1; i < n.args.size(); i++) { if (i > 1) b.append(','); b.append(render(n.args.get(i), 8)); }
                body = b.append(')').toString();
            }
            case "index" -> {
                StringBuilder b = new StringBuilder(accessBase(n.args.get(0))).append('[');
                for (int i = 1; i < n.args.size(); i++) { if (i > 1) b.append(','); b.append(render(n.args.get(i), -2)); }
                body = b.append(']').toString();
            }
            case "slice" -> body = render(n.args.get(0), -2) + " to " + render(n.args.get(1), -2);
            case "filter" -> body = accessBase(n.args.get(0)) + "?(" + render(n.args.get(1), -2) + ")";
            case "not" -> body = "!(" + render(n.args.get(0), -2) + ")";
            case "exists" -> body = "exists (" + render(n.args.get(0), -2) + ")";
            case "unknown" -> body = "(" + render(n.args.get(0), -2) + ") is unknown";
            case "unary" -> body = n.text + render(n.args.get(0), rank);
            case "regex" -> {
                body = render(n.args.get(0), 8) + " like_regex " + quote(n.text);
                if (!n.args.get(1).text.isEmpty()) body += " flag " + quote(n.args.get(1).text);
            }
            default -> body = render(n.args.get(0), rank) + " " + n.text + " " + render(n.args.get(1), rank);
        }
        if (rank < 9 && (parent == -1 || parent >= rank)) return "(" + body + ")";
        return body;
    }
    private static String renderAccess(Node node) {
        ArrayList<Node> path = new ArrayList<>();
        while (Set.of("member", "members", "elements", "any", "method", "index", "filter").contains(node.kind)) { path.add(node); node = node.args.getFirst(); }
        StringBuilder out = new StringBuilder(accessBase(node));
        for (int i = path.size() - 1; i >= 0; i--) {
            Node step = path.get(i);
            switch (step.kind) {
                case "member" -> out.append('.').append(quote(step.text));
                case "members" -> out.append(".*");
                case "elements" -> out.append("[*]");
                case "any" -> out.append(".**").append(step.text);
                case "filter" -> out.append("?(").append(render(step.args.get(1), -2)).append(')');
                case "method", "index" -> {
                    boolean method = step.kind.equals("method");
                    if (method) out.append('.').append(step.text).append('('); else out.append('[');
                    for (int j = 1; j < step.args.size(); j++) { if (j > 1) out.append(','); out.append(render(step.args.get(j), -2)); }
                    out.append(method ? ')' : ']');
                }
                default -> throw new AssertionError();
            }
        }
        return out.toString();
    }
    private static String accessBase(Node n) {
        if (n.kind.equals("number") || n.predicate() && priority(n) == 9) return "(" + render(n, -2) + ")";
        return render(n, 8);
    }
    private static String quote(String text) { return JsonNode.string(text).render(); }
    private record Token(String kind, String text, int start, int end) {}
    private static final class Parser {
        private final List<Token> tokens;
        private int at, depth, filters, subscripts;
        Parser(String source) { tokens = new Scanner(source).scan(); }
        JsonPath parse() {
            if (peek().kind.equals("end")) throw new SqlException("22P02", "empty jsonpath");
            boolean strict = takeWord("strict"); if (!strict) takeWord("lax");
            Node node = expression(0); if (!peek().kind.equals("end")) throw syntax();
            return new JsonPath(strict, node);
        }
        private Node expression(int min) {
            Node left = prefix(); int chains = 0;
            for (;;) {
                String op = peek().text.toLowerCase(Locale.ROOT); int precedence = precedence(op);
                if (precedence < min) break;
                boolean nesting = precedence != 3;
                if (nesting) checkDepth(++chains + depth);
                at++;
                if (op.equals("starts")) {
                    requireWord("with"); if (left.predicate()) throw syntax();
                    Node right;
                    if (peek().kind.equals("string")) right = new Node("string", next().text);
                    else if (peek().kind.equals("variable")) right = new Node("variable", next().text);
                    else throw syntax();
                    left = new Node("starts", "starts with", left, right); continue;
                }
                if (op.equals("like_regex")) {
                    if (left.predicate() || !peek().kind.equals("string")) throw syntax();
                    String pattern = next().text, flags = "";
                    if (takeWord("flag")) { if (!peek().kind.equals("string")) throw syntax(); String input = next().text; for (char c : input.toCharArray()) if ("ismxq".indexOf(c) < 0) throw syntax(); for (char c : "ismxq".toCharArray()) if (input.indexOf(c) >= 0) flags += c; }
                    if (flags.indexOf('x') >= 0 && flags.indexOf('q') < 0) throw new SqlException("0A000", "jsonpath regular expression flag x is not supported");
                    RegexProgram.compile(pattern, flags, true);
                    left = new Node("regex", pattern, left, new Node("flags", flags)); continue;
                }
                if (nesting) enterDepth();
                Node right = expression(precedence + 1);
                if (nesting) depth--;
                boolean connective = op.equals("&&") || op.equals("||");
                if (connective != left.predicate() || connective != right.predicate()) throw syntax();
                String kind = connective ? op.equals("&&") ? "and" : "or" : precedence == 3 ? "compare" : "binary";
                left = new Node(kind, op.equals("<>") ? "!=" : op, left, right);
            }
            return left;
        }
        private void enterDepth() { checkDepth(++depth); }
        private void checkDepth(int value) { if (value > 256) throw new SqlException("54001", "jsonpath nesting depth exceeds the maximum of 256"); }
        private static int precedence(String op) { return switch (op) { case "||" -> 1; case "&&" -> 2; case "==", "!=", "<>", "<", "<=", ">", ">=", "starts", "like_regex" -> 3; case "+", "-" -> 4; case "*", "/", "%" -> 5; default -> -1; }; }
        private Node prefix() {
            if (take("+") || take("-")) {
                String op = tokens.get(at - 1).text; enterDepth(); Node value = expression(6); depth--; if (value.predicate()) throw syntax();
                if (value.kind.equals("number")) { Decimal number = Decimal.parse(value.text); return new Node("number", (op.equals("-") ? number.negate() : number).toString()); }
                return new Node("unary", op, value);
            }
            if (take("!")) {
                if (!peek().text.equals("(") && !word("exists")) throw syntax();
                enterDepth(); Node value; if (take("(")) { value = expression(0); require(")"); } else value = prefix(); depth--; if (!value.predicate()) throw syntax(); return new Node("not", "", value);
            }
            if (takeWord("exists")) { require("("); enterDepth(); Node value = expression(0); depth--; require(")"); if (value.predicate()) throw syntax(); return new Node("exists", "", value); }
            Node base;
            boolean grouped = false;
            if (take("(")) { enterDepth(); base = expression(0); depth--; require(")"); grouped = true; }
            else {
                Token token = next();
                base = switch (token.kind) {
                    case "string" -> new Node("string", token.text);
                    case "number" -> new Node("number", Decimal.parse(token.text).toString());
                    case "variable" -> new Node("variable", token.text);
                    case "symbol" -> {
                        if (token.text.equals("$")) yield new Node("root", "");
                        if (token.text.equals("@") && filters > 0) yield new Node("current", "");
                        throw syntax();
                    }
                    case "name" -> {
                        if (Set.of("null", "true", "false").contains(token.text)) yield new Node("literal", token.text);
                        if (token.text.equalsIgnoreCase("last") && subscripts > 0) yield new Node("last", "");
                        throw syntax();
                    }
                    default -> throw syntax();
                };
            }
            if (grouped && takeWord("is")) { requireWord("unknown"); if (!base.predicate()) throw syntax(); base = new Node("unknown", "", base); }
            return accessors(base);
        }
        private Node accessors(Node base) {
            for (;;) {
                if (take(".")) {
                    if (take("*")) { base = new Node("members", "", base); continue; }
                    if (take("**")) {
                        String levels = "";
                        if (take("{")) { String low = level(), high = takeWord("to") ? level() : low; require("}"); levels = low.equals("0") && high.equals("last") ? "" : "{" + low + (low.equals(high) ? "" : " to " + high) + "}"; }
                        base = new Node("any", levels, base); continue;
                    }
                    Token member = next(); if (!member.kind.equals("string") && !member.kind.equals("name")) throw syntax();
                    if (take("(")) {
                        if (!member.kind.equals("name")) throw syntax();
                        String name = member.text.toLowerCase(Locale.ROOT);
                        if (!Set.of("type", "size", "double", "ceiling", "floor", "abs", "keyvalue", "bigint", "boolean", "integer", "number", "string", "date", "decimal", "datetime", "time", "time_tz", "timestamp", "timestamp_tz").contains(name)) throw syntax();
                        ArrayList<Node> args = new ArrayList<>(); args.add(base);
                        if (!take(")")) {
                            do {
                                if (name.equals("datetime") && args.size() == 1 && peek().kind.equals("string")) args.add(new Node("string", next().text));
                                else if (Set.of("decimal", "time", "time_tz", "timestamp", "timestamp_tz").contains(name)) {
                                    String sign = take("-") ? "-" : take("+") ? "+" : ""; Token n = next(); if (!n.kind.equals("number") || !n.text.matches("0|[1-9][0-9]*")) throw syntax();
                                    if (!name.equals("decimal") && !sign.isEmpty()) throw syntax();
                                    args.add(new Node("number", n.text.equals("0") || sign.equals("+") ? n.text : sign + n.text));
                                } else throw syntax();
                            } while (take(","));
                            require(")");
                        }
                        if (args.size() > (name.equals("decimal") ? 3 : 2)) throw syntax();
                        if (Set.of("date", "datetime", "time", "time_tz", "timestamp", "timestamp_tz").contains(name)) throw new SqlException("0A000", "jsonpath datetime methods are not supported");
                        base = new Node("method", name, args);
                    } else base = new Node("member", member.text, base);
                } else if (take("[")) {
                    if (take("*")) { require("]"); base = new Node("elements", "", base); continue; }
                    ArrayList<Node> args = new ArrayList<>(); args.add(base); subscripts++; enterDepth();
                    do { Node index = expression(0); if (index.predicate()) throw syntax(); if (takeWord("to")) { Node end = expression(0); if (end.predicate()) throw syntax(); index = new Node("slice", "", index, end); } args.add(index); } while (take(","));
                    subscripts--; depth--; require("]"); base = new Node("index", "", args);
                } else if (take("?")) {
                    require("("); filters++; enterDepth(); Node predicate = expression(0); filters--; depth--; require(")"); if (!predicate.predicate()) throw syntax(); base = new Node("filter", "", base, predicate);
                } else return base;
            }
        }
        private String level() { if (takeWord("last")) return "last"; Token n = next(); if (!n.kind.equals("number") || !n.text.matches("0|[1-9][0-9]*")) throw syntax(); try { return Integer.toString(Integer.parseInt(n.text)); } catch (NumberFormatException failure) { throw SqlException.numericOverflow(); } }
        private Token peek() { return tokens.get(at); }
        private Token next() { Token result = peek(); if (!result.kind.equals("end")) at++; return result; }
        private boolean take(String s) { if (peek().kind.equals("symbol") && peek().text.equals(s)) { at++; return true; } return false; }
        private boolean word(String s) { return peek().kind.equals("name") && peek().text.equalsIgnoreCase(s); }
        private boolean takeWord(String s) { if (word(s)) { at++; return true; } return false; }
        private void require(String s) { if (!take(s)) throw syntax(); }
        private void requireWord(String s) { if (!takeWord(s)) throw syntax(); }
    }
    private static final class Scanner {
        private final String source;
        private int at;
        Scanner(String source) { SqlText.validate(source); this.source = source; }
        List<Token> scan() {
            ArrayList<Token> result = new ArrayList<>();
            while (at < source.length()) {
                char c = source.charAt(at); if (space(c)) { at++; continue; }
                if (comment()) continue;
                int start = at;
                if (c == '"') { at++; result.add(new Token("string", string(true), start, at)); continue; }
                if (c == '$') {
                    at++;
                    if (at < source.length() && source.charAt(at) == '"') { at++; result.add(new Token("variable", string(true), start, at)); }
                    else if (at < source.length() && nameChar(source.charAt(at)) && source.charAt(at) != '\\') { int first = at; while (at < source.length() && nameChar(source.charAt(at)) && source.charAt(at) != '\\') at++; result.add(new Token("variable", source.substring(first, at), start, at)); }
                    else result.add(new Token("symbol", "$", start, at));
                    continue;
                }
                if (digit(c) || c == '.' && at + 1 < source.length() && digit(source.charAt(at + 1))) {
                    int end = numberEnd(at), competingEnd = pgNumberEnd(at), nameEnd = at; while (nameEnd < source.length() && nameChar(source.charAt(nameEnd))) nameEnd++;
                    boolean radix = at + 1 < source.length() && source.charAt(at) == '0' && "xXoObB".indexOf(source.charAt(at + 1)) >= 0;
                    boolean numeric = nameEnd <= competingEnd || SqlText.encodedLength(source.substring(at, nameEnd)) <= SqlText.encodedLength(source.substring(at, competingEnd)) + (radix ? 0 : 1);
                    if (numeric) {
                        at = end; if (at < source.length() && nameChar(source.charAt(at))) throw syntax();
                        String number = source.substring(start, at);
                        if (!number.matches("(?:0|[1-9][0-9]*)(?:\\.[0-9]*)?(?:[eE][+-]?[0-9]+)?|\\.[0-9]+(?:[eE][+-]?[0-9]+)?")) throw syntax();
                        result.add(new Token("number", number, start, at)); continue;
                    }
                }
                if (nameChar(c)) {
                    String name = string(false);
                    if (at + 1 < source.length() && source.startsWith("/*", at)) { comment(); continue; }
                    result.add(new Token("name", name, start, at)); continue;
                }
                String op = at + 1 < source.length() ? source.substring(at, at + 2) : "";
                if (Set.of("**", "==", "!=", "<>", "<=", ">=", "&&", "||").contains(op)) { at += 2; result.add(new Token("symbol", op, start, at)); }
                else { at++; result.add(new Token("symbol", String.valueOf(c), start, at)); }
            }
            result.add(new Token("end", "", at, at)); return result;
        }
        private int numberEnd(int i) {
            if (source.charAt(i) == '0') i++; else while (i < source.length() && digit(source.charAt(i))) i++;
            if (i < source.length() && source.charAt(i) == '.') { i++; while (i < source.length() && digit(source.charAt(i))) i++; }
            if (i < source.length() && (source.charAt(i) == 'e' || source.charAt(i) == 'E')) { int exponent = i; i++; if (i < source.length() && (source.charAt(i) == '+' || source.charAt(i) == '-')) i++; int first = i; while (i < source.length() && digit(source.charAt(i))) i++; if (first == i) return exponent; }
            return i;
        }
        private int pgNumberEnd(int start) {
            int end = numberEnd(start), i = start;
            if (i + 2 < source.length() && source.charAt(i) == '0' && "xXoObB".indexOf(source.charAt(i + 1)) >= 0) {
                char prefix = source.charAt(i + 1); int radix = prefix == 'x' || prefix == 'X' ? 16 : prefix == 'o' || prefix == 'O' ? 8 : 2; i += 2;
                while (i < source.length()) { char c = source.charAt(i); if (Character.digit(c, radix) >= 0 && c < 128) i++; else if (c == '_' && i + 1 < source.length() && Character.digit(source.charAt(i + 1), radix) >= 0) i++; else break; }
                return Math.max(end, i);
            }
            while (i < source.length()) { char c = source.charAt(i); if (digit(c)) i++; else if (c == '_' && i + 1 < source.length() && digit(source.charAt(i + 1))) i++; else break; }
            return Math.max(end, i);
        }
        private boolean comment() {
            if (!source.startsWith("/*", at)) return false;
            int end = source.indexOf("*/", at + 2); if (end < 0) throw syntax(); at = end + 2; return true;
        }
        private String string(boolean quoted) {
            StringBuilder value = new StringBuilder();
            while (at < source.length()) {
                char c = source.charAt(at);
                if (quoted && c == '"') { at++; return value.toString(); }
                if (!quoted && !nameChar(c)) return value.toString();
                at++;
                if (c != '\\') { value.append(c); continue; }
                if (at == source.length()) throw syntax(); c = source.charAt(at++);
                switch (c) {
                    case 'b' -> value.append('\b'); case 'f' -> value.append('\f'); case 'n' -> value.append('\n'); case 'r' -> value.append('\r'); case 't' -> value.append('\t'); case 'v' -> value.append('\u000b');
                    case 'x' -> appendCode(value, hex(2));
                    case 'u' -> {
                        ArrayList<Integer> run = new ArrayList<>(); run.add(unicode());
                        while (source.startsWith("\\u", at)) { at += 2; run.add(unicode()); }
                        for (int i = 0; i < run.size(); i++) {
                            int code = run.get(i);
                            if (code >= 0xD800 && code <= 0xDBFF) { if (++i == run.size() || run.get(i) < 0xDC00 || run.get(i) > 0xDFFF) throw new SqlException("22P02", "invalid Unicode surrogate pair"); code = 0x10000 + ((code - 0xD800) << 10) + run.get(i) - 0xDC00; }
                            else if (code >= 0xDC00 && code <= 0xDFFF) throw new SqlException("22P02", "invalid Unicode surrogate pair");
                            appendCode(value, code);
                        }
                    }
                    default -> value.append(c);
                }
            }
            if (quoted) throw syntax(); return value.toString();
        }
        private int unicode() {
            if (at < source.length() && source.charAt(at) == '{') { at++; int start = at; while (at < source.length() && source.charAt(at) != '}') at++; int n = at - start; if (at == source.length() || n < 1 || n > 6) throw syntax(); at = start; int code = hex(n); at++; if (code > 0x10FFFF) throw syntax(); return code; }
            return hex(4);
        }
        private int hex(int count) { int result = 0; for (int i = 0; i < count; i++) { if (at == source.length()) throw syntax(); int digit = Character.digit(source.charAt(at++), 16); if (digit < 0) throw syntax(); result = (result << 4) | digit; } return result; }
        private static void appendCode(StringBuilder value, int code) { if (code == 0) throw new SqlException("22P05", "Unicode NUL cannot be converted to text"); value.appendCodePoint(code); }
        private static boolean space(char c) { return " \t\n\r\f".indexOf(c) >= 0; }
        private static boolean digit(char c) { return c >= '0' && c <= '9'; }
        private static boolean nameChar(char c) { return !space(c) && "?%$.[]{}()|&!=<>@#,*:-+/\"".indexOf(c) < 0; }
    }
    private static SqlException syntax() { return new SqlException("42601", "invalid jsonpath syntax"); }
}
