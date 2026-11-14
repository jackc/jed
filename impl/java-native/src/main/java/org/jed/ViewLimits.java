package org.jed;

import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

/** Structural view expansion admission, independent of planning passes and execution cost. */
final class ViewLimits {
    private record Shape(int depth, int views) {}
    private final Map<String, Engine.View> views;
    private final Map<String, Shape> shapes = new HashMap<>();
    private final Set<String> active = new HashSet<>();
    private ViewLimits(Map<String, Engine.View> views) { this.views = views; }
    static void check(SqlParser.Statement statement, Map<String, Engine.View> views) {
        if (views.isEmpty()) return;
        Shape shape = new ViewLimits(views).walk(statement, Set.of(), false);
        if (shape.depth > 256) throw limit();
    }
    private Shape view(String name) {
        Shape cached = shapes.get(name); if (cached != null) return cached;
        if (!active.add(name)) throw new SqlException("42P17", "recursive view definition");
        Shape body;
        try { body = walk(views.get(name).query(), Set.of(), true); }
        finally { active.remove(name); }
        Shape result = new Shape(body.depth, Math.min(1025, body.views + 1)); shapes.put(name, result); return result;
    }
    private Shape walk(Object node, Set<String> ctes, boolean insideView) {
        if (node == null) return new Shape(0, 0);
        if (node instanceof SqlParser.Table table) {
            List<String> parts = table.name().parts(); String name = parts.getLast();
            if (parts.size() == 1 && ctes.contains(name) || !views.containsKey(name)) return new Shape(0, 0);
            Shape shape = view(name);
            if (!insideView && shape.views > 1024) throw limit();
            return new Shape(shape.depth + 1, shape.views);
        }
        if (node instanceof SqlParser.With with) {
            Set<String> scope = new HashSet<>(ctes); int depth = 0, count = 0;
            if (with.recursive()) for (SqlParser.Cte cte : with.ctes()) scope.add(cte.name());
            for (SqlParser.Cte cte : with.ctes()) {
                Shape child = walk(cte.body(), scope, insideView); depth = Math.max(depth, child.depth + 1); count = Math.min(1025, count + child.views); scope.add(cte.name());
            }
            Shape body = walk(with.body(), scope, insideView); return new Shape(Math.max(depth, body.depth), Math.min(1025, count + body.views));
        }
        int own = node instanceof SqlParser.Expr || node instanceof SqlParser.Derived || node instanceof SqlParser.SetOperation || node instanceof SqlParser.CreateView ? 1 : 0;
        Iterable<?> children = node instanceof List<?> list ? list : java.util.Arrays.asList(SqlParser.depthChildren(node));
        int depth = 0, count = 0;
        for (Object child : children) {
            Shape shape = walk(child, ctes, insideView); depth = Math.max(depth, shape.depth); count = Math.min(1025, count + shape.views);
        }
        return new Shape(own + depth, count);
    }
    private static SqlException limit() { return new SqlException("54001", "expanded view exceeds statement complexity limits"); }
}
