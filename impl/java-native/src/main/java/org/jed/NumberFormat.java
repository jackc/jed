package org.jed;

import java.util.List;
import java.util.Locale;
import java.util.Set;

/** Exact numeric template processing; floats enter through their binary decimal expansion. */
final class NumberFormat {
    private NumberFormat() {}
    private static final class Description {
        final List<Formatting.Node> nodes;
        int pre, post, multiplier, zero = -1, lastZero = -1;
        boolean point, fill, roman, lowerRoman, exponent, blank, postSign;
        String sign = "";
        Description(String template) {
            nodes = Formatting.parse(template, false); boolean afterV = false; int digits = 0, signCount = 0, romanCount = 0;
            for (int i = 0; i < nodes.size(); i++) {
                Formatting.Node node = nodes.get(i); if (node.keyword() == null) continue;
                String id = node.id();
                switch (id) {
                    case "9", "0" -> { if (sign.equals("PR")) throw syntax(); if (id.equals("0")) { if (zero < 0) zero = digits; lastZero = digits; } digits++; if (point) post++; else pre++; if (afterV) multiplier++; }
                    case "DEC", "D" -> { if (point || afterV) throw syntax(); point = true; }
                    case "V" -> { if (point) throw syntax(); afterV = true; }
                    case "FM" -> fill = true;
                    case "B" -> blank = true;
                    case "RN", "rn" -> { roman = true; lowerRoman = id.equals("rn"); romanCount++; }
                    case "S", "MI", "PL", "SG", "PR" -> {
                        if (id.equals("S") && signCount != 0 || sign.equals("S") || id.equals("PR") && digits == 0) throw syntax();
                        sign = id; signCount++; postSign = digits != 0;
                    }
                    case "E" -> { if (i != nodes.size() - 1 || exponent) throw syntax(); exponent = true; }
                    default -> { }
                }
            }
            if (roman && (romanCount != 1 || signCount != 0 || point || exponent || multiplier != 0)) throw syntax();
            if (exponent && (fill || signCount != 0 || multiplier != 0 || roman || blank || zero >= 0 || nodes.stream().anyMatch(n -> Set.of("G", "COMMA", "L", "TH", "th").contains(n.id())))) throw syntax();
        }
    }
    static String render(SqlValue value, String template) {
        Description d = new Description(template); if (template.isEmpty()) return "";
        boolean floating = value.type().isFloat(), finite = !floating || Double.isFinite(value.asFloat());
        boolean negative = floating ? Double.doubleToRawLongBits(value.asFloat()) < 0 : value.type().isInteger() ? value.asInteger() < 0 : value.asDecimal().negative();
        Decimal number = finite ? value.type().isInteger() ? Decimal.of(value.asInteger()) : floating ? Decimal.fromDoubleExact(value.asFloat()) : value.asDecimal() : Decimal.zero(0);
        if (d.multiplier > 0 && finite) {
            if (floating) {
                double power = d.multiplier > 308 ? Double.POSITIVE_INFINITY : Double.parseDouble("1e" + d.multiplier);
                if (d.multiplier == 23 || d.multiplier == 210) power = Math.nextUp(power);
                double product = value.type() == SqlType.F32 ? (float) (value.asFloat() * (float) power) : value.asFloat() * power;
                finite = Double.isFinite(product); if (finite) number = Decimal.fromDoubleExact(product);
            } else {
                number = number.multiply(Decimal.parse("1e" + d.multiplier));
                if (value.type().isInteger()) SqlValue.ofInteger(value.type(), number.toLongExact());
            }
        }
        if (d.roman) {
            int integer = 0;
            if (finite) { try { integer = Math.toIntExact(floating ? (negative ? -even(number.abs(), 0).toLongExact() : even(number, 0).toLongExact()) : number.toLongRounded()); } catch (ArithmeticException | SqlException ignored) { } }
            String text = roman(integer); if (d.lowerRoman) text = text.toLowerCase(Locale.ROOT);
            return d.fill ? text : " ".repeat(Math.max(0, 15 - text.length())) + text;
        }
        if (d.exponent) {
            if (!finite) return " " + "#".repeat(d.pre) + (d.point ? "." : "") + "#".repeat(d.post) + "####";
            int exponent = number.isZero() ? 0 : number.precision() - number.scale() - 1;
            Decimal mantissa = Decimal.parse(number.abs().toString() + "e" + -exponent);
            mantissa = floating || value.type() == SqlType.I16 || value.type() == SqlType.I32 ? even(mantissa, d.post) : mantissa.rescale(d.post);
            if ((floating || value.type() == SqlType.I16 || value.type() == SqlType.I32) && mantissa.compareTo(Decimal.of(10)) >= 0) { exponent++; mantissa = mantissa.divide(Decimal.of(10), d.post); }
            return (negative ? "-" : " ") + mantissa.toString() + "e" + (exponent < 0 ? "-" : "+") + Formatting.number(Math.abs(exponent), 2);
        }
        int fractional = d.post;
        if (floating) fractional = Math.min(fractional, Math.max(0, (value.type() == SqlType.F32 ? 6 : 15) - (finite ? Math.max(1, number.integerDigits()) : Double.isNaN(value.asFloat()) ? 3 : 8)));
        Decimal rounded = floating ? even(number.abs(), fractional) : number.abs().rescale(fractional);
        String image = finite ? rounded.toString() : Double.isNaN(value.asFloat()) ? "NaN" : "Infinity";
        int point = image.indexOf('.'); String integer = point < 0 ? image : image.substring(0, point), fraction = point < 0 ? "" : image.substring(point + 1);
        boolean overflow = integer.length() > d.pre;
        if (overflow) { integer = "#".repeat(d.pre); fraction = "#".repeat(fractional); }
        int skip = Math.max(0, d.pre - integer.length());
        if (!overflow && finite && integer.equals("0") && d.post > 0 && ((d.zero < 0 && (!d.fill || !rounded.isZero())) || d.zero >= d.pre)) { integer = ""; skip = d.pre; }
        if (d.fill && !overflow) {
            int keep = Math.max(0, d.lastZero - d.pre + 1), end = fraction.length();
            while (end > keep && fraction.charAt(end - 1) == '0') end--;
            fraction = fraction.substring(0, end);
        }
        StringBuilder out = new StringBuilder(); boolean after = false, started = false, signWritten = false; int beforeIndex = 0, afterIndex = 0;
        boolean defaultSign = d.sign.isEmpty() || d.sign.equals("PL");
        for (Formatting.Node node : d.nodes) {
            String id = node.id();
            if (floating && !overflow && (fractional == 0 || !finite) && (id.equals("DEC") || id.equals("D"))) break;
            if (node.keyword() == null) { out.append(node.text()); continue; }
            if (id.equals("9") || id.equals("0")) {
                char digit = 0;
                if (!after) {
                    if (beforeIndex >= skip) digit = integer.charAt(beforeIndex - skip);
                    else if (d.zero >= 0 && beforeIndex >= d.zero) digit = '0';
                    beforeIndex++;
                } else { if (afterIndex < fraction.length()) digit = fraction.charAt(afterIndex); afterIndex++; }
                if (digit != 0) {
                    if (!signWritten) { if (defaultSign) out.append(negative ? "-" : d.fill ? "" : " "); else if (d.sign.equals("PR")) out.append(negative ? "<" : d.fill ? "" : " "); else if (d.sign.equals("S") && !d.postSign) out.append(negative ? "-" : "+"); signWritten = true; }
                    out.append(digit); started = true;
                } else if (!d.fill) {
                    if (after && floating) break;
                    out.append(' ');
                }
            } else switch (id) {
                case "DEC", "D" -> {
                    if (d.post == 0) { after = true; break; }
                    if (!signWritten) { if (defaultSign) out.append(negative ? "-" : d.fill ? "" : " "); else if (d.sign.equals("PR")) out.append(negative ? "<" : d.fill ? "" : " "); else if (d.sign.equals("S") && !d.postSign) out.append(negative ? "-" : "+"); signWritten = true; }
                    out.append('.'); after = true;
                }
                case "COMMA", "G" -> { if (started) out.append(','); else if (!d.fill) out.append(' '); }
                case "L" -> out.append(' ');
                case "S" -> { if (d.postSign) out.append(negative ? '-' : '+'); }
                case "MI" -> out.append(negative ? "-" : d.fill ? "" : " ");
                case "PL" -> out.append(negative ? d.fill ? "" : " " : "+");
                case "SG" -> out.append(negative ? '-' : '+');
                case "PR" -> out.append(negative ? ">" : d.fill ? "" : " ");
                case "TH", "th" -> { if (!negative && !d.point && finite && !overflow) out.append(Formatting.ordinal(rounded.toLongRounded(), id.equals("TH"))); }
                default -> { }
            }
        }
        return out.toString();
    }
    private static Decimal even(Decimal number, int scale) {
        Decimal rounded = number.rescale(scale);
        if (number.scale() <= scale) return rounded;
        String digits = number.coefficientDigits(); int discarded = number.scale() - scale, cut = digits.length() - discarded;
        if (cut < 0 || cut >= digits.length() || digits.charAt(cut) != '5') return rounded;
        for (int i = cut + 1; i < digits.length(); i++) if (digits.charAt(i) != '0') return rounded;
        String coefficient = rounded.coefficientDigits();
        if ((coefficient.charAt(coefficient.length() - 1) - '0') % 2 != 0) rounded = rounded.subtract(Decimal.parse("1e-" + scale));
        return rounded;
    }
    static Decimal read(String input, String template, CostMeter meter) {
        if (template.isEmpty()) return null;
        Description d = new Description(template); if (d.exponent) throw new SqlException("0A000", "EEEE not supported for input");
        if (d.roman) {
            String text = input.trim(); int space = text.indexOf(' '); if (space >= 0) text = text.substring(0, space); text = text.toUpperCase(Locale.ROOT); int value = 0, previous = 0;
            for (int i = text.length() - 1; i >= 0; i--) { int digit = switch (text.charAt(i)) { case 'I' -> 1; case 'V' -> 5; case 'X' -> 10; case 'L' -> 50; case 'C' -> 100; case 'D' -> 500; case 'M' -> 1000; default -> 0; }; if (digit == 0) throw invalid(); value += digit < previous ? -digit : digit; previous = digit; }
            if (value < 1 || value > 3999 || !roman(value).equals(text)) throw invalid(); return Decimal.of(value);
        }
        StringBuilder digits = new StringBuilder(); int at = 0, scale = 0, integer = 0; boolean negative = false, decimal = false;
        for (Formatting.Node node : d.nodes) {
            String id = node.id();
            if (node.keyword() == null) { if (at < input.length() && (!node.text().isBlank() || input.charAt(at) == ' ')) at++; continue; }
            if (Set.of("9", "0", "DEC", "D").contains(id)) {
                if (at < input.length() && input.charAt(at) == ' ') at++;
                if (at < input.length() && (input.charAt(at) == '-' || input.charAt(at) == '+' || input.charAt(at) == '<')) { negative |= input.charAt(at) != '+'; at++; }
                if (at < input.length() && input.charAt(at) == '.' && !decimal) { decimal = true; at++; if (id.equals("DEC") || id.equals("D")) continue; }
                if (at < input.length() && digit(input.charAt(at))) {
                    if (!decimal || scale < d.post) { digits.append(input.charAt(at++)); if (decimal) scale++; else integer++; }
                }
            } else if (Set.of("S", "MI", "PL", "SG", "PR").contains(id)) {
                if (at < input.length() && (input.charAt(at) == '-' || input.charAt(at) == '+' || input.charAt(at) == '<' || input.charAt(at) == '>')) { negative |= input.charAt(at) == '-' || input.charAt(at) == '<' || input.charAt(at) == '>'; at++; }
            } else if (id.equals("COMMA") || id.equals("G")) { if (at < input.length() && (input.charAt(at) == ',' || input.charAt(at) == ' ')) at++; }
            else if (id.equals("L")) { if (at < input.length() && !digit(input.charAt(at)) && input.charAt(at) != '-' && input.charAt(at) != '+') at++; }
            else if (id.equals("TH") || id.equals("th")) at = Math.min(input.length(), at + 2);
        }
        if (digits.isEmpty()) throw invalid(); if (integer > d.pre) throw SqlException.numericOverflow();
        Decimal value = Decimal.parse((negative ? "-" : "") + digits + "e-" + scale);
        if (d.multiplier > 0) {
            Decimal factor = Decimal.parse("1e-" + d.multiplier).rescale(16 + d.multiplier);
            ScalarPlan.decimalWork("*", value, factor, meter); value = value.multiply(factor);
        }
        return value;
    }
    private static String roman(int number) {
        if (number < 1 || number > 3999) return "###############";
        StringBuilder result = new StringBuilder("M".repeat(number / 1000)); number %= 1000;
        if (number / 100 != 0) result.append(FormattingCatalog.RN_HUNDREDS.get(number / 100 - 1)); number %= 100;
        if (number / 10 != 0) result.append(FormattingCatalog.RN_TENS.get(number / 10 - 1)); number %= 10;
        if (number != 0) result.append(FormattingCatalog.RN_UNITS.get(number - 1)); return result.toString();
    }
    private static SqlException syntax() { return new SqlException("42601", "invalid numeric format specification"); }
    private static boolean digit(char value) { return value >= '0' && value <= '9'; }
    private static SqlException invalid() { return new SqlException("22P02", "invalid input syntax for numeric formatting"); }
}
