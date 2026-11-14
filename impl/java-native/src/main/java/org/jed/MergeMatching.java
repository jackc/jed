package org.jed;

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.List;

/** The one selected matching-loop shape shared by MERGE execution and EXPLAIN. */
final class MergeMatching {
    private final SqlParser.Merge syntax;
    private final Engine.Table table;
    private final RelationPlan source;
    private final ScalarPlan.Resolver pairResolver;
    private final List<ScalarPlan.Binding> bindings;
    private final CostMeter meter;
    private final List<JoinPlan.Pair> keys;
    private final boolean hash;

    MergeMatching(SqlParser.Merge syntax, Engine.Table table, RelationPlan source,
                  ScalarPlan.Resolver pairResolver, CostMeter meter) {
        this.syntax = syntax; this.table = table; this.source = source; this.pairResolver = pairResolver; this.meter = meter;
        bindings = new ArrayList<>(table.bindings(syntax.alias())); bindings.addAll(source.bindings());
        keys = JoinPlan.matchingKeys(syntax.on(), table.bindings(syntax.alias()), source.bindings());
        boolean selected = false;
        if (!keys.isEmpty()) try {
            PlanNode probe = probe(false), scan = scan(probe), input = source(false);
            if (scan.detail().startsWith("Full scan") && table.indexes().stream().noneMatch(index -> index.kind() != 0)) {
                long matches = PlanEstimate.mul(input.rows(), probe.rows());
                selected = hashCost(input, full(false), matches) < PlanEstimate.add(input.cost(), PlanEstimate.mul(input.rows(), probe.cost()));
            }
        } catch (SqlException unavailable) { if (!unavailable.sqlState().equals("0A000")) throw unavailable; }
        hash = selected;
    }
    boolean hash() { return hash; }
    byte[] key(List<SqlValue> values, boolean probe, CostMeter meter) {
        try {
            ByteArrayOutputStream bytes = new ByteArrayOutputStream(); DataOutputStream output = new DataOutputStream(bytes); boolean absent = false;
            for (JoinPlan.Pair key : keys) {
                SqlValue value = values.get(probe ? key.right() : key.left()); byte[] encoded = value.isNull() ? new byte[0] : Encoding.key(value);
                meter.charge(Math.max(1, encoded.length) * (probe ? Costs.HASH_PROBE : Costs.HASH_BUILD)); meter.guard();
                absent |= value.isNull(); output.writeInt(encoded.length); output.write(encoded);
            }
            output.flush(); return absent ? null : bytes.toByteArray();
        } catch (IOException impossible) { throw new AssertionError(impossible); }
    }
    PlanNode explain(boolean verbose) {
        PlanNode input = source(verbose), probe = probe(false), scan = scan(probe);
        long pairs = PlanEstimate.mul(input.rows(), probe.rows());
        String condition = verbose ? "on=" + new ExplainExpression(bindings, pairResolver, meter).print(syntax.on()) : "on:conjuncts=" + FilterRules.terms(syntax.on()).size();
        PlanNode loop;
        if (hash) {
            PlanNode full = full(false);
            loop = new PlanNode("Hash Join", "left; keys=" + keys.size() + "; " + condition, pairs, hashCost(input, full, pairs), List.of(input, full), "buffered");
        } else {
            PlanNode repeated = new PlanNode(scan.label(), scan.detail(), PlanEstimate.mul(input.rows(), scan.rows()), PlanEstimate.mul(input.rows(), scan.cost()), scan.children(), "buffered");
            loop = new PlanNode("Nested Loop", "left; " + condition, pairs, PlanEstimate.add(input.cost(), PlanEstimate.mul(input.rows(), probe.cost())), List.of(input, repeated), "buffered");
        }
        List<PlanNode> children = new ArrayList<>(); children.add(loop); long rows = input.rows(), cost = loop.cost();
        if (syntax.clauses().stream().anyMatch(clause -> clause.kind().equals("source"))) {
            PlanNode unmatched = describe(syntax.targetWhere(), false); children.add(unmatched); rows = PlanEstimate.add(rows, unmatched.rows()); cost = PlanEstimate.add(cost, unmatched.cost());
        }
        return new PlanNode("Merge " + table.name(), "when=" + syntax.clauses().size(), rows, cost, children, "buffered");
    }
    PlanNode actual(long total, long loopCost, long probeCost, long unmatchedCost, PlanNode sourceActual) {
        PlanNode estimate = explain(false), loop = estimate.children().getFirst(), probe = loop.children().getLast();
        PlanNode scanned = new PlanNode(probe.label(), probe.detail(), probe.rows(), probeCost, List.of(), probe.lane());
        PlanNode matched = new PlanNode(loop.label(), loop.detail(), loop.rows(), loopCost, List.of(sourceActual, scanned), loop.lane());
        List<PlanNode> children = new ArrayList<>(); children.add(matched);
        if (estimate.children().size() > 1) { PlanNode tail = estimate.children().getLast(); children.add(new PlanNode(tail.label(), tail.detail(), tail.rows(), unmatchedCost, tail.children().stream().map(PlanMetrics::zero).toList(), tail.lane())); }
        return new PlanNode(estimate.label(), estimate.detail(), estimate.rows(), total, children, estimate.lane());
    }
    private PlanNode source(boolean verbose) {
        PlanNode node = source.explain(verbose); String detail = node.detail();
        if (verbose) detail = TableExplanation.append(detail, "output=[" + java.util.stream.IntStream.range(0, source.types().size()).mapToObj(i -> "#" + i).collect(java.util.stream.Collectors.joining(", ")) + "]");
        return node.finish(detail, PlanEstimate.mul(node.rows(), Costs.ROW_PRODUCED), "buffered");
    }
    private PlanNode probe(boolean verbose) { return describe(syntax.on(), verbose); }
    private PlanNode full(boolean verbose) { return describe(null, verbose); }
    private PlanNode describe(SqlParser.Expr predicate, boolean verbose) {
        SqlParser.Select query = new SqlParser.Select(List.of(), predicate, List.of(), null, null, false);
        return TableExplanation.describe(query, table, table.columns().stream().map(Engine.Column::name).toList(), syntax.alias() == null ? table.name() : syntax.alias(), List.of(), bindings, pairResolver, meter, false, false, false, verbose, false);
    }
    private static PlanNode scan(PlanNode node) { return node.label().equals("Filter") ? node.children().getFirst() : node; }
    private long hashCost(PlanNode input, PlanNode full, long matches) {
        long width = 0;
        for (JoinPlan.Pair key : keys) { SqlType type = table.columns().get(key.left()).type(); int fixed = type.kind() == SqlType.Kind.DECIMAL ? 0 : Encoding.fixedWidth(type); width = PlanEstimate.add(width, fixed > 0 ? fixed : EstimatorFacts.DEFAULT_VARIABLE_KEY_BYTES); }
        long build = PlanEstimate.mul(full.rows(), width), probes = PlanEstimate.add(PlanEstimate.mul(input.rows(), width), PlanEstimate.mul(matches, PlanEstimate.add(keys.size() * 4L, width)));
        return PlanEstimate.add(PlanEstimate.add(input.cost(), full.cost()), PlanEstimate.add(build, PlanEstimate.add(probes, PlanEstimate.mul(matches, PlanEstimate.operations(syntax.on())))));
    }
}
