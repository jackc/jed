package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.TreeSet;

/** Persisted resolved signatures pin host implementations without making table reads need their code. */
final class HostDependencies {
    private HostDependencies() {}
    record Dependency(String name, List<SqlType> arguments, SqlType result, String componentId, long semanticVersion) {
        Dependency { arguments = List.copyOf(arguments); }
        HostFunction schemaFunction() {
            return HostFunction.scalar(name, arguments, result, ignored -> { throw new IllegalStateException("schema-only host function"); }).volatility(HostFunction.Volatility.IMMUTABLE).component(componentId, semanticVersion);
        }
    }
    static int compare(Dependency a, Dependency b) {
        int order = SqlText.compare(a.name(), b.name()); if (order != 0) return order;
        for (int i = 0; i < Math.min(a.arguments().size(), b.arguments().size()); i++) {
            order = Integer.compare(CatalogCodec.code(a.arguments().get(i)), CatalogCodec.code(b.arguments().get(i))); if (order != 0) return order;
        }
        return Integer.compare(a.arguments().size(), b.arguments().size());
    }
    static List<Dependency> collect(SqlParser.Expr syntax, ScalarPlan.Resolver resolver, boolean index) {
        TreeSet<Dependency> dependencies = new TreeSet<>(HostDependencies::compare); visit(syntax, resolver, index, dependencies);
        if (dependencies.size() > 65535) throw new SqlException("54000", "too many host dependencies");
        return List.copyOf(dependencies);
    }
    private static void visit(SqlParser.Expr syntax, ScalarPlan.Resolver resolver, boolean index, TreeSet<Dependency> dependencies) {
        if (syntax == null) return;
        SqlParser.Expr expression = DefaultExpression.unwrap(syntax);
        for (SqlParser.Expr child : DefaultExpression.children(expression)) visit(child, resolver, index, dependencies);
        if (!(expression instanceof SqlParser.Call call)) return;
        ScalarPlan.Expression resolved = HostFunctions.resolve(call, resolver);
        if (resolved == null || !(resolved.eval() instanceof HostFunctions.Call host)) return;
        HostFunction function = host.function;
        if (function.componentId() == null || function.componentId().isEmpty() || index && function.volatility() != HostFunction.Volatility.IMMUTABLE)
            throw new SqlException("42P17", "persisted host function must declare a component identity" + (index ? " and be immutable" : ""));
        if (SqlText.encodedLength(function.name()) > 65535 || SqlText.encodedLength(function.componentId()) > 65535 || function.arguments().size() > 65535) throw new SqlException("54000", "host dependency metadata too large");
        dependencies.add(new Dependency(function.name(), function.arguments(), function.result(), function.componentId(), function.semanticVersion()));
    }
    static List<Dependency> forIndex(Engine.Index index, Engine.Table table, QueryEnvironment environment) {
        ScalarPlan.Resolver resolver = environment.resolver(table.bindings(null), new ScalarPlan.Frame()).persisted();
        TreeSet<Dependency> dependencies = new TreeSet<>(HostDependencies::compare);
        for (Engine.IndexKey key : index.keys()) if (key.expression() != null) dependencies.addAll(collect(key.expression(), resolver, true));
        dependencies.addAll(collect(index.predicate(), resolver, true));
        if (index.kind() != 0 && !dependencies.isEmpty()) throw new SqlException("42P17", "index method cannot persist host dependencies");
        return List.copyOf(dependencies);
    }
    static boolean matches(List<Dependency> dependencies, Environment environment) {
        for (Dependency dependency : dependencies) {
            HostFunction current = environment.exactFunction(dependency.name(), dependency.arguments());
            if (current == null || !same(dependency, current)) return false;
        }
        return true;
    }
    static void require(List<Dependency> dependencies, Environment environment) {
        for (Dependency dependency : dependencies) {
            HostFunction current = environment.exactFunction(dependency.name(), dependency.arguments());
            if (current == null) throw new SqlException("42883", "missing host function: " + dependency.name());
            if (!same(dependency, current)) throw new SqlException("XX002", "persisted host function component/version differs: " + dependency.name());
        }
    }
    private static boolean same(Dependency dependency, HostFunction function) {
        return dependency.result().equals(function.result()) && dependency.componentId().equals(function.componentId()) && dependency.semanticVersion() == function.semanticVersion();
    }
}
