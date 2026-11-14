package org.jed;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Iterator;
import java.util.List;
import java.util.Set;
import java.util.function.Consumer;

/** Depth-first path execution; accessor chains use an explicit continuation stack. */
final class JsonPathEval {
    record Result(List<JsonNode> items, boolean failed) {}
    private record Item(JsonNode value, int last, boolean suppress, Base base) {
        Item with(JsonNode node) { return new Item(node, last, suppress, base); }
    }
    private record Frame(int step, Iterator<Item> items, boolean emit, boolean unwrapped) {}
    private final JsonPath program;
    private final JsonNode document, variables;
    private final CostMeter meter;
    private final boolean exists;
    private final Base documentBase, variablesBase;
    private long nextBase;
    private final java.util.IdentityHashMap<JsonPath.Node, RegexProgram> regexPrograms = new java.util.IdentityHashMap<>();
    private final java.util.Map<String, Base> passingBases = new java.util.HashMap<>();
    private JsonPathEval(JsonPath program, JsonNode document, JsonNode variables, CostMeter meter, boolean exists, long nextBase) {
        this.program = program; this.document = document; this.variables = variables; this.meter = meter; this.exists = exists; this.documentBase = new Base(document, 0); this.variablesBase = new Base(variables, 1); this.nextBase = nextBase;
    }
    static Result evaluate(JsonPath path, JsonNode document, JsonNode variables, boolean silent, boolean exists, CostMeter meter) {
        return evaluate(path, document, variables, silent, exists, meter, 2);
    }
    static Result evaluate(JsonPath path, JsonNode document, JsonNode variables, boolean silent, boolean exists, CostMeter meter, long nextBase) {
        return evaluate(path, document, variables, silent, exists, meter, nextBase, List.of());
    }
    static Result evaluate(JsonPath path, JsonNode document, JsonNode variables, boolean silent, boolean exists, CostMeter meter, long nextBase, List<String> passingNames) {
        return evaluate(path, path.toString(), document, variables, silent, exists, meter, nextBase, passingNames, true);
    }
    static Result evaluateRows(JsonPath path, JsonNode document, JsonNode variables, boolean silent, CostMeter meter) {
        return evaluate(path, path.toString(), document, variables, silent, false, meter, 2, List.of(), false);
    }
    static Result evaluateTable(JsonPath path, String source, JsonNode document, JsonNode variables, boolean exists, CostMeter meter, long nextBase, List<String> passingNames) {
        return evaluate(path, source, document, variables, false, exists, meter, nextBase, passingNames, false);
    }
    private static Result evaluate(JsonPath path, String source, JsonNode document, JsonNode variables, boolean silent, boolean exists, CostMeter meter, long nextBase, List<String> passingNames, boolean finalItemCharge) {
        if (variables.kind() != JsonNode.Kind.OBJECT) throw new SqlException("22023", "jsonpath variables must be an object");
        meter.charge(Costs.JSONPATH_COMPILE * SqlText.encodedLength(source)); meter.guard();
        JsonPath compiled = JsonPath.compile(path.toString());
        JsonPathEval evaluator = new JsonPathEval(compiled, document, variables, meter, exists, nextBase);
        evaluator.compileRegexes(compiled.root());
        for (int i = 0; i < passingNames.size(); i++) evaluator.passingBases.putIfAbsent(passingNames.get(i), new Base(variables.get(passingNames.get(i)), i + 1));
        ArrayList<JsonNode> output = new ArrayList<>();
        try {
            evaluator.run(compiled.root(), new Item(document, -1, false, evaluator.documentBase), item -> {
                if (finalItemCharge) meter.operation(); output.add(item.value);
                if (exists && !compiled.strict()) throw Stop.INSTANCE;
            });
        } catch (Stop done) { }
        catch (SqlException failure) { if (silent && suppressible(failure)) return new Result(List.copyOf(output), true); throw failure; }
        return new Result(List.copyOf(output), false);
    }
    private void compileRegexes(JsonPath.Node root) {
        var pending = new ArrayDeque<JsonPath.Node>(); pending.push(root);
        while (!pending.isEmpty()) { var node = pending.pop(); if (node.kind().equals("regex")) { RegexProgram regex = RegexProgram.compile(node.text(), node.args().get(1).text(), true); regex.chargeCompile(meter); regexPrograms.put(node, regex); } for (int i = node.args().size() - 1; i >= 0; i--) pending.push(node.args().get(i)); }
    }
    private static final class Stop extends RuntimeException {
        private static final long serialVersionUID = 1L;
        static final Stop INSTANCE = new Stop();
        private Stop() { super(null, null, false, false); }
    }
    private void step() { meter.charge(Costs.JSONPATH_STEP); meter.guard(); }
    private static boolean suppressible(SqlException error) { return error.sqlState().startsWith("22"); }
    private void run(JsonPath.Node node, Item context, Consumer<Item> out) {
        if (accessor(node)) {
            ArrayList<JsonPath.Node> steps = new ArrayList<>(); JsonPath.Node base = node;
            while (accessor(base)) { steps.add(base); base = base.args().getFirst(); }
            Collections.reverse(steps); run(base, context, seed -> chain(seed, steps, context, out)); return;
        }
        if (node.predicate()) { Boolean result = predicate(node, context); out.accept(context.with(result == null ? JsonNode.NULL : JsonNode.bool(result))); return; }
        switch (node.kind()) {
            case "root" -> out.accept(new Item(document, context.last, context.suppress, documentBase));
            case "current" -> out.accept(context);
            case "last" -> out.accept(context.with(JsonNode.number(Decimal.of(context.last))));
            case "number" -> out.accept(context.with(JsonNode.number(Decimal.parse(node.text()))));
            case "string" -> out.accept(context.with(JsonNode.string(node.text())));
            case "literal" -> out.accept(context.with(node.text().equals("null") ? JsonNode.NULL : JsonNode.bool(node.text().equals("true"))));
            case "variable" -> { JsonNode value = variables.get(node.text()); if (value == null) throw new SqlException("42704", "could not find jsonpath variable " + node.text()); out.accept(new Item(value, context.last, context.suppress, passingBases.getOrDefault(node.text(), variablesBase))); }
            case "binary" -> {
                List<Item> left = operand(node.args().get(0), context, true), right = operand(node.args().get(1), context, true);
                Decimal a = numeric(left, "22038"), b = numeric(right, "22038"); step();
                if ((node.text().equals("/") || node.text().equals("%")) && b.isZero()) throw new SqlException("22012", "division by zero");
                ScalarPlan.decimalWork(node.text(), a, b, meter);
                Decimal value = switch (node.text()) { case "+" -> a.add(b); case "-" -> a.subtract(b); case "*" -> a.multiply(b); case "/" -> a.divide(b); case "%" -> a.modulo(b); default -> throw new AssertionError(); };
                step(); out.accept(context.with(JsonNode.number(value)));
            }
            case "unary" -> {
                run(node.args().getFirst(), context, item -> {
                    List<Item> values = unwrap(item); for (Item value : values) {
                        step();
                        if (value.value.kind() != JsonNode.Kind.NUMBER) { if (exists && !program.strict() && node == program.root()) continue; throw new SqlException("2203B", "unary jsonpath operand is not numeric"); }
                        step(); out.accept(value.with(JsonNode.number(node.text().equals("-") ? value.value.number().negate() : value.value.number())));
                    }
                });
            }
            default -> throw new SqlException("0A000", "unsupported jsonpath expression: " + node.kind());
        }
    }
    private static boolean accessor(JsonPath.Node node) { return Set.of("member", "members", "elements", "index", "filter", "method", "any").contains(node.kind()); }
    private void chain(Item seed, List<JsonPath.Node> steps, Item context, Consumer<Item> out) {
        ArrayDeque<Frame> stack = new ArrayDeque<>(); stack.push(new Frame(0, List.of(seed).iterator(), false, false));
        while (!stack.isEmpty()) {
            Frame frame = stack.pop(); if (!frame.items.hasNext()) continue;
            Item item = frame.items.next(); stack.push(frame);
            if (frame.emit) { step(); if (item.base.number < 0) item.base.number = nextBase++; }
            if (frame.step == steps.size()) { out.accept(item); continue; }
            JsonPath.Node operation = steps.get(frame.step);
            boolean unwrap = !program.strict() && item.value.kind() == JsonNode.Kind.ARRAY && !frame.unwrapped && (Set.of("member", "members", "filter").contains(operation.kind()) || operation.kind().equals("method") && !Set.of("type", "size").contains(operation.text()));
            if (unwrap) { stack.push(new Frame(frame.step, children(item).iterator(), false, true)); continue; }
            step();
            Iterable<Item> results = apply(operation, item, context);
            stack.push(new Frame(frame.step + 1, results.iterator(), true, false));
        }
    }
    private Iterable<Item> apply(JsonPath.Node op, Item item, Item context) {
        JsonNode value = item.value;
        switch (op.kind()) {
            case "member" -> {
                if (value.kind() != JsonNode.Kind.OBJECT) return navigation(item, "2203A");
                for (JsonNode.Member member : value.members()) { step(); if (member.key().equals(op.text())) return List.of(item.with(member.value())); }
                return navigation(item, "2203A");
            }
            case "members" -> { if (value.kind() != JsonNode.Kind.OBJECT) return navigation(item, "2203C"); return children(item); }
            case "elements" -> {
                if (value.kind() == JsonNode.Kind.ARRAY) return children(item);
                if (!program.strict()) return List.of(item); return navigation(item, "22039");
            }
            case "index" -> {
                List<JsonNode> array;
                if (value.kind() == JsonNode.Kind.ARRAY) array = value.elements();
                else if (!program.strict()) array = List.of(value); else return navigation(item, "22039");
                ArrayList<Item> output = new ArrayList<>(); Item indexContext = new Item(context.value, array.size() - 1, item.suppress, context.base);
                for (int i = 1; i < op.args().size(); i++) {
                    JsonPath.Node index = op.args().get(i); boolean slice = index.kind().equals("slice");
                    int lo = subscript(slice ? index.args().getFirst() : index, indexContext), hi = slice ? subscript(index.args().get(1), indexContext) : lo;
                    if (lo < 0 || hi >= array.size() || lo > hi) { if (program.strict() && !item.suppress) throw new SqlException("22033", "jsonpath array subscript out of bounds"); }
                    for (long j = Math.max(0, lo); j <= Math.min(hi, array.size() - 1); j++) output.add(new Item(array.get((int) j), array.size() - 1, item.suppress, item.base));
                }
                return output;
            }
            case "filter" -> { return Boolean.TRUE.equals(predicate(op.args().get(1), item)) ? List.of(item) : List.of(); }
            case "method" -> { return method(op, item); }
            case "any" -> {
                int lo = 0, hi = Integer.MAX_VALUE;
                if (!op.text().isEmpty()) { String[] bounds = op.text().substring(1, op.text().length() - 1).split(" to "); lo = bounds[0].equals("last") ? Integer.MAX_VALUE : Integer.parseInt(bounds[0]); hi = bounds.length == 1 ? lo : bounds[1].equals("last") ? Integer.MAX_VALUE : Integer.parseInt(bounds[1]); }
                return descendants(item, lo, hi);
            }
            default -> throw new AssertionError();
        }
    }
    private Iterable<Item> descendants(Item seed, int lo, int hi) {
        record Level(Iterator<Item> items, int depth) {}
        return () -> new Iterator<>() {
            final ArrayDeque<Level> levels = new ArrayDeque<>();
            Item ready;
            { levels.push(new Level(List.of(seed).iterator(), 0)); }
            @Override public boolean hasNext() {
                if (ready != null) return true;
                while (!levels.isEmpty()) {
                    Level level = levels.peek(); if (!level.items.hasNext()) { levels.pop(); continue; }
                    Item item = level.items.next(); int depth = level.depth;
                    if (depth > 0) step();
                    boolean leaf = item.value.kind() != JsonNode.Kind.ARRAY && item.value.kind() != JsonNode.Kind.OBJECT;
                    if (depth < hi && !leaf) levels.push(new Level(children(item).iterator(), depth + 1));
                    if (depth >= lo && depth <= hi || lo == Integer.MAX_VALUE && hi == Integer.MAX_VALUE && depth > 0 && leaf) { ready = new Item(item.value, item.last, true, item.base); return true; }
                }
                return false;
            }
            @Override public Item next() { if (!hasNext()) throw new java.util.NoSuchElementException(); Item item = ready; ready = null; return item; }
        };
    }
    private List<Item> children(Item item) {
        ArrayList<Item> items = new ArrayList<>();
        if (item.value.kind() == JsonNode.Kind.ARRAY) for (JsonNode child : item.value.elements()) items.add(new Item(child, item.value.elements().size() - 1, item.suppress, item.base));
        else if (item.value.kind() == JsonNode.Kind.OBJECT) for (JsonNode.Member member : item.value.members()) items.add(item.with(member.value()));
        return items;
    }
    private List<Item> navigation(Item item, String state) { if (program.strict() && !item.suppress) throw new SqlException(state, "jsonpath structural navigation failed"); return List.of(); }
    private List<Item> unwrap(Item item) { return !program.strict() && item.value.kind() == JsonNode.Kind.ARRAY ? children(item) : List.of(item); }
    private List<Item> operand(JsonPath.Node node, Item context, boolean unwrap) { ArrayList<Item> items = new ArrayList<>(); run(node, context, item -> { if (unwrap) items.addAll(unwrap(item)); else items.add(item); }); return items; }
    private static Decimal numeric(List<Item> items, String state) { if (items.size() != 1 || items.getFirst().value.kind() != JsonNode.Kind.NUMBER) throw new SqlException(state, "jsonpath operand is not a single number"); return items.getFirst().value.number(); }
    private int subscript(JsonPath.Node expression, Item context) { Decimal number = numeric(operand(expression, context, false), "22033"); try { return Math.toIntExact(number.truncate(0).toLongExact()); } catch (ArithmeticException | SqlException failure) { throw new SqlException("22033", "invalid jsonpath subscript"); } }
    private Boolean predicate(JsonPath.Node node, Item context) {
        if (node.kind().equals("and") || node.kind().equals("or")) {
            Boolean a = predicate(node.args().get(0), context), b = predicate(node.args().get(1), context);
            boolean and = node.kind().equals("and");
            if (Boolean.valueOf(!and).equals(a) || Boolean.valueOf(!and).equals(b)) return !and;
            return a == null || b == null ? null : and;
        }
        if (node.kind().equals("not")) { Boolean value = predicate(node.args().getFirst(), context); return value == null ? null : !value; }
        if (node.kind().equals("unknown")) return predicate(node.args().getFirst(), context) == null;
        if (node.kind().equals("exists")) {
            final boolean[] found = {false};
            try { run(node.args().getFirst(), context, item -> { found[0] = true; if (!program.strict()) throw Stop.INSTANCE; }); }
            catch (Stop done) { }
            catch (SqlException failure) { if (suppressible(failure)) return null; throw failure; }
            return found[0];
        }
        try {
            List<Item> left = operand(node.args().get(0), context, true);
            if (node.kind().equals("regex")) {
                boolean found = false, unknown = false; RegexProgram regex = regexPrograms.get(node);
                for (Item item : left) { step(); if (item.value.kind() != JsonNode.Kind.STRING) { unknown = true; if (program.strict()) return null; } else if (regex.search(item.value.string(), 0, meter) != null) { found = true; if (!program.strict()) return true; } }
                if (found) return true; if (unknown) return null; return false;
            }
            List<Item> right = operand(node.args().get(1), context, !node.kind().equals("starts"));
            boolean truePair = false, unknown = false;
            for (Item a : left) for (Item b : right) {
                step(); Boolean truth = compare(node.kind().equals("starts") ? "starts" : node.text(), a.value, b.value);
                if (truth == null) { unknown = true; if (program.strict()) return null; }
                else if (truth) { truePair = true; if (!program.strict()) return true; }
            }
            if (truePair) return true; if (unknown) return null; return false;
        } catch (SqlException failure) { if (suppressible(failure)) return null; throw failure; }
    }
    private Boolean compare(String operator, JsonNode a, JsonNode b) {
        if (a.kind() == JsonNode.Kind.NULL || b.kind() == JsonNode.Kind.NULL) {
            if (operator.equals("starts")) return null;
            boolean both = a.kind() == b.kind(); return switch (operator) { case "==", "<=", ">=" -> both; case "!=" -> !both; default -> false; };
        }
        if (a.kind() != b.kind() || a.kind() == JsonNode.Kind.ARRAY || a.kind() == JsonNode.Kind.OBJECT) return null;
        int order;
        if (a.kind() == JsonNode.Kind.STRING) {
            long work = Math.min(SqlText.length(a.string()), SqlText.length(b.string())); meter.charge(Math.max(0, work - 1) * Costs.VARLEN_COMPARE); meter.guard();
            if (operator.equals("starts")) return a.string().startsWith(b.string()); order = SqlText.compare(a.string(), b.string());
        } else if (operator.equals("starts")) return null;
        else if (a.kind() == JsonNode.Kind.NUMBER) { ScalarPlan.decimalWork("compare", a.number(), b.number(), meter); order = a.number().compareTo(b.number()); }
        else order = Boolean.compare(a.booleanValue(), b.booleanValue());
        return switch (operator) { case "==" -> order == 0; case "!=" -> order != 0; case "<" -> order < 0; case "<=" -> order <= 0; case ">" -> order > 0; case ">=" -> order >= 0; default -> throw new AssertionError(); };
    }
    private List<Item> method(JsonPath.Node op, Item item) {
        JsonNode value = item.value; String name = op.text();
        if (name.equals("type")) return List.of(item.with(JsonNode.string(value.kind().name().toLowerCase(java.util.Locale.ROOT))));
        if (name.equals("size")) { if (value.kind() != JsonNode.Kind.ARRAY && program.strict()) return navigation(item, "22039"); return List.of(item.with(JsonNode.number(Decimal.of(value.kind() == JsonNode.Kind.ARRAY ? value.elements().size() : 1)))); }
        if (name.equals("keyvalue")) {
            if (value.kind() != JsonNode.Kind.OBJECT) throw new SqlException("2203C", "keyvalue requires an object");
            long id = item.base.id(value); ArrayList<Item> results = new ArrayList<>();
            for (JsonNode.Member member : value.members()) {
                JsonNode pair = JsonNode.object(List.of(new JsonNode.Member("id", JsonNode.number(Decimal.of(id))), new JsonNode.Member("key", JsonNode.string(member.key())), new JsonNode.Member("value", member.value())));
                results.add(new Item(pair, item.last, item.suppress, new Base(pair, -1)));
            }
            return results;
        }
        try {
            JsonNode result;
            if (name.equals("string")) {
                if (value.kind() != JsonNode.Kind.STRING && value.kind() != JsonNode.Kind.NUMBER && value.kind() != JsonNode.Kind.BOOLEAN) throw invalidMethod();
                result = JsonNode.string(value.kind() == JsonNode.Kind.STRING ? value.string() : value.render());
            } else if (name.equals("boolean")) {
                if (value.kind() == JsonNode.Kind.BOOLEAN) result = value;
                else if (value.kind() == JsonNode.Kind.NUMBER) { String digits = value.number().toString(); if (digits.indexOf('.') >= 0) throw invalidMethod(); result = JsonNode.bool(SqlCasts.cast(SqlValue.ofText(digits), SqlType.I32).asInteger() != 0); }
                else if (value.kind() == JsonNode.Kind.STRING) { if (!value.string().equals(value.string().strip())) throw invalidMethod(); result = JsonNode.bool(SqlCasts.cast(SqlValue.ofText(value.string()), SqlType.BOOLEAN).asBoolean()); }
                else throw invalidMethod();
            } else if (name.equals("double")) {
                if (value.kind() != JsonNode.Kind.NUMBER && value.kind() != JsonNode.Kind.STRING) throw invalidMethod();
                double number = SqlCasts.cast(SqlValue.ofText(value.kind() == JsonNode.Kind.NUMBER ? value.number().toString() : value.string()), SqlType.F64).asFloat();
                if (!Double.isFinite(number)) throw invalidMethod();
                result = value.kind() == JsonNode.Kind.NUMBER ? value : JsonNode.number(floatDecimal(number));
            } else if (name.equals("integer") || name.equals("bigint")) {
                SqlValue source = value.kind() == JsonNode.Kind.NUMBER ? SqlValue.ofDecimal(value.number()) : value.kind() == JsonNode.Kind.STRING ? SqlValue.ofText(value.string()) : null;
                if (source == null) throw invalidMethod(); result = JsonNode.number(Decimal.of(SqlCasts.cast(source, name.equals("integer") ? SqlType.I32 : SqlType.I64).asInteger()));
            } else {
                Decimal number;
                if (value.kind() == JsonNode.Kind.NUMBER) number = value.number();
                else if (value.kind() == JsonNode.Kind.STRING && (name.equals("number") || name.equals("decimal"))) number = Decimal.parse(value.string());
                else throw invalidMethod();
                if (name.equals("abs")) number = number.abs(); else if (name.equals("floor")) number = number.floor(); else if (name.equals("ceiling")) number = number.ceil();
                else if (name.equals("decimal") && op.args().size() > 1) {
                    int precision = Math.toIntExact(Decimal.parse(op.args().get(1).text()).toLongExact()), scale = op.args().size() > 2 ? Math.toIntExact(Decimal.parse(op.args().get(2).text()).toLongExact()) : 0;
                    if (precision < 1 || precision > 1000 || scale < -1000 || scale > 1000) throw new SqlException("22023", "invalid jsonpath decimal modifier");
                    number = number.roundPlaces(scale); if (!number.isZero() && number.precision() - number.scale() > precision - scale) throw invalidMethod();
                }
                result = JsonNode.number(number);
            }
            return List.of(item.with(result));
        } catch (ArithmeticException failure) { throw invalidMethod(); }
        catch (SqlException failure) { if (failure.sqlState().equals("22023") || !suppressible(failure)) throw failure; throw invalidMethod(); }
    }
    /** PostgreSQL's deterministic binary-json layout, used only for keyvalue object ids. */
    private static final class Base {
        private final JsonNode root; private long number;
        private java.util.IdentityHashMap<JsonNode, Long> offsets;
        Base(JsonNode root, long number) { this.root = root; this.number = number; }
        long id(JsonNode value) {
            if (offsets == null) { offsets = new java.util.IdentityHashMap<>(); layout(root, 0); }
            return number * 10_000_000_000L + offsets.getOrDefault(value, 0L);
        }
        private long layout(JsonNode node, long offset) {
            if (node.kind() == JsonNode.Kind.ARRAY || node.kind() == JsonNode.Kind.OBJECT) {
                offset = align(offset); offsets.put(node, offset);
                if (node.kind() == JsonNode.Kind.ARRAY) {
                    long end = offset + 4 + 4L * node.elements().size();
                    for (JsonNode child : node.elements()) end = layout(child, end);
                    return end;
                }
                long end = offset + 4 + 8L * node.members().size();
                for (JsonNode.Member member : node.members()) end += SqlText.encodedLength(member.key());
                for (JsonNode.Member member : node.members()) end = layout(member.value(), end);
                return end;
            }
            if (node.kind() == JsonNode.Kind.STRING) return offset + SqlText.encodedLength(node.string());
            if (node.kind() != JsonNode.Kind.NUMBER) return offset;
            Decimal value = node.number(); int groups = 0, weight = value.base10000Weight();
            if (!value.isZero()) {
                String digits = value.coefficientDigits(); int zeros = 0;
                for (int i = digits.length() - 1; i >= 0 && digits.charAt(i) == '0'; i--) zeros++;
                groups = weight - Math.floorDiv(zeros - value.scale(), 4) + 1;
            }
            return align(offset) + (value.scale() <= 63 && weight >= -64 && weight <= 63 ? 6 : 8) + 2L * groups;
        }
        private static long align(long value) { return (value + 3) & ~3L; }
    }
    private static Decimal floatDecimal(double value) {
        Decimal exact = Decimal.fromDoubleExact(value); int places = 15 - exact.precision() + exact.scale();
        Decimal rounded = exact.roundPlaces(places);
        Decimal unit = Decimal.parse("1e" + -places);
        if (exact.subtract(rounded).abs().multiply(Decimal.of(2)).compareTo(unit) == 0 && (rounded.divide(unit, 0).toLongExact() & 1) != 0) rounded = rounded.subtract(exact.negative() ? unit.negate() : unit);
        return rounded.rescale(rounded.minimumScale());
    }
    private static SqlException invalidMethod() { return new SqlException("22036", "invalid jsonpath item method argument"); }
}
