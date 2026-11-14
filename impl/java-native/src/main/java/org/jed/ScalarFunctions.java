package org.jed;

import java.util.Arrays;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.Set;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.HexFormat;

/** Catalog-resolved primitive scalar kernels. Unknown kernels remain explicit gaps. */
final class ScalarFunctions {
    private ScalarFunctions() {}
    private static final Set<String> FLOAT_MATH = Set.of("sqrt", "exp", "ln", "log", "log10", "pow", "power", "sin", "cos", "tan", "cbrt", "pi", "radians", "degrees", "asin", "acos", "atan", "atan2", "cot", "sinh", "cosh", "tanh", "asinh", "acosh", "atanh");
    private static final Set<String> SUPPORTED = Set.of(
        "make_interval", "make_timestamp", "make_timestamptz", "date_part", "age", "justify_hours", "justify_days", "justify_interval", "overlay", "quote_literal", "quote_ident", "quote_nullable", "make_date", "uuid_extract_version", "uuid_extract_timestamp", "abs", "round", "trunc", "ceil", "ceiling", "floor", "sign", "scale", "min_scale", "trim_scale", "factorial",
        "length", "char_length", "character_length", "octet_length", "bit_length", "substr", "substring",
        "left", "right", "reverse", "strpos", "starts_with", "ascii", "chr", "btrim", "ltrim", "rtrim", "split_part",
        "md5", "sha224", "sha256", "sha384", "sha512", "get_byte", "set_byte", "get_bit", "set_bit", "to_hex", "decode", "encode",
        "repeat", "lpad", "rpad", "replace", "lower", "upper", "initcap", "translate", "position");

    static ScalarPlan.Expression resolve(String name, List<ScalarPlan.Expression> args) {
        return resolve(name, args, java.util.Collections.nCopies(args.size(), "value"));
    }

    /** Origins distinguish untyped literal adaptation from conversion of a typed expression. */
    static ScalarPlan.Expression resolve(String name, List<ScalarPlan.Expression> args, List<String> origins) {
        ScalarPlan.Expression temporal = TemporalFunctions.resolve(name, args);
        if (temporal != null) return temporal;
        ScalarPlan.Expression pair = NumericPairs.resolve(name, args, origins);
        if (pair != null) return pair;
        if (!SUPPORTED.contains(name) && !FLOAT_MATH.contains(name)) return null;
        List<Candidate> candidates = candidates(name, args, origins, false);
        boolean fallback = candidates.isEmpty();
        if (fallback) candidates = candidates(name, args, origins, true);
        if (candidates.isEmpty()) throw new SqlException("42883", "function does not exist for argument types: " + name);
        for (int i = 0; candidates.size() > 1 && i < args.size(); i++) {
            if (!origins.get(i).equals("unknown") && !(fallback && origins.get(i).equals("string_literal"))) continue;
            String selected = null;
            boolean disagreement = false;
            for (Candidate candidate : candidates) {
                String family = candidate.fact.families().get(i);
                if (family.equals("text")) { selected = "text"; disagreement = false; break; }
                if (selected != null && !selected.equals(family)) disagreement = true;
                selected = family;
            }
            if (disagreement) throw new SqlException("42725", "function is ambiguous: " + name);
            List<Candidate> retained = new ArrayList<>();
            for (Candidate candidate : candidates) if (candidate.fact.families().get(i).equals(selected)) retained.add(candidate);
            candidates = retained;
        }
        if (candidates.size() != 1) throw new SqlException("42725", "function is ambiguous: " + name);
        Candidate selected = candidates.getFirst();
        FunctionCatalog.Fact match = selected.fact;
        if (name.equals("substring") && match.families().size() > 1 && match.families().get(1).equals("text")) return null;
        List<ScalarPlan.Expression> converted = new ArrayList<>();
        for (int i = 0; i < args.size(); i++) {
            ScalarPlan.Expression argument = args.get(i);
            SqlType target = selected.targets.get(i);
            // Decimal kernels accept integer operands directly, without a charged cast node.
            if (target == SqlType.DECIMAL && argument.type().isInteger()) target = argument.type();
            if (target != null && !target.equals(argument.type())) {
                SqlType destination = target;
                boolean widenFloat = argument.type() == SqlType.F32 && target == SqlType.F64;
                converted.add(new ScalarPlan.Expression(target, meter -> {
                    if (widenFloat) meter.operation();
                    return SqlCasts.cast(argument.run(meter), destination);
                }, argument.constant()));
            } else converted.add(argument);
        }
        SqlType result = match.result().equals("promoted") ? converted.getFirst().type() : SqlType.named(match.result());
        return new ScalarPlan.Expression(result, meter -> {
            meter.operation();
            SqlValue[] values = new SqlValue[converted.size()];
            for (int i = 0; i < values.length; i++) {
                values[i] = converted.get(i).run(meter);
                if (values[i].isNull() && !name.equals("quote_nullable")) return SqlValue.nullValue(result);
            }
            SqlValue value = evaluate(name, values, result, meter);
            if (Set.of("encode", "overlay", "quote_literal", "quote_ident", "quote_nullable").contains(name) && !args.stream().allMatch(ScalarPlan.Expression::constant)) {
                meter.scalarInput(value.type().kind() == SqlType.Kind.BYTEA ? value.asBytes().length : SqlText.encodedLength(value.asText()));
            }
            return value;
        });
    }

    static List<SqlType> parameterHints(String name, List<ScalarPlan.Expression> args, List<String> origins) {
        if (!origins.contains("unknown")) return null;
        List<Candidate> options = candidates(name, args, origins, false);
        if (options.isEmpty()) {
            if (FunctionCatalog.FACTS.stream().anyMatch(fact -> fact.name().equals(name) && fact.arity() == args.size() && fact.families().stream().noneMatch(family -> family.startsWith("any")))) throw new SqlException("42883", "function does not exist for argument types: " + name);
            return null;
        }
        for (int i = 0; options.size() > 1 && i < args.size(); i++) {
            if (!origins.get(i).equals("unknown")) continue;
            String selected = null; boolean differs = false;
            for (Candidate option : options) {
                String family = option.fact.families().get(i);
                if (family.equals("text")) { selected = family; differs = false; break; }
                if (selected != null && !selected.equals(family)) differs = true;
                selected = family;
            }
            if (differs) throw new SqlException("42725", "function is ambiguous: " + name);
            String family = selected; int slot = i;
            options = options.stream().filter(option -> option.fact.families().get(slot).equals(family)).toList();
        }
        if (options.size() != 1) throw new SqlException("42725", "function is ambiguous: " + name);
        return options.getFirst().targets;
    }
    private record Candidate(FunctionCatalog.Fact fact, List<SqlType> targets) {}
    private static List<Candidate> candidates(String name, List<ScalarPlan.Expression> args, List<String> origins, boolean stringFallback) {
        List<Candidate> result = new ArrayList<>();
        int best = Integer.MAX_VALUE;
        for (FunctionCatalog.Fact fact : FunctionCatalog.FACTS) {
            if (!fact.name().equals(name) || fact.arity() != args.size() || fact.families().stream().anyMatch(family -> family.startsWith("any") && !family.equals("any"))) continue;
            int score = 0;
            List<SqlType> targets = new ArrayList<>();
            for (int i = 0; i < args.size(); i++) {
                SqlType type = args.get(i).type();
                String origin = origins.get(i);
                String slot = fact.families().get(i);
                boolean unknown = origin.equals("unknown");
                if (stringFallback && origin.equals("string_literal")) {
                    if (!Set.of("text", "any", "bytea", "uuid", "date", "timestamp", "timestamptz", "interval", "json", "jsonb", "jsonpath").contains(slot)) { score = Integer.MAX_VALUE; break; }
                    unknown = true;
                }
                if (unknown || slot.equals("any") || family(type).equals(slot)) {
                    SqlType target = unknown ? familyType(slot) : type;
                    if (slot.equals("float") && fact.result().equals("f64")) target = SqlType.F64;
                    targets.add(target); continue;
                }
                int cost = Integer.MAX_VALUE;
                SqlType target = null;
                for (ArgCoercionCatalog.Fact conversion : ArgCoercionCatalog.FACTS) {
                    if (!conversion.from().equals(type.kind().name().toLowerCase(Locale.ROOT)) && !conversion.from().equals(origin)) continue;
                    SqlType destination = SqlType.named(conversion.to());
                    if (family(destination).equals(slot) && conversion.cost() < cost) { cost = conversion.cost(); target = destination; }
                }
                if (target == null) { score = Integer.MAX_VALUE; break; }
                score += cost; targets.add(target);
            }
            if (score < best) { best = score; result.clear(); }
            if (score == best && score != Integer.MAX_VALUE) result.add(new Candidate(fact, targets));
        }
        return result;
    }

    private static SqlType familyType(String family) {
        return switch (family) {
            case "integer" -> SqlType.I64;
            case "float" -> SqlType.F64;
            case "any" -> null;
            default -> SqlType.named(family);
        };
    }

    private static String family(SqlType type) {
        if (type.isInteger()) return "integer";
        if (type.isFloat()) return "float";
        return type.kind().name().toLowerCase(Locale.ROOT);
    }

    private static SqlValue evaluate(String name, SqlValue[] a, SqlType result, CostMeter meter) {
        if (FLOAT_MATH.contains(name) && result == SqlType.DECIMAL) {
            Decimal first = decimal(a[0]);
            return SqlValue.ofDecimal(switch (name) {
                case "sqrt" -> DecimalFunctions.sqrt(first, meter);
                case "exp" -> DecimalFunctions.exp(first, meter);
                case "ln" -> DecimalFunctions.ln(first, meter);
                case "log10" -> DecimalFunctions.log10(first, meter);
                case "log" -> a.length == 1 ? DecimalFunctions.log10(first, meter) : DecimalFunctions.log(first, decimal(a[1]), meter);
                case "pow", "power" -> DecimalFunctions.power(first, decimal(a[1]), meter);
                default -> throw new SqlException("0A000", "numeric function is not implemented: " + name);
            });
        }
        if (FLOAT_MATH.contains(name)) return SqlValue.ofFloat(SqlType.F64, FloatFunctions.evaluate(name, a));
        if (Set.of("abs", "round", "trunc", "ceil", "ceiling", "floor", "sign", "scale", "min_scale", "trim_scale", "factorial").contains(name)) {
            return numeric(name, a, result, meter);
        }
        SqlValue first = a[0];
        if (name.equals("quote_nullable") && first.isNull()) return SqlValue.ofText("NULL");
        if (name.equals("quote_literal") || name.equals("quote_nullable")) return SqlValue.ofText(TextQuotes.literal(first.asText()));
        if (name.equals("quote_ident")) return SqlValue.ofText(TextQuotes.identifier(first.asText()));
        if (name.equals("make_date")) return SqlValue.ofDate(Temporal.makeDate(a[0].asInteger(), a[1].asInteger(), a[2].asInteger()));
        if (name.equals("make_interval")) return SqlValue.ofInterval(TemporalFunctions.makeInterval(a));
        if (name.equals("make_timestamp") || name.equals("make_timestamptz")) return TemporalFunctions.makeTimestamp(a, name.equals("make_timestamptz"), meter);
        if (name.equals("date_part")) return TemporalFunctions.extract(a[0].asText(), a[1], true, meter);
        if (name.equals("age")) {
            if (a.length == 2) return SqlValue.ofInterval(TemporalFunctions.age(a[0], a[1], meter));
            meter.charge(Costs.TIMEZONE); meter.guard();
            long local = meter.zone().toLocal(meter.statementTime());
            long midnight = Temporal.assemble(Math.floorDiv(local, Temporal.DAY_MICROS), 0);
            if (a[0].type().kind() == SqlType.Kind.TIMESTAMPTZ) { meter.charge(Costs.TIMEZONE); meter.guard(); midnight = meter.zone().toInstant(midnight); }
            return SqlValue.ofInterval(TemporalFunctions.age(SqlValue.ofTimestamp(a[0].type(), midnight), a[0], meter));
        }
        if (name.equals("justify_hours")) return SqlValue.ofInterval(a[0].asInterval().justifyHours());
        if (name.equals("justify_days")) return SqlValue.ofInterval(a[0].asInterval().justifyDays());
        if (name.equals("justify_interval")) return SqlValue.ofInterval(a[0].asInterval().justify());
        if (name.equals("uuid_extract_timestamp")) {
            Long micros = first.asUuid().timestampMicros();
            return micros == null ? SqlValue.nullValue(SqlType.TIMESTAMPTZ) : SqlValue.ofTimestamp(SqlType.TIMESTAMPTZ, micros);
        }
        if (name.equals("uuid_extract_version")) return first.asUuid().rfcVariant() ? SqlValue.ofInteger(SqlType.I16, first.asUuid().version()) : SqlValue.nullValue(SqlType.I16);
        if (name.equals("repeat")) return SqlValue.ofText(TextAmplifiers.repeat(first.asText(), a[1].asInteger(), meter));
        if (name.equals("lpad") || name.equals("rpad")) return SqlValue.ofText(TextAmplifiers.pad(first.asText(), a[1].asInteger(), a.length == 3 ? a[2].asText() : " ", name.equals("lpad"), meter));
        if (name.equals("replace")) return SqlValue.ofText(TextAmplifiers.replace(first.asText(), a[1].asText(), a[2].asText(), meter));
        if (name.equals("encode")) return SqlValue.ofText(BinaryText.encode(first.asBytes(), a[1].asText()));
        if (name.equals("decode")) return SqlValue.ofBytes(BinaryText.decode(first.asText(), a[1].asText()));
        if (name.equals("md5") || name.startsWith("sha")) {
            byte[] input = first.type().kind() == SqlType.Kind.TEXT ? SqlText.encode(first.asText()) : first.asBytes();
            String algorithm = name.equals("md5") ? "MD5" : "SHA-" + name.substring(3);
            try {
                byte[] digest = MessageDigest.getInstance(algorithm).digest(input);
                return name.equals("md5") ? SqlValue.ofText(HexFormat.of().formatHex(digest)) : SqlValue.ofBytes(digest);
            } catch (NoSuchAlgorithmException failure) {
                throw new IllegalStateException("required JDK crypto provider is missing: " + algorithm, failure);
            }
        }
        if (name.equals("to_hex")) return SqlValue.ofText(Long.toHexString(first.asInteger()));
        if (name.equals("get_byte") || name.equals("set_byte") || name.equals("get_bit") || name.equals("set_bit")) {
            byte[] data = first.asBytes();
            long position = a[1].asInteger();
            boolean bit = name.endsWith("bit");
            if (position < 0 || position >= (long) data.length * (bit ? 8 : 1)) {
                throw new SqlException("2202E", "index out of valid range");
            }
            int index = (int) (bit ? position / 8 : position);
            int shift = (int) (position % 8);
            if (name.startsWith("get")) return integer(bit ? (data[index] >>> shift) & 1 : data[index] & 255);
            long replacement = a[2].asInteger();
            if (bit) {
                if (replacement != 0 && replacement != 1) throw new SqlException("22023", "new bit must be 0 or 1");
                data[index] = (byte) ((data[index] & ~(1 << shift)) | (replacement << shift));
            } else data[index] = (byte) replacement;
            return SqlValue.ofBytes(data);
        }
        boolean bytes = first.type().kind() == SqlType.Kind.BYTEA;
        String text = bytes ? null : name.equals("chr") ? null : first.asText();
        switch (name) {
            case "lower", "upper", "initcap": {
                if (!name.equals("initcap")) return SqlValue.ofText(meter.environment().caseText(text, name.equals("upper")));
                StringBuilder output = new StringBuilder(text.length());
                boolean word = false;
                for (int i = 0; i < text.length(); i++) {
                    char c = text.charAt(i);
                    boolean letter = c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z';
                    boolean alphanumeric = letter || c >= '0' && c <= '9';
                    boolean upper = name.equals("upper") || name.equals("initcap") && !word;
                    if (upper && c >= 'a' && c <= 'z') c -= 32;
                    else if (!upper && c >= 'A' && c <= 'Z') c += 32;
                    output.append(c);
                    word = alphanumeric;
                }
                return SqlValue.ofText(output.toString());
            }
            case "translate": {
                String from = a[1].asText(), to = a[2].asText();
                int[] replacements = to.codePoints().toArray();
                StringBuilder output = new StringBuilder(text.length());
                for (int i = 0; i < text.length();) {
                    int c = text.codePointAt(i); i += Character.charCount(c);
                    int offset = from.indexOf(c);
                    if (offset < 0) output.appendCodePoint(c);
                    else {
                        int index = from.codePointCount(0, offset);
                        if (index < replacements.length) output.appendCodePoint(replacements[index]);
                    }
                }
                return SqlValue.ofText(output.toString());
            }
            case "length", "char_length", "character_length":
                return integer(bytes ? first.asBytes().length : SqlText.length(text));
            case "octet_length", "bit_length": {
                long length = bytes ? first.asBytes().length : SqlText.encode(text).length;
                return integer(length * (name.equals("bit_length") ? 8L : 1));
            }
            case "overlay": {
                long start = a[2].asInteger();
                if (start <= 0) throw new SqlException("22011", "negative substring length not allowed");
                long count = a.length == 4 ? a[3].asInteger() : bytes ? a[1].asBytes().length : SqlText.length(a[1].asText());
                long end;
                try { end = Math.addExact(start, count); }
                catch (ArithmeticException failure) { throw SqlException.numericOverflow(); }
                if (end < Integer.MIN_VALUE || end > Integer.MAX_VALUE) throw SqlException.numericOverflow();
                int length = bytes ? first.asBytes().length : SqlText.length(text);
                int head = (int) Math.min(length, start - 1);
                int tail = (int) Math.min(length, Math.max(end, 1) - 1);
                if (bytes) {
                    byte[] source = first.asBytes(), replacement = a[1].asBytes();
                    byte[] output = new byte[Math.addExact(head + length - tail, replacement.length)];
                    System.arraycopy(source, 0, output, 0, head);
                    System.arraycopy(replacement, 0, output, head, replacement.length);
                    System.arraycopy(source, tail, output, head + replacement.length, length - tail);
                    return SqlValue.ofBytes(output);
                }
                return SqlValue.ofText(slice(text, 0, head) + a[1].asText() + slice(text, tail, length));
            }
            case "substr", "substring": {
                int length = bytes ? first.asBytes().length : SqlText.length(text);
                long start = a[1].asInteger();
                long count = a.length == 3 ? a[2].asInteger() : Long.MAX_VALUE;
                if (count < 0) throw new SqlException("22011", "negative substring length not allowed");
                long end = a.length == 2 || start > Long.MAX_VALUE - count ? Long.MAX_VALUE : start + count;
                int beginIndex = (int) Math.min(length, Math.max(1, start) - 1);
                int endIndex = (int) Math.min(length, Math.max(1, end) - 1);
                if (bytes) return SqlValue.ofBytes(Arrays.copyOfRange(first.asBytes(), beginIndex, Math.max(beginIndex, endIndex)));
                return SqlValue.ofText(slice(text, beginIndex, Math.max(beginIndex, endIndex)));
            }
            case "left", "right": {
                int length = SqlText.length(text);
                long requested = a[1].asInteger();
                long count = requested >= 0 ? Math.min(requested, length) : Math.max(0, length + requested);
                int start = name.equals("left") ? 0 : length - (int) count;
                return SqlValue.ofText(slice(text, start, start + (int) count));
            }
            case "reverse": return SqlValue.ofText(new StringBuilder(text).reverse().toString());
            case "strpos", "position": {
                if (bytes) {
                    byte[] input = first.asBytes(), needle = a[1].asBytes();
                    outer: for (int i = 0; i <= input.length - needle.length; i++) {
                        for (int j = 0; j < needle.length; j++) if (input[i + j] != needle[j]) continue outer;
                        return integer(i + 1);
                    }
                    return integer(0);
                }
                int position = text.indexOf(a[1].asText());
                return integer(position < 0 ? 0 : text.codePointCount(0, position) + 1);
            }
            case "starts_with": return SqlValue.ofBoolean(text.startsWith(a[1].asText()));
            case "ascii": return integer(text.isEmpty() ? 0 : text.codePointAt(0));
            case "chr": {
                long code = first.asInteger();
                if (code < 0) throw new SqlException("22023", "character number must be positive");
                if (code == 0) throw new SqlException("54000", "null character not permitted");
                if (code < 0 || code > 0x10ffff || code >= 0xd800 && code <= 0xdfff) {
                    throw new SqlException("54000", "requested character too large for encoding");
                }
                return SqlValue.ofText(new String(Character.toChars((int) code)));
            }
            case "btrim", "ltrim", "rtrim": {
                if (bytes) {
                    byte[] input = first.asBytes(), remove = a[1].asBytes();
                    int lo = 0, hi = input.length;
                    while (!name.equals("rtrim") && lo < hi && contains(remove, input[lo])) lo++;
                    while (!name.equals("ltrim") && hi > lo && contains(remove, input[hi - 1])) hi--;
                    return SqlValue.ofBytes(Arrays.copyOfRange(input, lo, hi));
                }
                String remove = a.length == 1 ? " " : a[1].asText();
                int lo = 0, hi = text.length();
                while (!name.equals("rtrim") && lo < hi && remove.indexOf(text.codePointAt(lo)) >= 0) lo += Character.charCount(text.codePointAt(lo));
                while (!name.equals("ltrim") && hi > lo && remove.indexOf(text.codePointBefore(hi)) >= 0) hi -= Character.charCount(text.codePointBefore(hi));
                return SqlValue.ofText(text.substring(lo, hi));
            }
            case "split_part": return SqlValue.ofText(splitPart(text, a[1].asText(), a[2].asInteger()));
            default: throw new IllegalStateException("missing scalar kernel " + name);
        }
    }

    private static boolean contains(byte[] values, byte value) {
        for (byte candidate : values) if (candidate == value) return true;
        return false;
    }
    private static String slice(String text, int start, int end) {
        return text.substring(text.offsetByCodePoints(0, start), text.offsetByCodePoints(0, end));
    }
    private static SqlValue integer(long number) { return SqlValue.ofInteger(SqlType.I32, number); }

    private static String splitPart(String text, String separator, long field) {
        if (field == 0) throw new SqlException("22023", "field position must not be zero");
        if (separator.isEmpty()) return field == 1 || field == -1 ? text : "";
        if (field < 0) {
            long count = 1;
            for (int i = 0; (i = text.indexOf(separator, i)) >= 0; i += separator.length()) count++;
            field += count + 1;
        }
        if (field <= 0) return "";
        int start = 0;
        for (long current = 1; ; current++) {
            int end = text.indexOf(separator, start);
            if (current == field) return end < 0 ? text.substring(start) : text.substring(start, end);
            if (end < 0) return "";
            start = end + separator.length();
        }
    }

    private static Decimal decimal(SqlValue value) {
        return value.type().isInteger() ? Decimal.of(value.asInteger()) : value.asDecimal();
    }

    private static SqlValue numeric(String name, SqlValue[] a, SqlType result, CostMeter meter) {
        SqlValue first = a[0];
        if (name.equals("abs") && first.type().isInteger()) {
            long number = first.asInteger();
            if (number == Long.MIN_VALUE) throw SqlException.numericOverflow();
            return SqlValue.ofInteger(result, Math.abs(number));
        }
        if (first.type().isFloat()) {
            double number = first.asFloat();
            double value = switch (name) {
                case "abs" -> Math.abs(number);
                case "ceil", "ceiling" -> Math.ceil(number);
                case "floor" -> Math.floor(number);
                case "trunc" -> number < 0 ? Math.ceil(number) : Math.floor(number);
                case "sign" -> number > 0 ? 1 : number < 0 ? -1 : 0;
                case "round" -> roundFloat(number, a.length == 1 ? 0 : a[1].asInteger());
                default -> throw new IllegalStateException("unsupported float scalar");
            };
            return SqlValue.ofFloat(result, value);
        }
        Decimal value = decimal(first);
        return switch (name) {
            case "abs" -> SqlValue.ofDecimal(value.abs());
            case "round" -> SqlValue.ofDecimal(value.roundPlaces(a.length == 1 ? 0 : a[1].asInteger()));
            case "trunc" -> SqlValue.ofDecimal(value.truncatePlaces(a.length == 1 ? 0 : a[1].asInteger()));
            case "ceil", "ceiling" -> SqlValue.ofDecimal(value.ceil());
            case "floor" -> SqlValue.ofDecimal(value.floor());
            case "sign" -> SqlValue.ofDecimal(Decimal.of(value.isZero() ? 0 : value.negative() ? -1 : 1));
            case "scale" -> integer(value.scale());
            case "min_scale" -> integer(value.minimumScale());
            case "trim_scale" -> SqlValue.ofDecimal(value.rescale(value.minimumScale()));
            case "factorial" -> {
                long n = first.asInteger();
                if (n < 0) throw new SqlException("22003", "factorial of a negative number is undefined");
                Decimal accumulated = Decimal.of(1);
                for (long k = 2; k <= n; k++) {
                    Decimal next = Decimal.of(k);
                    long work = accumulated.groupsAtScale(accumulated.scale()) * next.groupsAtScale(next.scale());
                    meter.charge((work - 1) * Costs.DECIMAL_WORK); meter.guard();
                    accumulated = accumulated.multiply(next);
                }
                yield SqlValue.ofDecimal(accumulated);
            }
            default -> throw new IllegalStateException("unsupported numeric scalar");
        };
    }

    private static double roundFloat(double value, long places) {
        if (!Double.isFinite(value) || value == 0) return value;
        if (places >= 324) return value;
        if (places < -323) return Math.copySign(0, value);
        double factor = Math.pow(10, places);
        if (factor == 0) return Math.copySign(0, value);
        if (Double.isInfinite(factor)) {
            Decimal rounded = Decimal.fromDoubleExact(value).rescale((int) places);
            return Math.copySign(Double.parseDouble(rounded.toString()), value);
        }
        double shifted = value * factor;
        if (!Double.isFinite(shifted)) return value;
        double magnitude = Math.abs(shifted);
        double integral = Math.floor(magnitude);
        double rounded = Math.copySign(integral + (magnitude - integral >= 0.5 ? 1 : 0), shifted);
        return rounded / factor;
    }
}
