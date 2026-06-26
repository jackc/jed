// Runtime text → numeric/boolean casts — the parts the PG-clean oracle corpus cannot express (the
// runtime-text-cast slice; spec/design/grammar.md §36, spec/design/types.md §5, spec/types/casts.toml).
// The accepted-grammar int/decimal/boolean cases AGREE with PostgreSQL and are oracle-checked in
// suites/cast/text_to_scalar.test (run on every core); this file covers only what that corpus cannot:
// (a) the jed-stricter grammar DIVERGENCES — hex / digit-underscore / NaN trap 22P02 where PG accepts
// them — and (b) runtime text → f32/f64, kept out of the corpus because the float renderer is in the
// determinism-exception ledger. Every cast is on a NON-LITERAL text column, so it exercises the
// per-row evalCast path, not the resolve-time literal fold. Mirrors impl/rust/tests/cast_text_runtime.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

// Build t(id i32 pk, s text) with one row per string (id = 1..).
function seeded(rows: string[]): ReturnType<typeof dbWith> {
  const stmts = ["CREATE TABLE t (id i32 PRIMARY KEY, s text)"];
  for (const [i, s] of rows.entries()) {
    stmts.push(`INSERT INTO t VALUES (${i + 1}, '${s}')`);
  }
  return dbWith(stmts);
}

const at = (db: ReturnType<typeof dbWith>, expr: string, id: number): string =>
  query(db, `SELECT ${expr} FROM t WHERE id = ${id}`)[0][0];

// --- (a) jed-stricter grammar divergences on the RUNTIME path -------------------------------------

test("runtime text cast uses jed's grammar (hex / underscore / NaN trap 22P02)", () => {
  // Each PG ACCEPTS but jed rejects, so they cannot be oracle-checked — proving the runtime path
  // uses jed's own literal grammar, identical to the resolve-time literal form.
  for (const [s, expr] of [
    ["0x10", "s :: int"], // PG: '0x10'::int4 → 16
    ["1_000", "s :: int"], // PG: '1_000'::int4 → 1000
    ["NaN", "s :: numeric"], // PG: 'NaN'::numeric → NaN (jed decimal is finite)
  ]) {
    const db = seeded([s]);
    assert.equal(
      errCode(() => execute(db, `SELECT ${expr} FROM t WHERE id = 1`)),
      "22P02",
      `${s} :: ${expr}`,
    );
  }
});

// --- (b) runtime text → f32/f64 (out of the corpus: float render is determinism-exempt) ----------

test("runtime text → f64 (finite, incl. scientific notation)", () => {
  const db = seeded(["1.5", "-0.25", "100", "1e3"]);
  assert.equal(at(db, "s :: float8", 1), "1.5");
  assert.equal(at(db, "s :: float8", 2), "-0.25");
  assert.equal(at(db, "s :: float8", 3), "100");
  assert.equal(at(db, "s :: float8", 4), "1000");
});

test("runtime text → f32 (binary32-exact values render cleanly)", () => {
  // 0.5 / 0.25 are exactly representable in binary32, so the cast's fround is a no-op and the
  // render is exact (a non-exact value like 3.14 would render its full binary32 expansion).
  const db = seeded(["0.5", "0.25"]);
  assert.equal(at(db, "s :: float4", 1), "0.5");
  assert.equal(at(db, "s :: float4", 2), "0.25");
});

test("runtime text → float special words (NaN / ±Infinity)", () => {
  const db = seeded(["NaN", "Infinity", "-inf"]);
  assert.equal(at(db, "s :: float8", 1), "NaN");
  assert.equal(at(db, "s :: float8", 2), "Infinity");
  assert.equal(at(db, "s :: float8", 3), "-Infinity");
});

test("runtime text → float overflow (22003) and malformed (22P02)", () => {
  const db = seeded(["1e400", "abc"]);
  // a FINITE literal beyond binary64 range traps 22003 (not ±Inf — the finite-overflow rule)
  assert.equal(
    errCode(() => execute(db, "SELECT s :: float8 FROM t WHERE id = 1")),
    "22003",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT s :: float8 FROM t WHERE id = 2")),
    "22P02",
  );
});

test("runtime text → float NULL propagates", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, s text)",
    "INSERT INTO t VALUES (1, NULL)",
  ]);
  assert.equal(at(db, "s :: float8", 1), "NULL");
});
