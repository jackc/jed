package org.jed;

import java.io.IOException;
import java.util.List;

/** Single-column GiST bounds, behind primary and ordered bounds and before GIN. */
final class GistAccess {
    record Bound(Engine.Index index, SqlValue query, String strategy) {}
    private GistAccess() {}
    static Bound find(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, ScalarPlan.Resolver constants, CostMeter meter) {
        for (Engine.Index index : table.indexes().stream().filter(value -> StoredDefinitions.parsed(value) && value.kind() == 2 && value.keys().size() == 1 && value.predicate() == null).sorted((a, b) -> SqlText.compare(a.name(), b.name())).toList()) {
            Engine.IndexKey key = index.keys().getFirst(); String name = names.get(key.column()); boolean range = key.type().kind() == SqlType.Kind.RANGE;
            for (SqlParser.Expr term : FilterRules.terms(predicate)) {
                if (!(term instanceof SqlParser.Binary binary)) continue;
                String strategy = binary.op(); SqlParser.Expr operand;
                if (column(binary.left(), name, alias)) operand = binary.right();
                else if (column(binary.right(), name, alias) && List.of("&&", "=", "<@").contains(strategy)) { operand = binary.left(); if (strategy.equals("<@")) strategy = "@>"; }
                else continue;
                if (range ? !List.of("&&", "@>").contains(strategy) : !strategy.equals("=")) continue;
                ScalarPlan.Expression expression;
                try { expression = constants.resolve(operand, key.type()); }
                catch (SqlException failure) { if (List.of("42703", "42P01").contains(failure.sqlState())) continue; throw failure; }
                if (!expression.constant() && !(operand instanceof SqlParser.Name)) continue;
                SqlValue query = meter.unmetered(() -> expression.run(meter));
                if (range && !query.isNull() && query.type().kind() != SqlType.Kind.RANGE) continue;
                if (range && !query.isNull() && query.asRange().isEmpty() && strategy.equals("@>")) continue;
                if (!query.isNull() && !query.type().equals(key.type())) { try { query = SqlCasts.cast(query, key.type()); } catch (SqlException failure) { if (failure.sqlState().equals("22003")) continue; throw failure; } }
                return new Bound(index, query, strategy);
            }
        }
        return null;
    }
    private static boolean column(SqlParser.Expr expression, String name, String alias) {
        return expression instanceof SqlParser.Name ref && ref.parts().getLast().equals(name) && (ref.parts().size() == 1 || ref.parts().size() == 2 && ref.parts().getFirst().equals(alias));
    }
    static GinAccess.Candidates gather(Bound bound, CostMeter meter) throws IOException {
        GinAccess.Candidates candidates = new GinAccess.Candidates(meter);
        try {
            if (bound.query().isNull() || bound.query().type().kind() == SqlType.Kind.RANGE && bound.query().asRange().isEmpty()) return candidates;
            Engine.Index index = bound.index(); GistTree tree = index.gist();
            if (tree == null) tree = GistTree.build(index.keys().stream().map(Engine.IndexKey::type).toList(), index.entries(), null);
            tree.search(List.of(bound.query()), List.of(bound.strategy()), meter, candidates::add); return candidates;
        } catch (IOException | RuntimeException failure) { candidates.close(); throw failure; }
    }
}
