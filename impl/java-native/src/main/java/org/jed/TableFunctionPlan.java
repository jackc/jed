package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Typed, incremental FROM generators; generation is charged independently of result emission. */
final class TableFunctionPlan implements RelationPlan {
    private final String function;
    private final CostMeter planningMeter;
    private final SqlParser.Call callMetadata;
    private final List<ScalarPlan.Expression> arguments = new ArrayList<>();
    private final List<ScalarPlan.Binding> bindings;
    private final SqlType type;
    private final boolean record;
    private final boolean json, each, keys, path;
    private final boolean regex;
    private RegexFunctions.Invocation regexInvocation;
    private List<JsonNode> pathItems;
    private String jsonSource;
    private JsonNode document;
    private boolean initialized, finished;
    private long current, stop, step;
    private List<SqlValue> elements;
    private int element;
    TableFunctionPlan(SqlParser.TableFunction syntax, QueryEnvironment environment) {
        planningMeter = environment.meter(); callMetadata = syntax.call(); function = syntax.call().name();
        regex = function.equals("regexp_matches") || function.equals("regexp_split_to_table");
        path = function.equals("jsonb_path_query") || function.equals("jsonb_path_query_tz");
        if (function.equals("unnest") && syntax.call().args().size() != 1 && !syntax.definitions().isEmpty()) throw new SqlException("42601", "column definition list requires one unnest argument");
        var fact = SetReturningCatalog.FACTS.stream().filter(row -> (row.name().equals(function) || path && row.name().equals("jsonb_path_query")) && (path || regex || row.arity() == syntax.call().args().size())).findFirst()
            .orElseThrow(() -> new SqlException("42883", "table function does not exist: " + function));
        if (!path && !regex && syntax.call().args().stream().anyMatch(arg -> arg instanceof SqlParser.NamedArgument)) throw new SqlException("42883", "table function does not declare argument names");
        json = function.startsWith("json") && !path;
        each = json && function.contains("_each"); keys = json && function.endsWith("_object_keys");
        var resolver = environment.resolver();
        var declared = new ArrayList<SqlType>(); var unique = new java.util.HashSet<String>();
        for (SqlParser.Column column : syntax.definitions()) {
            if (!unique.add(column.name())) throw new SqlException("42701", "duplicate column name in definition list");
            declared.add(resolver.type(column.type()));
        }
        if (regex) { regexInvocation = RegexFunctions.invocation(syntax.call(), resolver); type = regexInvocation.type; }
        else if (path) { arguments.addAll(JsonPathFunctions.arguments(syntax.call(), resolver)); type = SqlType.JSONB; }
        else if (function.equals("generate_series")) {
            SqlType promoted = null;
            for (SqlParser.Expr arg : syntax.call().args()) {
                var value = resolver.resolve(arg, null); arguments.add(value);
                if (value.type() == SqlType.UNKNOWN) continue;
                if (!value.type().isInteger()) throw new SqlException("42883", "generate_series requires integer arguments");
                promoted = promoted == null ? value.type() : ScalarPlan.common(promoted, value.type());
            }
            type = promoted == null ? SqlType.I64 : promoted;
        } else if (function.equals("unnest")) {
            var value = resolver.resolve(syntax.call().args().getFirst(), null); arguments.add(value);
            if (value.type() == SqlType.UNKNOWN) throw new SqlException("42P18", "could not determine array element type");
            if (value.type().kind() != SqlType.Kind.ARRAY) throw new SqlException("42883", "unnest requires an array argument");
            type = value.type().element();
        } else if (json) {
            SqlType input = SqlType.named(fact.families().getFirst());
            var value = resolver.resolve(syntax.call().args().getFirst(), input);
            if (!value.type().equals(input)) throw new SqlException("42883", "table function argument type does not match");
            arguments.add(value); type = SqlType.named(fact.result());
        } else throw new SqlException("0A000", "table function is not implemented yet: " + function);
        record = type.kind() == SqlType.Kind.RECORD;
        if (record != !declared.isEmpty()) throw new SqlException("42601", record ? "record-returning function requires a column definition list" : "column definition list requires a record-returning function");
        if (record) {
            if (type.fields().size() != declared.size()) throw SqlException.typeMismatch("function return row and query-specified return row do not match");
            for (int i = 0; i < declared.size(); i++) if (!binaryCompatible(type.fields().get(i).type(), declared.get(i))) throw SqlException.typeMismatch("function return row and query-specified return row do not match");
        }
        if (syntax.columns().size() > (each ? 2 : 1)) throw new SqlException("42P10", "too many table-function column aliases");
        String label = syntax.alias() == null ? function : syntax.alias();
        String column = syntax.columns().isEmpty() ? fact.columnSource().equals("function") ? syntax.alias() == null ? function : syntax.alias() : fact.column() : syntax.columns().getFirst();
        if (record) {
            var output = new ArrayList<ScalarPlan.Binding>();
            for (int i = 0; i < declared.size(); i++) output.add(new ScalarPlan.Binding(List.of(label), syntax.definitions().get(i).name(), declared.get(i)));
            bindings = List.copyOf(output);
        } else if (each) {
            String key = syntax.columns().isEmpty() ? "key" : syntax.columns().getFirst();
            String value = syntax.columns().size() < 2 ? fact.column() : syntax.columns().get(1);
            bindings = List.of(new ScalarPlan.Binding(List.of(label), key, SqlType.TEXT), new ScalarPlan.Binding(List.of(label), value, type));
        } else bindings = List.of(new ScalarPlan.Binding(List.of(label), column, type, false, true, function.equals("unnest") ? arguments.getFirst().collation().asColumn() : CollationDerivation.NONE));
    }
    @Override public PlanNode explain(boolean verbose) {
        long count = EstimatorFacts.DEFAULT_SRF_ROWS;
        if (function.equals("generate_series")) {
            Long first = integer(0), last = integer(1);
            Long stride = callMetadata.args().size() == 2 ? 1L : integer(2);
            if (first != null && last != null && stride != null && stride != 0) {
                if (stride > 0 ? first > last : first < last) count = 0;
                else { long difference = stride > 0 ? last - first : first - last; long quotient = Long.divideUnsigned(difference, stride > 0 ? stride : -stride); count = quotient < 0 ? Long.MAX_VALUE : PlanEstimate.add(quotient, 1); }
            }
        }
        long work = regex || path || json ? 1 : 0;
        for (SqlParser.Expr argument : callMetadata.args()) work = PlanEstimate.add(work, PlanEstimate.operations(argument));
        return new PlanNode("SRF " + function, "", count, PlanEstimate.add(work, PlanEstimate.mul(count, Costs.GENERATED_ROW)), List.of(), "buffered");
    }
    private Long integer(int index) {
        if (!StatisticsEstimate.literalSyntax(callMetadata.args().get(index))) return null;
        ScalarPlan.Expression expression = arguments.get(index);
        if (!expression.constant()) return null;
        try { SqlValue value = planningMeter.unmetered(() -> expression.run(planningMeter)); return value.isNull() || !value.type().isInteger() ? null : value.asInteger(); }
        catch (SqlException failure) { if (failure.sqlState().equals("42P02")) return null; throw failure; }
    }
    @Override public List<ScalarPlan.Binding> bindings() { return bindings; }
    @Override public List<String> names() { return bindings.stream().map(ScalarPlan.Binding::name).toList(); }
    @Override public List<SqlType> types() { return bindings.stream().map(ScalarPlan.Binding::type).toList(); }
    @Override public void touch(boolean[] columns) { }
    private void initialize(CostMeter meter) {
        initialized = true;
        if (regex) { elements = regexInvocation.rows(meter); return; }
        List<SqlValue> values = new ArrayList<>();
        for (ScalarPlan.Expression arg : arguments) values.add(arg.run(meter));
        if (values.stream().anyMatch(SqlValue::isNull)) { finished = true; return; }
        if (path) {
            pathItems = JsonPathEval.evaluateRows(values.get(1).asJsonPath(), values.get(0).asJsonb(), values.get(2).asJsonb(), values.get(3).asBoolean(), meter).items();
        } else if (function.equals("generate_series")) {
            current = values.get(0).asInteger(); stop = values.get(1).asInteger(); step = values.size() == 3 ? values.get(2).asInteger() : 1;
            if (step == 0) throw new SqlException("22023", "step size cannot be equal to zero");
            finished = step > 0 ? current > stop : current < stop;
        } else if (json) {
            SqlValue input = values.getFirst();
            jsonSource = input.type() == SqlType.JSON ? input.asJson() : null;
            document = jsonSource == null ? input.asJsonb() : JsonNode.parse(jsonSource, false);
            JsonNode.Kind expected = each || keys ? JsonNode.Kind.OBJECT : JsonNode.Kind.ARRAY;
            if (jsonSource != null && document.kind() != JsonNode.Kind.ARRAY && document.kind() != JsonNode.Kind.OBJECT) JsonInput.decodedValidation(jsonSource);
            if (document.kind() != expected) throw new SqlException("22023", "table function requires a JSON " + expected.name().toLowerCase(java.util.Locale.ROOT));
            if (jsonSource != null && (type == SqlType.TEXT || keys || each)) JsonInput.decodedValidation(jsonSource);
        } else elements = values.getFirst().asArray().elements();
    }
    @Override public Row next(CostMeter meter) {
        if (!initialized) initialize(meter);
        if (finished) return null;
        if (json) return nextJson(meter);
        SqlValue value;
        if (path) {
            if (element == pathItems.size()) { finished = true; return null; }
            value = SqlValue.ofJsonb(pathItems.get(element++));
        } else if (function.equals("generate_series")) {
            value = SqlValue.ofInteger(type, current);
            try {
                long next = Math.addExact(current, step);
                finished = !type.contains(next) || (step > 0 ? next > stop : next < stop); current = next;
            } catch (ArithmeticException overflow) { finished = true; }
        } else {
            if (element == elements.size()) { finished = true; return null; }
            value = elements.get(element++);
        }
        meter.guard(); meter.charge(Costs.GENERATED_ROW); meter.guard();
        if (!record) return new Row(names(), List.of(value));
        var fields = new ArrayList<SqlValue>();
        for (int i = 0; i < bindings.size(); i++) fields.add(value.isNull() ? SqlValue.nullValue(bindings.get(i).type()) : binaryCoerce(value.asComposite().get(i), bindings.get(i).type()));
        return new Row(names(), fields);
    }
    private Row nextJson(CostMeter meter) {
        int size = each || keys ? document.members().size() : document.elements().size();
        if (element == size) { finished = true; return null; }
        int index = element++;
        meter.guard(); meter.charge(Costs.GENERATED_ROW); meter.guard();
        if (keys) return new Row(names(), List.of(SqlValue.ofText(jsonText(document.members().get(index).key()))));
        JsonNode node = each ? document.members().get(index).value() : document.elements().get(index);
        SqlValue value;
        if (type == SqlType.JSONB) value = SqlValue.ofJsonb(node);
        else if (type == SqlType.JSON) value = SqlValue.ofJson(jsonSource.substring(node.start, node.end));
        else if (node.kind() == JsonNode.Kind.NULL) value = SqlValue.nullValue(SqlType.TEXT);
        else if (node.kind() == JsonNode.Kind.STRING) value = SqlValue.ofText(jsonText(node.string()));
        else {
            if (jsonSource == null && !arguments.getFirst().constant() && (node.kind() == JsonNode.Kind.ARRAY || node.kind() == JsonNode.Kind.OBJECT)) meter.scalarInput(node.renderedBytes());
            value = SqlValue.ofText(jsonSource == null ? node.render() : jsonSource.substring(node.start, node.end));
        }
        return new Row(names(), each ? List.of(SqlValue.ofText(jsonText(document.members().get(index).key())), value) : List.of(value));
    }
    private static String jsonText(String text) {
        for (int i = 0; i < text.length(); i++) {
            char c = text.charAt(i);
            if (c == 0) throw new SqlException("22P05", "JSON string contains a zero code point");
            if (Character.isHighSurrogate(c)) {
                if (++i == text.length() || !Character.isLowSurrogate(text.charAt(i))) throw new SqlException("22P02", "invalid JSON surrogate pair");
            } else if (Character.isLowSurrogate(c)) throw new SqlException("22P02", "invalid JSON surrogate pair");
        }
        return text;
    }
    private static boolean binaryCompatible(SqlType source, SqlType target) {
        if (source.isInteger() && target.isInteger()) return true;
        if (source.kind() != target.kind()) return false;
        if (source.kind() == SqlType.Kind.ARRAY) return binaryCompatible(source.element(), target.element());
        if (source.isComposite() || source.kind() == SqlType.Kind.RANGE) return source.equals(target);
        return true;
    }
    private static SqlValue binaryCoerce(SqlValue value, SqlType target) {
        if (value.isNull()) return SqlValue.nullValue(target);
        if (target.isInteger()) return SqlValue.ofInteger(target, value.asInteger());
        if (target.kind() == SqlType.Kind.ARRAY) {
            SqlArray array = value.asArray(); var elements = new ArrayList<SqlValue>();
            for (SqlValue element : array.elements()) elements.add(binaryCoerce(element, target.element()));
            return SqlValue.ofArray(target, new SqlArray(array.dims(), array.lowerBounds(), elements));
        }
        return value.relabel(target);
    }
    @Override public void close() { finished = true; elements = null; document = null; jsonSource = null; pathItems = null; }
}
