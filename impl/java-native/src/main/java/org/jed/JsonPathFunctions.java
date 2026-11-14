package org.jed;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Set;

/** SQL entry points for a compiled SQL/JSON path. */
final class JsonPathFunctions {
    private JsonPathFunctions() {}
    static boolean recognized(String name) { return Set.of("jsonb_path_exists", "jsonb_path_match", "jsonb_path_query_array", "jsonb_path_query_first", "jsonb_path_query", "jsonb_path_exists_tz", "jsonb_path_match_tz", "jsonb_path_query_array_tz", "jsonb_path_query_first_tz", "jsonb_path_query_tz").contains(name); }
    static SqlParser.Call normalize(SqlParser.Call call) {
        List<String> names = List.of("target", "path", "vars", "silent"); SqlParser.Expr[] args = new SqlParser.Expr[4]; int position = 0;
        for (SqlParser.Expr argument : call.args()) {
            int index = argument instanceof SqlParser.NamedArgument named ? names.indexOf(named.name()) : position++;
            if (index < 0 || index >= args.length) throw new SqlException("42883", "unknown jsonpath function parameter");
            if (args[index] != null) throw new SqlException("42601", "parameter specified more than once");
            args[index] = argument instanceof SqlParser.NamedArgument named ? named.value() : argument;
        }
        if (args[0] == null || args[1] == null) throw new SqlException("42883", "jsonpath target and path required");
        if (args[2] == null) args[2] = new SqlParser.Literal(Lexer.Kind.STRING, "{}");
        if (args[3] == null) args[3] = new SqlParser.Literal(Lexer.Kind.WORD, "false");
        return new SqlParser.Call(call.name(), Arrays.asList(args));
    }
    static List<ScalarPlan.Expression> arguments(SqlParser.Call syntax, ScalarPlan.Resolver resolver) {
        SqlParser.Call call = normalize(syntax); ArrayList<ScalarPlan.Expression> args = new ArrayList<>();
        SqlType[] types = {SqlType.JSONB, SqlType.JSONPATH, SqlType.JSONB, SqlType.BOOLEAN};
        for (int i = 0; i < 4; i++) {
            ScalarPlan.Expression arg = i == 3 ? JsonFunctions.booleanArgument(call.args().get(i), resolver) : resolver.resolve(call.args().get(i), types[i]);
            if (i == 0 && arg.type() == SqlType.JSON) { ScalarPlan.Expression original = arg; arg = new ScalarPlan.Expression(SqlType.JSONB, meter -> SqlCasts.cast(original.run(meter), SqlType.JSONB, meter), original.constant()); }
            if (arg.type() != types[i] && arg.type() != SqlType.UNKNOWN) throw new SqlException("42883", "jsonpath function argument type mismatch");
            args.add(arg);
        }
        return List.copyOf(args);
    }
    static ScalarPlan.Expression resolve(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        if (!recognized(call.name()) || call.name().equals("jsonb_path_query") || call.name().equals("jsonb_path_query_tz")) return null;
        List<ScalarPlan.Expression> args = arguments(call, resolver); String name = call.name().replace("_tz", "");
        SqlType type = name.equals("jsonb_path_exists") || name.equals("jsonb_path_match") ? SqlType.BOOLEAN : SqlType.JSONB;
        return new ScalarPlan.Expression(type, meter -> {
            meter.operation(); ArrayList<SqlValue> values = new ArrayList<>();
            for (ScalarPlan.Expression arg : args) { SqlValue value = arg.run(meter); if (value.isNull()) return SqlValue.nullValue(type); values.add(value); }
            JsonPathEval.Result result = JsonPathEval.evaluate(values.get(1).asJsonPath(), values.get(0).asJsonb(), values.get(2).asJsonb(), values.get(3).asBoolean(), name.equals("jsonb_path_exists"), meter);
            if (name.equals("jsonb_path_exists")) return result.failed() ? SqlValue.nullValue(type) : SqlValue.ofBoolean(!result.items().isEmpty());
            if (name.equals("jsonb_path_match")) return match(result, values.get(3).asBoolean());
            if (name.equals("jsonb_path_query_first")) return result.items().isEmpty() ? SqlValue.nullValue(type) : SqlValue.ofJsonb(result.items().getFirst());
            SqlValue output = SqlValue.ofJsonb(JsonNode.array(result.items()));
            if (!args.stream().allMatch(ScalarPlan.Expression::constant)) meter.scalarInput(MemoryAccount.payload(output));
            return output;
        }, args.stream().allMatch(ScalarPlan.Expression::constant));
    }
    static ScalarPlan.Expression binary(SqlParser.Binary syntax, ScalarPlan.Resolver resolver) {
        if (!syntax.op().equals("@?") && !syntax.op().equals("@@")) return null;
        ScalarPlan.Expression a = resolver.resolve(syntax.left(), SqlType.JSONB), b = resolver.resolve(syntax.right(), SqlType.JSONPATH);
        if (a.type() != SqlType.JSONB || b.type() != SqlType.JSONPATH) throw new SqlException("42883", "jsonpath operator argument type mismatch");
        return new ScalarPlan.Expression(SqlType.BOOLEAN, meter -> {
            meter.operation(); SqlValue x = a.run(meter); if (x.isNull()) return SqlValue.nullValue(SqlType.BOOLEAN);
            SqlValue y = b.run(meter); if (y.isNull()) return SqlValue.nullValue(SqlType.BOOLEAN);
            boolean exists = syntax.op().equals("@?"); JsonPathEval.Result result = JsonPathEval.evaluate(y.asJsonPath(), x.asJsonb(), JsonNode.object(List.of()), true, exists, meter, 1);
            return exists ? result.failed() ? SqlValue.nullValue(SqlType.BOOLEAN) : SqlValue.ofBoolean(!result.items().isEmpty()) : match(result, true);
        }, a.constant() && b.constant());
    }
    private static SqlValue match(JsonPathEval.Result result, boolean silent) {
        if (result.items().size() != 1 || result.items().getFirst().kind() != JsonNode.Kind.BOOLEAN && result.items().getFirst().kind() != JsonNode.Kind.NULL) {
            if (silent) return SqlValue.nullValue(SqlType.BOOLEAN); throw new SqlException("22038", "jsonpath match requires one boolean");
        }
        JsonNode value = result.items().getFirst(); return value.kind() == JsonNode.Kind.NULL ? SqlValue.nullValue(SqlType.BOOLEAN) : SqlValue.ofBoolean(value.booleanValue());
    }
}
