package org.jed;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/** Statement-local sequence counters and session history, published only on success. */
final class Sequences {
    record History(Map<String, Long> values, String last) {
        History { values = Map.copyOf(values); }
        static History empty() { return new History(Map.of(), null); }
    }
    private final Engine.Snapshot snapshot;
    private final Map<String, Engine.Sequence> changed = new HashMap<>();
    private final Map<String, Long> history;
    private String last;
    private final java.util.Set<String> removed = new java.util.HashSet<>();
    private boolean mutating;
    private boolean persistentMutation;
    Sequences(Engine.Snapshot snapshot, History history) { this.snapshot = snapshot; this.history = new HashMap<>(history.values()); last = history.last(); }
    private static String key(String domain, String name) { return domain + "\0" + name; }
    private String key(String name) {
        String temp = key("temp", name);
        return changed.containsKey(temp) || snapshot.domains().containsKey("temp") && snapshot.domain("temp").sequences().containsKey(name) ? temp : key("main", name);
    }
    private Engine.Sequence byKey(String key) {
        if (removed.contains(key)) return null;
        if (changed.containsKey(key)) return changed.get(key);
        int split = key.indexOf('\0');
        Engine.Snapshot domain = snapshot.domains().get(key.substring(0, split));
        if (domain == null && key.startsWith("main\0")) domain = snapshot;
        return domain == null ? null : domain.sequences().get(key.substring(split + 1));
    }
    void discard(String domain, String name) { String key = key(domain, name); removed.add(key); changed.remove(key); }
    void install(String domain, Engine.Sequence sequence) { removed.remove(key(domain, sequence.name())); changed.put(key(domain, sequence.name()), sequence); }
    boolean mutating() { return mutating; }
    boolean temporaryOnly() { return mutating && !persistentMutation; }
    History history() { return new History(history, last); }
    Engine.Snapshot apply(Engine.Snapshot result) {
        for (var entry : changed.entrySet()) {
            String domain = entry.getKey().substring(0, entry.getKey().indexOf('\0'));
            Engine.Sequence sequence = entry.getValue(); Engine.Snapshot local = result.domain(domain);
            Engine.Sequence definition = local.sequences().get(sequence.name());
            if (definition != null) result = result.withDomain(domain, local.putSequence(new Engine.Sequence(sequence.name(), definition.increment(), definition.minimum(), definition.maximum(), definition.start(), definition.cache(), sequence.last(), definition.cycle(), sequence.called(), definition.owner(), definition.ownerColumn())));
        }
        return result;
    }
    void renameHistory(String domain, String oldName, String newName) {
        String oldKey = key(domain, oldName), newKey = key(domain, newName);
        Long value = history.remove(oldKey); if (value != null) history.put(newKey, value);
        if (oldKey.equals(last)) last = newKey;
    }
    static String renameReferences(String expression, String oldName, String newName) {
        List<Lexer.Token> tokens = new ArrayList<>(new Lexer(expression).scan()); boolean changed = false;
        for (int i = 0; i + 3 < tokens.size(); i++) {
            Lexer.Token function = tokens.get(i), argument = tokens.get(i + 2);
            if ((function.kind() == Lexer.Kind.WORD || function.kind() == Lexer.Kind.NAME)
                && List.of("nextval", "currval", "setval").contains(function.text())
                && tokens.get(i + 1).is("(") && argument.kind() == Lexer.Kind.STRING
                && (tokens.get(i + 3).is(")") || tokens.get(i + 3).is(",")) && name(argument.text()).equals(oldName)) {
                tokens.set(i + 2, new Lexer.Token(Lexer.Kind.STRING, nameArgument(newName), argument.start())); changed = true;
            }
        }
        return changed ? SqlTokens.render(tokens.subList(0, tokens.size() - 1)) : expression;
    }
    static SqlParser.Expr renameReferences(SqlParser.Expr expression, String oldName, String newName) {
        if (!(expression instanceof SqlParser.StoredExpression stored)) return expression;
        String text = renameReferences(stored.text(), oldName, newName);
        return text.equals(stored.text()) ? expression : new SqlParser.StoredExpression(text, new SqlParser("SELECT " + text).parse().items().getFirst().expression(), stored.position());
    }
    private Engine.Sequence sequence(String name) {
        Engine.Sequence sequence = byKey(key(name));
        if (sequence == null) throw new SqlException("42P01", "sequence does not exist: " + name);
        return sequence;
    }
    static String name(String input) {
        String text = input.trim(); if (text.equals("\"\"")) return "";
        try {
            List<Lexer.Token> tokens = new Lexer(text).scan();
            if (tokens.size() != 2 || tokens.getFirst().kind() != Lexer.Kind.WORD && tokens.getFirst().kind() != Lexer.Kind.NAME) throw new SqlException("42602", "invalid sequence name");
            return tokens.getFirst().text();
        } catch (SqlException failure) { throw new SqlException("42602", "invalid sequence name"); }
    }
    ScalarPlan.Expression resolve(SqlParser.Call syntax, ScalarPlan.Resolver resolver, QueryEnvironment environment) {
        String function = syntax.name();
        if (!List.of("nextval", "currval", "setval", "lastval").contains(function)) return null;
        int count = syntax.args().size();
        if (syntax.star() || syntax.distinct() || syntax.filter() != null || !syntax.order().isEmpty()
                || (function.equals("lastval") ? count != 0 : function.equals("setval") ? count < 2 || count > 3 : count != 1)) throw new SqlException("42883", "invalid sequence function arguments");
        List<ScalarPlan.Expression> args = new ArrayList<>();
        for (int i = 0; i < count; i++) {
            SqlType expected = i == 0 ? SqlType.TEXT : i == 1 ? SqlType.I64 : SqlType.BOOLEAN;
            ScalarPlan.Expression expression = resolver.resolve(syntax.args().get(i), expected);
            if (expression.type() != SqlType.UNKNOWN && expression.type().kind() != expected.kind() && !(i == 1 && expression.type().isInteger())) throw new SqlException("42883", "invalid sequence function argument type");
            args.add(expression);
        }
        if (count != 0 && syntax.args().getFirst() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) {
            String name = name(literal.text()); sequence(name); environment.viewSequence(name);
        }
        boolean advances = function.equals("nextval") || function.equals("setval");
        mutating |= advances;
        if (advances) persistentMutation |= !(syntax.args().getFirst() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING && key(name(literal.text())).startsWith("temp\0"));
        return new ScalarPlan.Expression(SqlType.I64, meter -> {
            meter.operation(); List<SqlValue> values = new ArrayList<>();
            for (ScalarPlan.Expression arg : args) { SqlValue value = arg.run(meter); if (value.isNull()) return SqlValue.nullValue(SqlType.I64); values.add(value); }
            if (function.equals("lastval")) { if (last == null) throw new SqlException("55000", "lastval is not defined in this session"); Long value = history.get(last); if (value == null || byKey(last) == null) throw new SqlException("55000", "last-used sequence is unavailable"); return SqlValue.ofInteger(SqlType.I64, value); }
            String name = name(values.getFirst().asText()); Engine.Sequence sequence = sequence(name); String key = key(name);
            if (function.equals("currval")) {
                Long value = history.get(key); if (value == null) throw new SqlException("55000", "currval is not defined in this session");
                return SqlValue.ofInteger(SqlType.I64, value);
            }
            meter.charge(Costs.SEQUENCE_ADVANCE); meter.guard();
            long value; boolean called = true;
            if (function.equals("setval")) {
                value = values.get(1).asInteger(); if (value < sequence.minimum() || value > sequence.maximum()) throw SqlException.numericOverflow();
                called = count < 3 || values.get(2).asBoolean();
            } else {
                value = sequence.last();
                if (sequence.called()) {
                    boolean outside = false;
                    try { value = Math.addExact(value, sequence.increment()); }
                    catch (ArithmeticException failure) { outside = true; }
                    outside |= value < sequence.minimum() || value > sequence.maximum();
                    if (outside) {
                        if (!sequence.cycle()) throw new SqlException("2200H", "sequence limit exceeded");
                        value = sequence.increment() > 0 ? sequence.minimum() : sequence.maximum();
                    }
                }
                last = key;
            }
            changed.put(key, new Engine.Sequence(name, sequence.increment(), sequence.minimum(), sequence.maximum(), sequence.start(), sequence.cache(), value, sequence.cycle(), called, sequence.owner(), sequence.ownerColumn()));
            if (called) history.put(key, value);
            return SqlValue.ofInteger(SqlType.I64, value);
        });
    }
    static SqlType serialType(SqlParser.TypeName type) {
        SqlType result = switch (type.name()) {
            case "serial", "serial4" -> SqlType.I32;
            case "bigserial", "serial8" -> SqlType.I64;
            case "smallserial", "serial2" -> SqlType.I16;
            default -> null;
        };
        if (result != null && (type.precision() >= 0 || type.dimensions() != 0)) throw new SqlException("42601", "serial cannot have type modifiers or array dimensions");
        return result;
    }
    static String objectName(String table, String column, String label) {
        String left = table, right = column == null ? "" : column;
        int available = 63 - SqlText.encode(label).length - (column == null ? 1 : 2);
        while (SqlText.encode(left).length + SqlText.encode(right).length > available) {
            if (SqlText.encode(left).length > SqlText.encode(right).length) left = left.substring(0, left.offsetByCodePoints(left.length(), -1));
            else right = right.substring(0, right.offsetByCodePoints(right.length(), -1));
        }
        return left + (column == null ? "" : "_" + right) + "_" + label;
    }
    static String primaryKeyName(String table) {
        while (SqlText.encode(table).length > 58) table = table.substring(0, table.offsetByCodePoints(table.length(), -1));
        return table + "_pkey";
    }
    static String autoName(Engine.Snapshot snapshot, String table, String column) {
        return autoName(snapshot, table, column, "seq");
    }
    static String autoName(Engine.Snapshot snapshot, String table, String column, String kind) { return autoName(snapshot, table, column, kind, java.util.Set.of()); }
    static String autoName(Engine.Snapshot snapshot, String table, String column, String kind, java.util.Set<String> reserved) {
        for (int suffix = 0; ; suffix++) {
            String label = kind + (suffix == 0 ? "" : suffix), left = table, right = column;
            int available = 63 - SqlText.encode(label).length - 2;
            while (SqlText.encode(left).length + SqlText.encode(right).length > available) {
                if (SqlText.encode(left).length > SqlText.encode(right).length) left = left.substring(0, left.offsetByCodePoints(left.length(), -1));
                else right = right.substring(0, right.offsetByCodePoints(right.length(), -1));
            }
            String name = left + "_" + right + "_" + label;
            if (!reserved.contains(name) && !Engine.relationTaken(snapshot, name) && !name.equals(table) && (!(kind.equals("key") || kind.equals("excl")) || snapshot.tables().values().stream().noneMatch(t -> ForeignKeys.nameTaken(t, name)))) return name;
        }
    }
    static String nameArgument(String name) {
        boolean bare = !name.isEmpty();
        for (int i = 0; i < name.length(); i++) { char c = name.charAt(i); bare &= c >= 'a' && c <= 'z' || c == '_' || i > 0 && c >= '0' && c <= '9'; }
        return bare ? name : "\"" + name.replace("\"", "\"\"") + "\"";
    }
    static SqlParser.StoredExpression defaultExpression(String name) {
        String text = "nextval ( '" + nameArgument(name).replace("'", "''") + "' )";
        return new SqlParser.StoredExpression(text, new SqlParser("SELECT " + text).parse().items().getFirst().expression(), 0);
    }
    static Engine.Sequence definition(String name, Engine.Sequence old, List<SqlParser.SequenceOption> options, ScalarPlan.Resolver resolver) {
        Map<String, SqlParser.SequenceOption> values = new HashMap<>(); for (var option : options) values.put(option.name(), option);
        SqlType type = values.containsKey("as") ? resolver.type(values.get("as").type()) : SqlType.I64;
        if (!type.isInteger()) throw invalid("sequence type must be integer");
        long low = type == SqlType.I16 ? Short.MIN_VALUE : type == SqlType.I32 ? Integer.MIN_VALUE : Long.MIN_VALUE;
        long high = type == SqlType.I16 ? Short.MAX_VALUE : type == SqlType.I32 ? Integer.MAX_VALUE : Long.MAX_VALUE;
        long increment = number(values, "increment", old == null ? 1 : old.increment());
        if (increment == 0) throw invalid("sequence increment must not be zero");
        long minimum = values.containsKey("minimum") ? number(values, "minimum", increment > 0 ? 1 : low) : old == null ? increment > 0 ? 1 : low : old.minimum();
        long maximum = values.containsKey("maximum") ? number(values, "maximum", increment > 0 ? high : -1) : old == null ? increment > 0 ? high : -1 : old.maximum();
        long start = number(values, "start", old == null ? increment > 0 ? minimum : maximum : old.start());
        long cache = number(values, "cache", old == null ? 1 : old.cache());
        long last = values.containsKey("restart") ? number(values, "restart", start) : old == null ? start : old.last();
        boolean cycle = number(values, "cycle", old != null && old.cycle() ? 1 : 0) != 0;
        if (minimum < low || maximum > high || minimum >= maximum || cache < 1 || start < minimum || start > maximum || last < minimum || last > maximum) throw invalid("invalid sequence bounds, start, restart or cache");
        return new Engine.Sequence(name, increment, minimum, maximum, start, cache, last, cycle, old != null && old.called() && !values.containsKey("restart"), old == null ? null : old.owner(), old == null ? -1 : old.ownerColumn());
    }
    private static long number(Map<String, SqlParser.SequenceOption> options, String name, long fallback) { var option = options.get(name); return option == null || option.value() == null ? fallback : option.value(); }
    private static SqlException invalid(String message) { return new SqlException("22023", message); }
}
