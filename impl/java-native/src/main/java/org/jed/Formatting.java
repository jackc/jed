package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Handwritten template scanner shared by numeric and calendar formatting. */
final class Formatting {
    private Formatting() {}
    record Node(String text, FormattingCatalog.Keyword keyword, boolean fill, boolean translated, String suffix) {
        String id() { return keyword == null ? "" : keyword.id(); }
    }
    static List<Node> parse(String input, boolean datetime) {
        List<Node> nodes = new ArrayList<>(); List<FormattingCatalog.Keyword> keywords = datetime ? FormattingCatalog.DCH : FormattingCatalog.NUM;
        int at = 0;
        while (at < input.length()) {
            boolean fill = false, translated = false;
            if (datetime && (input.startsWith("FM", at) || input.startsWith("fm", at))) { fill = true; at += 2; }
            else if (datetime && (input.startsWith("TM", at) || input.startsWith("tm", at))) { translated = true; at += 2; }
            FormattingCatalog.Keyword keyword = null;
            for (FormattingCatalog.Keyword candidate : keywords) if (input.startsWith(candidate.name(), at)) { keyword = candidate; break; }
            if (keyword != null) {
                at += keyword.name().length(); String suffix = "";
                if (datetime && (input.startsWith("TH", at) || input.startsWith("th", at) || input.startsWith("SP", at))) { suffix = input.substring(at, at + 2); at += 2; }
                nodes.add(new Node(keyword.name(), keyword, fill, translated, suffix)); continue;
            }
            if (at >= input.length()) break;
            if (input.charAt(at) == '"') {
                at++;
                while (at < input.length() && input.charAt(at) != '"') {
                    if (input.charAt(at) == '\\' && at + 1 < input.length()) at++;
                    int end = at + Character.charCount(input.codePointAt(at)); nodes.add(new Node(input.substring(at, end), null, false, false, "quoted")); at = end;
                }
                if (at < input.length()) at++;
            } else {
                if (input.charAt(at) == '\\' && at + 1 < input.length() && input.charAt(at + 1) == '"') at++;
                int end = at + Character.charCount(input.codePointAt(at)); nodes.add(new Node(input.substring(at, end), null, false, false, "")); at = end;
            }
        }
        return nodes;
    }
    static String number(long value, int width) {
        String digits = Long.toString(value); int padding = Math.max(0, width - digits.length());
        if (padding == 0) return digits;
        return value < 0 ? "-" + "0".repeat(padding) + digits.substring(1) : "0".repeat(padding) + digits;
    }
    static String ordinal(long value, boolean upper) {
        value = Math.abs(value); int last = (int) (value % 10);
        int index = value % 100 >= 11 && value % 100 <= 13 ? 3 : last >= 1 && last <= 3 ? last - 1 : 3;
        return (upper ? FormattingCatalog.ORDINAL_UPPER : FormattingCatalog.ORDINAL_LOWER).get(index);
    }
    static ScalarPlan.Expression resolve(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        String name = call.name();
        if (!List.of("to_char", "to_date", "to_timestamp", "to_number").contains(name)) return null;
        if (call.args().size() == 1 && name.equals("to_timestamp")) {
            ScalarPlan.Expression input = resolver.resolve(call.args().getFirst(), null);
            if (!input.type().isFloat() && input.type() != SqlType.UNKNOWN) throw new SqlException("42883", "to_timestamp epoch requires f64");
            return new ScalarPlan.Expression(SqlType.TIMESTAMPTZ, meter -> {
                meter.operation(); SqlValue value = input.run(meter); if (value.isNull()) return SqlValue.nullValue(SqlType.TIMESTAMPTZ);
                double seconds = value.asFloat(); if (Double.isNaN(seconds)) throw Temporal.overflow();
                if (Double.isInfinite(seconds)) return SqlValue.ofTimestamp(SqlType.TIMESTAMPTZ, seconds < 0 ? Long.MIN_VALUE : Long.MAX_VALUE);
                double shifted = Math.rint((seconds - 946684800.0) * 1_000_000.0);
                if (shifted <= Long.MIN_VALUE || shifted >= Long.MAX_VALUE) throw Temporal.overflow();
                try { return SqlValue.ofTimestamp(SqlType.TIMESTAMPTZ, Math.addExact((long) shifted, 946684800000000L)); }
                catch (ArithmeticException error) { throw Temporal.overflow(); }
            }, input.constant());
        }
        if (call.args().size() != 2) throw new SqlException("42883", "formatting function requires two arguments");
        ScalarPlan.Expression input = resolver.resolve(call.args().getFirst(), null), template = resolver.resolve(call.args().get(1), SqlType.TEXT);
        if (template.type() != SqlType.UNKNOWN && template.type().kind() != SqlType.Kind.TEXT) throw new SqlException("42883", "formatting template requires text");
        boolean render = name.equals("to_char"), date = input.type().kind() == SqlType.Kind.TIMESTAMP || input.type().kind() == SqlType.Kind.TIMESTAMPTZ || input.type().kind() == SqlType.Kind.INTERVAL;
        if (render && (input.type() == SqlType.UNKNOWN || call.args().getFirst() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING)) throw new SqlException("42725", "to_char input is ambiguous");
        if (render ? !date && !input.type().isNumeric() : input.type() != SqlType.UNKNOWN && input.type().kind() != SqlType.Kind.TEXT) throw new SqlException("42883", "formatting input type is not supported");
        SqlType type = render ? SqlType.TEXT : name.equals("to_date") ? SqlType.DATE : name.equals("to_number") ? SqlType.DECIMAL : SqlType.TIMESTAMPTZ;
        return new ScalarPlan.Expression(type, meter -> {
            meter.operation(); SqlValue value = input.run(meter), pattern = template.run(meter);
            if (value.isNull() || pattern.isNull()) return SqlValue.nullValue(type);
            if (render) {
                String output = date ? DateTimeFormat.render(value, pattern.asText(), meter) : NumberFormat.render(value, pattern.asText());
                if (output == null) return SqlValue.nullValue(type);
                if (!input.constant() || !template.constant()) meter.scalarInput(SqlText.encodedLength(output));
                return SqlValue.ofText(output);
            }
            if (name.equals("to_number")) { Decimal output = NumberFormat.read(value.asText(), pattern.asText(), meter); return output == null ? SqlValue.nullValue(type) : SqlValue.ofDecimal(output); }
            return DateTimeFormat.read(value.asText(), pattern.asText(), type, meter);
        }, input.constant() && template.constant());
    }
}
