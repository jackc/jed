package org.jed;

import java.util.ArrayList;
import java.util.List;

/** VARIADIC any text builders, with exact admission before amplified output allocation. */
final class TextBuilders {
    private TextBuilders() {}
    static ScalarPlan.Expression resolve(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        String name = call.name();
        if (!List.of("concat", "concat_ws", "format").contains(name)) return null;
        int fixed = name.equals("concat") ? 0 : 1;
        if (call.args().size() < (name.equals("format") ? 1 : fixed + 1)) throw new SqlException("42883", "text builder requires arguments");
        if (call.distinct() || call.star() || !call.order().isEmpty() || call.filter() != null) throw new SqlException("42809", "text builder is not an aggregate");
        List<ScalarPlan.Expression> args = new ArrayList<>(); boolean spread = false;
        for (int i = 0; i < call.args().size(); i++) {
            SqlParser.Expr syntax = call.args().get(i);
            if (syntax instanceof SqlParser.NamedArgument) throw new SqlException("42883", "text builder has no named parameters");
            if (syntax instanceof SqlParser.Unary unary && unary.op().equals("variadic")) {
                if (i != fixed || i != call.args().size() - 1) throw new SqlException("42883", "invalid variadic arguments");
                spread = true; syntax = unary.value();
            }
            ScalarPlan.Expression value = resolver.resolve(syntax, i < fixed ? SqlType.TEXT : null);
            if (i < fixed && value.type() != SqlType.UNKNOWN && value.type().kind() != SqlType.Kind.TEXT) throw new SqlException("42883", "text builder requires a text first argument");
            if (spread && value.type().kind() != SqlType.Kind.ARRAY) throw SqlException.typeMismatch("VARIADIC argument must be an array");
            args.add(value);
        }
        boolean array = spread, constant = args.stream().allMatch(ScalarPlan.Expression::constant);
        return new ScalarPlan.Expression(SqlType.TEXT, meter -> {
            meter.operation(); List<SqlValue> values = new ArrayList<>();
            for (ScalarPlan.Expression arg : args) values.add(arg.run(meter));
            if (fixed != 0 && values.getFirst().isNull()) return SqlValue.nullValue(SqlType.TEXT);
            String prefix = fixed == 0 ? "" : values.getFirst().asText();
            List<SqlValue> tail = values.subList(fixed, values.size());
            if (array) {
                SqlValue packed = tail.getFirst();
                if (packed.isNull() && !name.equals("format")) return SqlValue.nullValue(SqlType.TEXT);
                tail = packed.isNull() ? List.of() : packed.asArray().elements();
            }
            if (name.equals("format")) return SqlValue.ofText(format(prefix, tail, meter));
            List<String> parts = new ArrayList<>(); long bytes = 0;
            for (SqlValue value : tail) if (!value.isNull()) {
                String text = output(value, meter); parts.add(text); bytes += SqlText.encodedLength(text);
            }
            if (name.equals("concat_ws")) {
                bytes += SqlText.encodedLength(prefix) * Math.max(0, parts.size() - 1);
                admit(bytes, meter);
            } else if (!constant) meter.scalarInput(bytes);
            String result = String.join(prefix, parts);
            return SqlValue.ofText(result);
        }, constant);
    }
    static String output(SqlValue value, CostMeter meter) {
        CompositeValues.chargeZones(value, meter);
        if (value.type() == SqlType.BOOLEAN) return value.asBoolean() ? "t" : "f";
        return Temporal.renderValue(value, meter.zone());
    }
    private static void admit(long bytes, CostMeter meter) {
        if (bytes > 0x3fffffffL) throw new SqlException("54000", "requested length too large");
        meter.scalarOutput(bytes);
    }
    private record Piece(String text, int padding, boolean left) {
        long bytes() { return SqlText.encodedLength(text) + padding; }
    }
    private static String format(String format, List<SqlValue> args, CostMeter meter) {
        meter.scalarInput(SqlText.encodedLength(format));
        List<Piece> pieces = new ArrayList<>(); long bytes = 0; int at = 0, next = 1;
        while (at < format.length()) {
            if (format.charAt(at) != '%') {
                int end = format.indexOf('%', at); if (end < 0) end = format.length();
                Piece piece = new Piece(format.substring(at, end), 0, false); pieces.add(piece); bytes += piece.bytes(); at = end; continue;
            }
            at++;
            if (at < format.length() && format.charAt(at) == '%') { pieces.add(new Piece("%", 0, false)); bytes++; at++; continue; }
            int position = 0, width = 0; boolean left = false;
            int start = at; while (at < format.length() && digit(format.charAt(at))) at++;
            if (at > start) {
                int number = number(format, start, at);
                if (at < format.length() && format.charAt(at) == '$') { position = position(number); at++; }
                else width = number;
            }
            while (at < format.length() && format.charAt(at) == '-') { left = true; at++; }
            if (at < format.length() && format.charAt(at) == '*') {
                at++; start = at; while (at < format.length() && digit(format.charAt(at))) at++;
                int widthPosition = next;
                if (at > start) {
                    widthPosition = position(number(format, start, at));
                    if (at == format.length() || format.charAt(at) != '$') throw invalid("width argument position must be ended by \"$\"");
                    at++;
                }
                SqlValue value = argument(args, widthPosition); next = widthPosition + 1;
                if (!value.isNull()) {
                    long requested = value.type().isInteger() ? value.asInteger() : SqlCasts.literal(output(value, meter), SqlType.I32, meter).asInteger();
                    if (requested < -2147483647L || requested > 2147483647L) throw range();
                    left |= requested < 0; width = (int) Math.abs(requested);
                } else width = 0;
            } else {
                start = at; while (at < format.length() && digit(format.charAt(at))) at++;
                if (at > start) width = number(format, start, at);
            }
            if (at == format.length()) throw invalid("unterminated format() type specifier");
            int type = format.codePointAt(at); at += Character.charCount(type);
            if (type != 's' && type != 'I' && type != 'L') throw invalid("unrecognized format() type specifier \"" + new String(Character.toChars(type)) + "\"");
            if (position == 0) position = next;
            SqlValue value = argument(args, position); next = position + 1;
            String text;
            if (value.isNull()) {
                if (type == 'I') throw new SqlException("22004", "null values cannot be formatted as an SQL identifier");
                text = type == 'L' ? "NULL" : "";
            } else {
                text = output(value, meter);
                if (type == 'I') text = TextQuotes.identifier(text);
                else if (type == 'L') text = TextQuotes.literal(text);
            }
            Piece piece = new Piece(text, Math.max(0, width - SqlText.length(text)), left);
            pieces.add(piece); bytes += piece.bytes();
        }
        admit(bytes, meter);
        StringBuilder output = new StringBuilder((int) bytes);
        for (Piece piece : pieces) {
            if (!piece.left()) spaces(output, piece.padding());
            output.append(piece.text());
            if (piece.left()) spaces(output, piece.padding());
        }
        return output.toString();
    }
    private static void spaces(StringBuilder output, int count) { for (int i = 0; i < count; i++) output.append(' '); }
    private static boolean digit(char c) { return c >= '0' && c <= '9'; }
    private static int number(String input, int start, int end) {
        long value = 0;
        for (int i = start; i < end; i++) { value = value * 10 + input.charAt(i) - '0'; if (value > Integer.MAX_VALUE) throw range(); }
        return (int) value;
    }
    private static int position(int value) {
        if (value == 0) throw invalid("format specifies argument 0, but arguments are numbered from 1");
        return value;
    }
    private static SqlValue argument(List<SqlValue> args, int position) {
        if (position <= 0 || position > args.size()) throw invalid("too few arguments for format()");
        return args.get(position - 1);
    }
    private static SqlException invalid(String message) { return new SqlException("22023", message); }
    private static SqlException range() { return new SqlException("22003", "number is out of range"); }
}
