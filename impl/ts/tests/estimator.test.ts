import assert from "node:assert/strict";
import { test } from "node:test";
import {
  ESTIMATOR_UNIT_IDS,
  SELECTIVITY_BOOLEAN,
  SELECTIVITY_EQUALITY,
  SELECTIVITY_INEQUALITY,
  SELECTIVITY_MATCH,
  SELECTIVITY_MATCHING,
  SELECTIVITY_NULL_TEST,
  SELECTIVITY_OPAQUE,
  SELECTIVITY_PAIRED_RANGE,
} from "../src/estimator_constants.ts";
import {
  type Selectivity,
  andSelectivity,
  estimateCandidate,
  estimateSelectivity,
  fractionSelectivity,
  notSelectivity,
  orSelectivity,
  saturatingEstimateAdd,
  saturatingEstimateMultiply,
  scaleEstimateCeil,
} from "../src/estimator.ts";
import { readTomlTables, specPath } from "./tomlmini.ts";

function namedSelectivity(token: string): Selectivity {
  const fractions = {
    equality: SELECTIVITY_EQUALITY,
    inequality: SELECTIVITY_INEQUALITY,
    paired_range: SELECTIVITY_PAIRED_RANGE,
    null_test: SELECTIVITY_NULL_TEST,
    match: SELECTIVITY_MATCH,
    matching: SELECTIVITY_MATCHING,
    boolean: SELECTIVITY_BOOLEAN,
    opaque: SELECTIVITY_OPAQUE,
  } as const;
  if (token === "all" || token === "zero" || token === "unique") return { kind: token };
  const fraction = fractions[token as keyof typeof fractions];
  if (fraction === undefined) throw new Error(`unknown selectivity token ${token}`);
  return fractionSelectivity(fraction);
}

function postfix(tokens: string[]): Selectivity {
  const stack: Selectivity[] = [];
  for (const token of tokens) {
    if (token === "not") stack.push(notSelectivity(stack.pop()!));
    else if (token === "and" || token === "or") {
      const rhs = stack.pop()!;
      const lhs = stack.pop()!;
      stack.push(token === "and" ? andSelectivity(lhs, rhs) : orSelectivity(lhs, rhs));
    } else stack.push(namedSelectivity(token));
  }
  assert.equal(stack.length, 1);
  return stack[0]!;
}

test("shared estimator arithmetic and predicate vectors", () => {
  for (const row of readTomlTables(specPath("cost/estimator_vectors.toml"), "arithmetic")) {
    const [a, b] = [row.big("a"), row.big("b")];
    const actual =
      row.str("op") === "sat_add"
        ? saturatingEstimateAdd(a, b)
        : row.str("op") === "sat_mul"
          ? saturatingEstimateMultiply(a, b)
          : scaleEstimateCeil(a, { numerator: b, denominator: row.big("c") });
    assert.equal(actual, row.big("expected"), row.str("id"));
  }
  for (const row of readTomlTables(specPath("cost/estimator_vectors.toml"), "predicate")) {
    assert.equal(
      estimateSelectivity(postfix(row.strs("tokens")), row.big("n")),
      row.big("expected"),
      row.str("id"),
    );
  }
});

test("shared candidate runtime-unit vectors", () => {
  for (const row of readTomlTables(specPath("cost/estimator_vectors.toml"), "candidate")) {
    const estimate = estimateCandidate({
      kind: row.str("kind"),
      indexName: row.str("index_name"),
      scanRows: row.big("scan_rows"),
      outputRows: row.big("output_rows"),
      accessPages: row.big("access_pages"),
      tableHeight: row.big("table_height"),
      filterNodes: row.big("filter_nodes"),
      accessWork: row.big("access_work"),
      producesRows: row.bool("produces_rows"),
    });
    assert.equal(estimate.rows, row.big("est_rows"), `${row.str("id")} rows`);
    assert.equal(estimate.cost, row.big("est_cost"), `${row.str("id")} cost`);
    assert.equal(estimate.tieKey, row.str("tie_key"), `${row.str("id")} tie`);
    for (const [i, id] of ESTIMATOR_UNIT_IDS.entries()) {
      const expected = row.has(`units.${id}`) ? row.big(`units.${id}`) : 0n;
      assert.equal(estimate.units[i], expected, `${row.str("id")} unit ${id}`);
    }
  }
});
