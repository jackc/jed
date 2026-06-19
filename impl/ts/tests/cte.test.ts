// Common table expressions — `WITH name [(cols)] AS [NOT] MATERIALIZED (query) [, …] <query>`,
// non-recursive (spec/design/cte.md). The row/name/error assertions and the inline/materialize
// cost contract live in the shared conformance corpus (spec/conformance/suites/cte/*.test). What
// remains here is the MATERIALIZED / NOT MATERIALIZED hint cost split (13/23), which the corpus
// pins by rows but NOT by cost.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute } from "../src/lib.ts";
import { dbWith } from "./util.ts";

// A 3-row, single-node table t(id, n) = {(1,10),(2,20),(3,30)}.
function t3(): Database {
  return dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, n int32)",
    "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
  ]);
}

function cost(db: Database, sql: string): bigint {
  return execute(db, sql).cost;
}

test("MATERIALIZED / NOT MATERIALIZED hints force the mode", () => {
  const db = t3();
  // MATERIALIZED forces a single-reference CTE to buffer: body once (7) + 3 cte_scan_row + 3
  // row_produced = 13 (vs the inlined 10).
  assert.strictEqual(
    cost(db, "WITH c AS MATERIALIZED (SELECT id FROM t) SELECT id FROM c ORDER BY id"),
    13n,
  );
  // NOT MATERIALIZED forces a two-reference CTE to inline (each reference re-runs the body): two
  // bodies (2x7) + 9 row_produced = 23 (vs the materialized 22).
  assert.strictEqual(
    cost(db, "WITH c AS NOT MATERIALIZED (SELECT id FROM t) SELECT a.id, b.id FROM c a CROSS JOIN c b"),
    23n,
  );
});
