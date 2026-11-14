package org.jed;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Catalog-driven positional binding of named/defaulted scalar arguments, before type resolution. */
final class NamedFunctions {
    private NamedFunctions() {}
    private static SqlParser.Call jsonMutator(SqlParser.Call call) {
        boolean lax = call.name().equals("jsonb_set_lax"), insert = call.name().equals("jsonb_insert");
        List<String> names = lax ? List.of("jsonb_in", "path", "replacement", "create_if_missing", "null_value_treatment") : List.of("jsonb_in", "path", "replacement", insert ? "insert_after" : "create_if_missing");
        SqlParser.Expr[] values = new SqlParser.Expr[names.size()]; int position = 0;
        for (SqlParser.Expr argument : call.args()) {
            int index = argument instanceof SqlParser.NamedArgument named ? names.indexOf(named.name()) : position++;
            if (index < 0 || index >= values.length) throw new SqlException("42883", "unknown JSON mutator parameter");
            if (values[index] != null) throw new SqlException("42601", "argument supplied more than once");
            values[index] = argument instanceof SqlParser.NamedArgument named ? named.value() : argument;
        }
        for (int i = 0; i < 3; i++) if (values[i] == null) throw new SqlException("42883", "missing JSON mutator parameter");
        if (values[3] == null) values[3] = new SqlParser.Literal(Lexer.Kind.WORD, insert ? "false" : "true");
        if (lax && values[4] == null) values[4] = new SqlParser.Literal(Lexer.Kind.STRING, "use_json_null");
        return new SqlParser.Call(call.name(), Arrays.asList(values));
    }
    static SqlParser.Call normalize(SqlParser.Call call) {
        if (JsonPathFunctions.recognized(call.name())) return JsonPathFunctions.normalize(call);
        if (List.of("jsonb_set", "jsonb_insert", "jsonb_set_lax").contains(call.name())) return jsonMutator(call);
        boolean named = call.args().stream().anyMatch(value -> value instanceof SqlParser.NamedArgument);
        List<FunctionCatalog.Fact> signatures = FunctionCatalog.FACTS.stream().filter(fact -> fact.name().equals(call.name()) && !fact.argumentNames().isEmpty()).toList();
        if (signatures.isEmpty()) {
            if (named) throw new SqlException("42883", "function has no named parameters: " + call.name());
            return call;
        }
        for (FunctionCatalog.Fact signature : signatures) {
            SqlParser.Expr[] values = new SqlParser.Expr[signature.arity()];
            int position = 0; boolean valid = true;
            for (SqlParser.Expr argument : call.args()) {
                int index = argument instanceof SqlParser.NamedArgument item ? signature.argumentNames().indexOf(item.name()) : position++;
                if (index < 0 || index >= values.length) { valid = false; break; }
                if (values[index] != null) throw new SqlException("42601", "argument supplied more than once");
                values[index] = argument instanceof SqlParser.NamedArgument item ? item.value() : argument;
            }
            if (!valid) continue;
            for (int i = 0; i < values.length; i++) {
                if (values[i] != null) continue;
                int defaultIndex = i - (values.length - signature.defaults().size());
                if (defaultIndex < 0) { valid = false; break; }
                values[i] = new SqlParser.Literal(Lexer.Kind.INTEGER, signature.defaults().get(defaultIndex));
            }
            if (valid) return new SqlParser.Call(call.name(), Arrays.asList(values), call.distinct(), call.star(), call.order(), call.filter(), call.withinGroup());
        }
        throw new SqlException("42883", "function arguments do not match: " + call.name());
    }
}
