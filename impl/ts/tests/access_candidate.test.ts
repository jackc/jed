// Complete access-candidate inventory is an internal planner invariant the SQL corpus cannot render:
// EXPLAIN shows only the selected path. Mirrors the Rust/Go white-box case.

import assert from "node:assert/strict";
import { test } from "node:test";
import type { Select } from "../src/ast.ts";
import {
  Engine,
  estimateScanCandidates,
  inventoryScanCandidates,
  MUTATION_SCAN_BOUND_POLICY,
  renderScanCandidateIdentity,
  selectLegacyScanCandidate,
  SELECT_SCAN_BOUND_POLICY,
  type RExpr,
  type ScopeRel,
  type SelectPlan,
} from "../src/executor.ts";
import { parseSQL } from "../src/parser.ts";
import { ParamTypes } from "../src/scope.ts";
import type { Snapshot } from "../src/snapshot.ts";
import { execute } from "../src/tooling.ts";

type PlannerInternals = {
  planSelect(select: Select, parent: null, ctes: [], ptypes: ParamTypes): SelectPlan;
  readSnap(): Snapshot;
};

test("scan candidate inventory is complete, canonical, and legacy-neutral", () => {
  const db = new Engine();
  for (const sql of [
    "CREATE TABLE inventory (id i32 PRIMARY KEY, a i32, b i32, tags i32[], span i32range)",
    "CREATE INDEX z_btree ON inventory (b)",
    "CREATE INDEX a_btree ON inventory (a)",
    "CREATE INDEX z_gin ON inventory USING gin (tags)",
    "CREATE INDEX a_gin ON inventory USING gin (tags)",
    "CREATE INDEX z_gist ON inventory USING gist (span)",
    "CREATE INDEX a_gist ON inventory USING gist (span)",
    "INSERT INTO inventory VALUES (1, 1, 1, '{1}', '[1,3)')",
    "INSERT INTO inventory VALUES (2, 2, 2, '{1,2}', '[2,4)')",
    "INSERT INTO inventory VALUES (3, 3, 3, '{3}', '[5,8)')",
    "INSERT INTO inventory VALUES (4, 4, 4, '{4}', '[9,12)')",
  ]) {
    execute(db, sql);
  }

  const filter = plannedInventoryFilter(
    db,
    `SELECT id FROM inventory WHERE
     (id = 1 OR id = 2) AND id >= 0 AND
     (a = 1 OR a = 2) AND a >= 0 AND
     (b = 1 OR b = 2) AND b >= 0 AND
     tags @> ARRAY[1] AND span && i32range(1, 3)`,
  );
  const original = db.table("inventory");
  assert(original !== undefined);
  // Deliberately scramble catalog iteration: canonical identity must determine inventory order.
  const table = { ...original, indexes: [...original.indexes].reverse() };
  const rel: ScopeRel = { label: "inventory", table, offset: 0 };
  const internals = db as unknown as PlannerInternals;
  const candidates = inventoryScanCandidates(filter, rel, internals.readSnap(), db);
  assert.deepStrictEqual(
    candidates.map((candidate) => renderScanCandidateIdentity(candidate.identity)),
    [
      "pk",
      "btree:a_btree",
      "btree:z_btree",
      "gist:a_gist",
      "gist:z_gist",
      "gin:a_gin",
      "gin:z_gin",
      "pk_interval",
      "index_interval:a_btree",
      "index_interval:z_btree",
      "full",
    ],
  );
  for (const candidate of candidates) {
    assert.strictEqual(candidate.residual, filter);
    assert.equal(candidate.bound === null, candidate.identity.kind === "full");
    if (candidate.identity.kind === "btree" || candidate.identity.kind === "index_interval") {
      assert.deepStrictEqual(candidate.scanOrder, {
        kind: "indexKey",
        indexName: candidate.identity.indexName,
        reversible: false,
      });
    } else {
      assert.deepStrictEqual(candidate.scanOrder, { kind: "storageKey", reversible: true });
    }
  }
  const estimates = estimateScanCandidates(candidates, rel, db, true);
  assert.equal(estimates.length, candidates.length);
  const logicalRows = estimates[0]!.rows;
  for (const [i, estimate] of estimates.entries()) {
    assert.equal(
      estimate.rows,
      logicalRows,
      `${renderScanCandidateIdentity(candidates[i]!.identity)} logical rows`,
    );
    assert.match(estimate.tieKey, /^[0-6]:/);
    assert(estimate.cost >= 0n);
  }
  for (const [sql, rows, emptyCandidate] of [
    ["SELECT id FROM inventory WHERE a IN (1, 1, 1, 1, 1)", 1n, null],
    ["SELECT id FROM inventory WHERE a = NULL", 0n, "btree:a_btree"],
    ["SELECT id FROM inventory WHERE a = 1 AND a = 2", 0n, "btree:a_btree"],
    ["SELECT id FROM inventory WHERE a > 3 AND a < 2", 0n, "btree:a_btree"],
  ] as const) {
    const shapeFilter = plannedInventoryFilter(db, sql);
    const shapeCandidates = inventoryScanCandidates(shapeFilter, rel, internals.readSnap(), db);
    for (const [i, estimate] of estimateScanCandidates(shapeCandidates, rel, db, true).entries()) {
      assert.equal(estimate.rows, rows, `${sql} logical rows`);
      if (renderScanCandidateIdentity(shapeCandidates[i]!.identity) === emptyCandidate) {
        assert.equal(estimate.cost, 0n, `${sql} ${emptyCandidate} empty access`);
      }
    }
  }
  const fullEstimate = estimateScanCandidates(
    inventoryScanCandidates(null, rel, internals.readSnap(), db),
    rel,
    db,
    true,
  )[0]!;
  const fullActual = execute(db, "SELECT id FROM inventory");
  assert.equal(fullActual.kind, "query");
  assert.equal(fullEstimate.cost, fullActual.cost, "exact full-scan estimate equals actual cost");
  // Direct >= conjuncts clip the OR unions. Preserve the old exception where the clipped PK set
  // replaces the broader contiguous PK bound.
  assert.equal(selectLegacyScanCandidate(candidates, SELECT_SCAN_BOUND_POLICY)?.kind, "pkSet");
  const indexClipFilter = plannedInventoryFilter(
    db,
    "SELECT id FROM inventory WHERE (a = 1 OR a = 2) AND a >= 0 AND (b = 1 OR b = 2) AND b >= 0",
  );
  const indexClipBound = selectLegacyScanCandidate(
    inventoryScanCandidates(indexClipFilter, rel, internals.readSnap(), db),
    SELECT_SCAN_BOUND_POLICY,
  );
  assert(indexClipBound?.kind === "indexSet");
  assert.equal(indexClipBound.indexSet.nameKey, "a_btree");

  const opclassFilter = plannedInventoryFilter(
    db,
    "SELECT id FROM inventory WHERE tags @> ARRAY[1] AND span && i32range(1, 3)",
  );
  const opclassCandidates = inventoryScanCandidates(opclassFilter, rel, internals.readSnap(), db);
  const selectBound = selectLegacyScanCandidate(opclassCandidates, SELECT_SCAN_BOUND_POLICY);
  assert(selectBound?.kind === "gist");
  assert.equal(selectBound.gist.nameKey, "a_gist");
  const mutationBound = selectLegacyScanCandidate(opclassCandidates, MUTATION_SCAN_BOUND_POLICY);
  assert(mutationBound?.kind === "gin");
  assert.equal(mutationBound.gin.nameKey, "a_gin");
});

function plannedInventoryFilter(db: Engine, sql: string): RExpr {
  const parsed = parseSQL(sql);
  assert.equal(parsed.kind, "select");
  const plan = (db as unknown as PlannerInternals).planSelect(
    parsed as Select,
    null,
    [],
    new ParamTypes(),
  );
  assert(plan.filter !== null);
  return plan.filter;
}
