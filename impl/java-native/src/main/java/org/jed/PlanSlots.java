package org.jed;

import java.util.List;
import java.util.function.IntUnaryOperator;

/** Translate verbose local slots within one query frame, leaving quoted SQL and subquery bodies alone. */
final class PlanSlots {
    private PlanSlots() {}
    static PlanNode shift(PlanNode node, int offset) { return transform(node, value -> Math.addExact(value, offset)); }
    static PlanNode remap(PlanNode node, int[] logical) { return transform(node, value -> value < logical.length ? logical[value] : value); }
    private static PlanNode transform(PlanNode node, IntUnaryOperator mapping) {
        boolean frame = node.label().equals("Subquery") || node.label().startsWith("Subquery ") || node.label().startsWith("CTE Scan ") || node.label().equals("WITH");
        List<PlanNode> children = frame ? node.children() : node.children().stream().map(child -> transform(child, mapping)).toList();
        return new PlanNode(node.label(), detail(node.detail(), mapping), node.rows(), node.cost(), children, node.lane());
    }
    private static String detail(String text, IntUnaryOperator mapping) {
        StringBuilder result = new StringBuilder(text.length()); boolean quoted = false;
        for (int i = 0; i < text.length();) {
            char c = text.charAt(i++);
            if (c == '\'') {
                result.append(c);
                if (quoted && i < text.length() && text.charAt(i) == '\'') result.append(text.charAt(i++)); else quoted = !quoted;
            } else if (!quoted && c == '#' && i < text.length() && Character.isDigit(text.charAt(i))) {
                int start = i; while (i < text.length() && Character.isDigit(text.charAt(i))) i++;
                result.append('#').append(mapping.applyAsInt(Integer.parseInt(text.substring(start, i))));
            } else result.append(c);
        }
        return result.toString();
    }
}
