// A1 — touched-column scan wiring (packed-leaf.md §4/§11; the PAX read-path dividend). A file-backed
// SELECT feed reconstructs only the query's touched columns (relMasks), leaving untouched columns NULL
// on the Packed leaf, instead of decoding the whole row. This is byte/result/cost-neutral IFF the mask
// is a complete superset of every column any consumer reads — an invariant already load-bearing for
// deferred VARIABLE-LENGTH values (an untouched unfetched value poisons if read,
// tests/lazy_inline_values.test.ts) but NEWLY load-bearing for FIXED-WIDTH columns (previously always
// decoded, so a mask gap was harmless). This battery actively exercises that: a WIDE ALL-FIXED-WIDTH
// table and a spread of query shapes each touching a different column subset, where a paged reopen
// (masked reconstruction) and a fully-resident in-memory database (whole rows) must agree on both rows
// and cost. A mask gap surfaces as a divergence here, never a silent wrong answer. Mirrors
// impl/rust/tests/masked_scan.rs and impl/go/masked_scan_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { Database, createDatabase, openDatabase } from "../src/tooling.ts";
import { render } from "../src/value.ts";
import type { Handle } from "./util.ts";

// A wide all-fixed-width table (i16/i32/i64, several nullable) plus a secondary index and a join
// partner. Every column is fixed-width, so on a paged reopen the leaf is Packed with no deferred values —
// the case rowAtMasked skips whole-column decodes that rowAt would have done.
function seed(db: Handle): void {
  db.execute(
    "CREATE TABLE w (id i32 PRIMARY KEY, c0 i16, c1 i32, c2 i64, c3 i32, c4 i16, c5 i64, c6 i32, c7 i32)",
  );
  db.execute(
    "INSERT INTO w VALUES " +
      "(1, 10, 100, 1000, 7, 3, 500, 42, 9), " +
      "(2, 20, 100, 2000, 7, NULL, 600, 43, 8), " +
      "(3, 10, 300, 3000, 8, 5, NULL, 44, 7), " +
      "(4, 20, 100, 4000, 8, 6, 800, NULL, 6), " +
      "(5, 10, 500, 5000, 9, NULL, 900, 46, 5)",
  );
  db.execute("CREATE INDEX w_c3 ON w (c3)");
  db.execute("CREATE TABLE w2 (id i32 PRIMARY KEY, k i32, note i32)");
  db.execute("INSERT INTO w2 VALUES (1, 7, 71), (2, 8, 82), (3, 7, 73), (5, 9, 95)");
}

// rowsSorted runs sql via execute() and returns its rows rendered to strings and sorted — an
// order-insensitive multiset compare (a query without ORDER BY has unspecified order; sorting both
// sides is sound for equality either way).
function rowsSorted(db: Handle, sql: string): string[] {
  const o = db.execute(sql);
  if (o.kind !== "query") throw new Error(`expected a query: ${sql}`);
  return o.rows.map((r) => r.map((v) => render(v)).join("\x1f")).sort();
}

function costOf(db: Handle, sql: string): bigint {
  return db.execute(sql).cost;
}

test("paged masked scan matches resident across query shapes", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-masked-wide-"));
  try {
    const path = join(dir, "wide.jed");
    const filedb = createDatabase(path, {});
    seed(filedb);
    filedb.close();

    const mem = Database.newInMemory().session();
    seed(mem);
    const paged = openDatabase(path);

    const queries = [
      // Whole-row and single/multi-column projections.
      "SELECT * FROM w",
      "SELECT c0 FROM w",
      "SELECT c3, c7 FROM w",
      "SELECT id, c5 FROM w",
      // WHERE on one column, project another (touched set spans filter + projection).
      "SELECT c1 FROM w WHERE c0 > 15",
      "SELECT id FROM w WHERE c7 < 8",
      "SELECT c6 FROM w WHERE c4 IS NULL",
      "SELECT c2 FROM w WHERE c5 IS NOT NULL",
      "SELECT c1 FROM w WHERE c0 > 5 AND c7 < 9", // AND predicate
      "SELECT c0, c6 FROM w WHERE c7 = 5 OR c7 = 8", // OR predicate, multi-column projection
      "SELECT c0 FROM w WHERE c0 > 1000", // zero survivors
      "SELECT id FROM w WHERE id > 0", // every row survives
      // Aggregates touching one operand column (row path both ways; masked reconstruction still applies).
      "SELECT count(*) FROM w",
      "SELECT sum(c2) FROM w",
      "SELECT sum(c1) FROM w",
      "SELECT sum(c3), count(c6) FROM w",
      "SELECT count(c4) FROM w", // COUNT over a nullable operand
      "SELECT min(c5), max(c6) FROM w",
      "SELECT sum(c0) FROM w WHERE c1 = 100", // filtered agg
      "SELECT count(*) FROM w WHERE c7 < 8", // filtered COUNT(*)
      "SELECT min(c5), max(c6) FROM w WHERE c4 IS NOT NULL", // filtered MIN/MAX over a nullable operand
      "SELECT sum(c1) FROM w WHERE c0 > 1000", // filter admits no rows
      // Single-integer-key GROUP BY (touched: the key + the operand).
      "SELECT c0, sum(c2) FROM w GROUP BY c0",
      "SELECT c0, sum(c1), count(c4) FROM w GROUP BY c0", // grouped multi-spec, nullable operand
      "SELECT c3, count(*) FROM w GROUP BY c3",
      "SELECT c0, sum(c1) FROM w WHERE c7 > 5 GROUP BY c0", // filtered grouped
      // ORDER BY satisfied by the PK scan (top-N streaming) and by a sort (non-PK).
      "SELECT c1 FROM w ORDER BY id",
      "SELECT c1 FROM w ORDER BY id LIMIT 3",
      "SELECT c6 FROM w ORDER BY c6 DESC",
      "SELECT id, c0 FROM w ORDER BY c0, id",
      // DISTINCT.
      "SELECT DISTINCT c0 FROM w",
      "SELECT DISTINCT c3, c0 FROM w",
      // PK point + range bounds (the rangeScanWithUnitsMasked feed).
      "SELECT c4 FROM w WHERE id = 2",
      "SELECT c2, c6 FROM w WHERE id >= 3",
      // Secondary-index bound (indexBoundRows — whole-row, must still agree).
      "SELECT c0 FROM w WHERE c3 = 7",
      // Join (each rel materialized under its own mask).
      "SELECT w.c0, w2.note FROM w JOIN w2 ON w2.id = w.id",
      "SELECT w.c1 FROM w JOIN w2 ON w2.k = w.c3 WHERE w2.note > 72",
      // Subquery / IN (the inner and outer each touch distinct columns).
      "SELECT c0 FROM w WHERE id IN (SELECT id FROM w2 WHERE k = 7)",
      "SELECT c7 FROM w WHERE EXISTS (SELECT 1 FROM w2 WHERE w2.id = w.id AND w2.note > 80)",
    ];
    for (const sql of queries) {
      assert.deepEqual(
        rowsSorted(mem, sql),
        rowsSorted(paged, sql),
        `rows differ (paged-masked vs resident): ${sql}`,
      );
      assert.equal(
        costOf(mem, sql),
        costOf(paged, sql),
        `cost differs (paged-masked vs resident): ${sql}`,
      );
    }
    paged.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
