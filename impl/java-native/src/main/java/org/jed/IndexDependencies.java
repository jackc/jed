package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.TreeMap;

/** Compositional persisted-expression admission and exact loaded timezone identities. */
final class IndexDependencies {
    private IndexDependencies() {}
    record ZonePin(String name, String version, int crc) {}
    record TimeZones(boolean dynamic, List<ZonePin> pins) {
        TimeZones { pins = List.copyOf(pins); }
        static TimeZones none() { return new TimeZones(false, List.of()); }
        boolean present() { return dynamic || !pins.isEmpty(); }
    }
    private static SqlException dependent() { return new SqlException("42P17", "index expression depends on session or mutable state"); }
    static void validateLiteral(String text, SqlType target) {
        if (target.kind() == SqlType.Kind.TIMESTAMPTZ && !Temporal.explicitTimestamp(text)) throw dependent();
        if (target.kind() == SqlType.Kind.DATE && List.of("today", "tomorrow", "yesterday", "now").contains(TimeZoneData.fold(text.strip()))) throw dependent();
    }
    static boolean containsZone(SqlType type) {
        return switch (type.kind()) {
            case TIMESTAMPTZ -> true;
            case ARRAY, RANGE -> containsZone(type.element());
            case COMPOSITE, RECORD -> type.fields().stream().anyMatch(field -> containsZone(field.type()));
            default -> false;
        };
    }
    static TimeZones analyze(SqlParser.Expr syntax, ScalarPlan.Resolver resolver) {
        Collector collector = new Collector(resolver); collector.visit(syntax); return collector.finish();
    }
    static TimeZones forIndex(Engine.Index index, Engine.Table table, QueryEnvironment environment) {
        Collector collector = new Collector(environment.resolver(table.bindings(null), new ScalarPlan.Frame()));
        for (Engine.IndexKey key : index.keys()) if (key.expression() != null) collector.visit(key.expression());
        collector.visit(index.predicate()); return collector.finish();
    }
    static TimeZones merge(TimeZones first, TimeZones second) {
        TreeMap<String, ZonePin> pins = new TreeMap<>(SqlText::compare);
        for (ZonePin pin : first.pins()) pins.put(pin.name(), pin);
        for (ZonePin pin : second.pins()) pins.put(pin.name(), pin);
        return new TimeZones(first.dynamic() || second.dynamic(), List.copyOf(pins.values()));
    }
    static boolean matches(TimeZones dependencies, Environment environment) {
        if (!dependencies.present()) return true;
        List<ZonePin> loaded = environment.zonePins();
        if (dependencies.dynamic()) return dependencies.pins().equals(loaded);
        for (ZonePin pin : dependencies.pins()) if (!loaded.contains(pin)) return false;
        return true;
    }
    static void require(Engine.Index index, Environment environment) {
        StoredDefinitions.require(index);
        HostDependencies.require(index.hostDependencies(), environment);
        if (!matches(index.timezoneDependencies(), environment)) throw new SqlException("XX002", "timezone data differs from persisted index: " + index.name());
    }
    private static final class Collector {
        final ScalarPlan.Resolver resolver;
        final CostMeter meter;
        final TreeMap<String, ZonePin> pins = new TreeMap<>(SqlText::compare);
        boolean dynamic;
        Collector(ScalarPlan.Resolver resolver) { this.resolver = resolver; meter = resolver.meter(); }
        TimeZones finish() {
            if (pins.size() > 65535) throw new SqlException("54000", "too many timezone dependencies");
            return new TimeZones(dynamic, List.copyOf(pins.values()));
        }
        void pin(ZonePin pin) {
            if (pin == null) return;
            if (pin.version().isEmpty()) throw dependent();
            if (SqlText.encodedLength(pin.name()) > 65535 || SqlText.encodedLength(pin.version()) > 65535) throw new SqlException("54000", "timezone dependency metadata is too long");
            pins.put(pin.name(), pin);
        }
        SqlType type(SqlParser.Expr expression) { return resolver.resolve(expression, null).type(); }
        SqlValue constant(SqlParser.Expr syntax) {
            ScalarPlan.Expression expression = resolver.resolve(syntax, null);
            if (!expression.constant()) return null;
            return meter.unmetered(() -> expression.run(meter));
        }
        void zone(SqlParser.Expr syntax) {
            SqlValue value = constant(syntax);
            if (value == null) { dynamic = true; for (ZonePin pin : meter.environment().zonePins()) pin(pin); }
            else if (!value.isNull()) pin(meter.environment().zonePin(value.asText()));
        }
        void visit(SqlParser.Expr syntax) {
            if (syntax == null) return;
            SqlParser.Expr expression = DefaultExpression.unwrap(syntax);
            if (expression instanceof SqlParser.Window) throw new SqlException("42P20", "window in index expression");
            if (expression instanceof SqlParser.Parameter) throw new SqlException("42P02", "parameter in index expression");
            if (expression instanceof SqlParser.Subquery || expression instanceof SqlParser.Exists || expression instanceof SqlParser.In in && in.query() != null
                    || expression instanceof SqlParser.ArrayConstructor array && array.query() != null || expression instanceof SqlParser.Quantified quantified && quantified.query() != null)
                throw new SqlException("0A000", "subquery in index expression");
            if (expression instanceof SqlParser.Call call && AggregatePlan.aggregateName(call.name())) throw new SqlException("42803", "aggregate in index expression");
            for (SqlParser.Expr child : DefaultExpression.children(expression)) visit(child);
            if (expression instanceof SqlParser.Cast cast) cast(cast);
            else if (expression instanceof SqlParser.Call call) call(call);
            else if (expression instanceof SqlParser.Binary binary) {
                if (type(binary.left()) == SqlType.DATE && binary.right() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) validateLiteral(literal.text(), SqlType.DATE);
                if (type(binary.right()) == SqlType.DATE && binary.left() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) validateLiteral(literal.text(), SqlType.DATE);
                binary(binary);
            }
            else if (expression instanceof SqlParser.JsonSql json && json.kind().equals("json_scalar") && containsZone(type(json.context()))) throw dependent();
        }
        void cast(SqlParser.Cast cast) {
            SqlType from = type(cast.value()), to = resolver.type(cast.target());
            if (from.kind() == SqlType.Kind.TEXT && (containsZone(to) || to.kind() == SqlType.Kind.DATE || to.kind() == SqlType.Kind.TIMESTAMP || to.kind() == SqlType.Kind.INTERVAL)) {
                SqlValue value = constant(cast.value());
                if (value == null) throw dependent();
                if (!value.isNull()) meter.immutableInput(() -> SqlCasts.literal(value.asText(), to, meter));
            }
            if (to.kind() == SqlType.Kind.TEXT && containsZone(from)) throw dependent();
            if (from.kind() == SqlType.Kind.TIMESTAMPTZ && (to.kind() == SqlType.Kind.TIMESTAMP || to.kind() == SqlType.Kind.DATE)
                    || to.kind() == SqlType.Kind.TIMESTAMPTZ && (from.kind() == SqlType.Kind.TIMESTAMP || from.kind() == SqlType.Kind.DATE)) throw dependent();
        }
        void binary(SqlParser.Binary binary) {
            SqlType left = type(binary.left()), right = type(binary.right());
            if (binary.op().equals("||") && (left.kind() == SqlType.Kind.TEXT || right.kind() == SqlType.Kind.TEXT) && (containsZone(left) || containsZone(right))) throw dependent();
            if (binary.op().equals("+") || binary.op().equals("-")) {
                SqlParser.Expr interval = left.kind() == SqlType.Kind.TIMESTAMPTZ && right.kind() == SqlType.Kind.INTERVAL ? binary.right()
                    : right.kind() == SqlType.Kind.TIMESTAMPTZ && left.kind() == SqlType.Kind.INTERVAL ? binary.left() : null;
                if (interval != null) {
                    SqlValue value = constant(interval);
                    if (value == null || !value.isNull() && (value.asInterval().months() != 0 || value.asInterval().days() != 0)) throw dependent();
                }
            }
        }
        void call(SqlParser.Call original) {
            ScalarPlan.Expression host = HostFunctions.resolve(original, resolver);
            if (host != null) { HostDependencies.collect(original, resolver, true); return; }
            SqlParser.Call call = NamedFunctions.normalize(original); String name = call.name(); List<SqlParser.Expr> args = call.args();
            if (List.of("extract", "date_part").contains(name) && args.size() == 2 && type(args.get(1)).kind() == SqlType.Kind.TIMESTAMPTZ) {
                SqlValue field = constant(args.getFirst());
                if (field == null || field.isNull() || !TimeZoneData.fold(field.asText()).equals("epoch")) throw dependent();
                return;
            }
            if (name.equals("timezone") && args.size() == 2) { zone(args.getFirst()); return; }
            if (name.equals("date_trunc") && args.size() >= 2 && type(args.get(1)).kind() == SqlType.Kind.TIMESTAMPTZ) {
                if (args.size() == 2) throw dependent(); zone(args.get(2)); return;
            }
            if (name.equals("make_timestamptz")) { if (args.size() < 7) throw dependent(); zone(args.get(6)); return; }
            if (List.of("to_json", "to_jsonb", "array_to_json", "row_to_json", "jsonb_build_array", "json_build_array", "jsonb_build_object", "json_build_object").contains(name))
                for (SqlParser.Expr arg : args) if (containsZone(type(arg))) throw dependent();
            if (name.endsWith("_tz") && name.startsWith("jsonb_path_")) throw dependent();
            List<SqlType> types = new ArrayList<>();
            for (SqlParser.Expr arg : args) types.add(type(arg instanceof SqlParser.NamedArgument named ? named.value() : arg));
            for (FunctionCatalog.Fact fact : FunctionCatalog.FACTS) {
                if (!fact.name().equals(name) || fact.volatility().equals("immutable")) continue;
                boolean matches = fact.arity() == types.size();
                if (List.of("concat", "concat_ws", "format").contains(name)) matches = true;
                if (!matches) continue;
                for (int i = 0; i < Math.min(types.size(), fact.families().size()); i++) if (!family(fact.families().get(i), types.get(i))) { matches = false; break; }
                if (matches) throw dependent();
            }
        }
    }
    private static boolean family(String name, SqlType type) {
        return switch (name) {
            case "any" -> true;
            case "integer" -> type.isInteger();
            case "float" -> type.kind() == SqlType.Kind.F32 || type.kind() == SqlType.Kind.F64;
            case "anyarray" -> type.kind() == SqlType.Kind.ARRAY;
            default -> type.kind().name().equalsIgnoreCase(name);
        };
    }
}
