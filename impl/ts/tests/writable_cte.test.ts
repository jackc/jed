// Data-modifying (writable) CTEs (spec/design/writable-cte.md) — the per-core slice that the
// PostgreSQL-clean conformance corpus (cte/data_modifying.test, cte/with_dml.test,
// cte/data_modifying_errors.test) cannot express: the COMMAND TAG of a data-modifying primary (the
// statement-outcome affected-row count, which the corpus's `statement ok` does not assert), and
// jed's DETERMINISTIC last-write-wins resolution of an update/update or update/delete of the SAME
// row — a documented divergence on a case PostgreSQL leaves unspecified (§7). Mirrored in
// impl/rust/tests/writable_cte.rs and impl/go/writable_cte_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Engine, execute, render } from "../src/lib.ts";

function exec(db: Engine, sql: string): void {
  execute(db, sql);
}

// rows runs sql (which must yield a query result) and returns its rows rendered as strings.
function rows(db: Engine, sql: string): string[][] {
  const o = execute(db, sql);
  if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
  return o.rows.map((r) => r.map(render));
}

// affected runs sql (which must yield a statement result) and returns its affected-row count.
function affected(db: Engine, sql: string): number | null {
  const o = execute(db, sql);
  if (o.kind !== "statement") throw new Error(`expected a statement result for ${sql}`);
  return o.rowsAffected;
}

// i32s renders a single-column integer result to a sorted array of numbers.
function i32s(rs: string[][]): number[] {
  return rs.map((r) => Number(r[0])).sort((a, b) => a - b);
}

function setup(): Engine {
  const db = new Engine();
  exec(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  exec(db, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)");
  return db;
}

// --- the command tag of a data-modifying primary (the result is the PRIMARY's, §4) ------------

test("WITH on INSERT primary without RETURNING reports the affected count", () => {
  const db = setup();
  exec(db, "CREATE TABLE dst (x i32)");
  // A WITH feeding an INSERT primary with no RETURNING is a STATEMENT whose count is the primary's
  // inserted-row count (a CTE's own count is never surfaced — §4).
  const n = affected(
    db,
    "WITH src AS (SELECT id FROM t WHERE id <= 2) INSERT INTO dst SELECT id FROM src",
  );
  assert.strictEqual(n, 2);
  assert.deepStrictEqual(i32s(rows(db, "SELECT x FROM dst")), [1, 2]);
});

test("WITH on DELETE primary without RETURNING reports the affected count", () => {
  const db = setup();
  const n = affected(
    db,
    "WITH old AS (SELECT id FROM t WHERE id >= 2) DELETE FROM t WHERE id IN (SELECT id FROM old)",
  );
  assert.strictEqual(n, 2);
  assert.deepStrictEqual(i32s(rows(db, "SELECT id FROM t")), [1]);
});

test("WITH on UPDATE primary without RETURNING reports the affected count", () => {
  const db = setup();
  const n = affected(
    db,
    "WITH hi AS (SELECT id FROM t WHERE v >= 20) UPDATE t SET v = v + 1 WHERE id IN (SELECT id FROM hi)",
  );
  assert.strictEqual(n, 2);
});

test("a data-modifying CTE's count is not surfaced under a SELECT primary", () => {
  const db = setup();
  // The data-modifying CTE inserts 1 row, but the SELECT primary's result is what is returned — and
  // it reads the PRE-statement table (the pin, §2), so count is 3, not 4.
  const r = rows(
    db,
    "WITH ins AS (INSERT INTO t VALUES (4, 40) RETURNING *) SELECT count(*) FROM t",
  );
  assert.deepStrictEqual(r, [["3"]]);
  // ...and the insert still landed (always to completion, §3).
  assert.deepStrictEqual(rows(db, "SELECT count(*) FROM t"), [["4"]]);
});

// --- jed's deterministic last-write-wins on a same-row conflict (PG-unspecified, §7) ----------

test("two updates of the same row: last-write-wins, both RETURN from the pin", () => {
  const db = setup();
  // Two CTEs update id=1. Each reads the PIN (pre-statement v=10) and returns its own new value, so
  // BOTH return a row; the writes apply in lexical order, last-write-wins, so the table ends at the
  // SECOND CTE's value. PostgreSQL applies and returns only ONE (unspecified which) — the documented
  // divergence.
  const r = i32s(
    rows(
      db,
      `WITH a AS (UPDATE t SET v = 100 WHERE id = 1 RETURNING v),
            b AS (UPDATE t SET v = 200 WHERE id = 1 RETURNING v)
       SELECT v FROM a UNION ALL SELECT v FROM b`,
    ),
  );
  assert.deepStrictEqual(r, [100, 200], "both updates compute RETURNING from the pin");
  // The committed value is the second (lexically later) write.
  assert.deepStrictEqual(rows(db, "SELECT v FROM t WHERE id = 1"), [["200"]]);
});

test("update then delete of the same row: delete wins", () => {
  // CTE a updates id=1 to 100; CTE b deletes id=1. Both read the pin (the pre-statement row), so a
  // returns 100 and b returns the pre-statement old value 10; b's delete applies after a's update, so
  // the row is gone at the end (delete wins).
  let db = setup();
  const upd = i32s(
    rows(db, "WITH a AS (UPDATE t SET v = 100 WHERE id = 1 RETURNING v) SELECT v FROM a"),
  );
  assert.deepStrictEqual(upd, [100]);
  // Reset and run the combined conflict.
  db = setup();
  const r = i32s(
    rows(
      db,
      `WITH a AS (UPDATE t SET v = 100 WHERE id = 1 RETURNING v),
            b AS (DELETE FROM t WHERE id = 1 RETURNING v)
       SELECT v FROM a UNION ALL SELECT v FROM b`,
    ),
  );
  assert.deepStrictEqual(r, [10, 100], "a returns the new value, b the pre-statement old value");
  // id=1 is gone (the delete applied last).
  assert.deepStrictEqual(i32s(rows(db, "SELECT id FROM t")), [2, 3]);
});
