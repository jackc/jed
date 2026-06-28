// External merge sort with spill-to-disk for ORDER BY (spec/design/spill.md). Spill is NOT a §8
// byte contract (it changes WHEN rows are resident, never WHAT a query observes — like the buffer
// pool), so it is verified per-core, not in the conformance corpus: a file-backed database sorting
// under a tiny workMem (which forces many sorted runs to spill + a k-way merge) must return
// byte-identical rows and cost to the same query run fully in memory. These tests pin that invariance
// across several ORDER BY shapes, the stable-sort tie-break the merge must reproduce, and that no
// spill temp file leaks. Files live under a fresh mkdtemp dir, never the repo tree.

import assert from "node:assert/strict";
import { mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { create, Engine, execute } from "../src/tooling.ts";
import type { Value } from "../src/lib.ts";

function runQuery(db: Engine, sql: string): { rows: Value[][]; cost: bigint } {
  const out = execute(db, sql);
  if (out.kind !== "query") throw new Error(`not a query: ${sql}`);
  return { rows: out.rows, cost: out.cost };
}

// seedSpill populates t(id i32 PK, k i32, s text) with n rows whose k is deliberately unsorted
// and has many duplicates + a repeating NULL (to exercise the stable-sort tie-break and NULL
// ordering), and a variable-length s (so a spilled run carries variable-width values).
function seedSpill(db: Engine, n: number): void {
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, k i32, s text)");
  for (let id = 0; id < n; id++) {
    const k = id % 7 === 0 ? "NULL" : String((id * 48271) % 100);
    const s = "x".repeat(id % 17);
    execute(db, `INSERT INTO t VALUES (${id}, ${k}, '${s}')`);
  }
}

const SHAPES = [
  "SELECT id, k FROM t ORDER BY k, id",
  "SELECT id, k FROM t ORDER BY k DESC, id DESC",
  "SELECT k, id FROM t ORDER BY k NULLS FIRST, id",
  "SELECT id FROM t ORDER BY k, id LIMIT 13",
  "SELECT id FROM t ORDER BY k, id LIMIT 13 OFFSET 9",
  "SELECT id, s FROM t WHERE k > 20 ORDER BY s, id",
  "SELECT id FROM t ORDER BY k, id OFFSET 195",
];

// valueEqual is a NULL-safe, value-canonical equality (NULL == NULL; decimals by value), so the
// spilling and in-memory results compare exactly even across NULLs.
function valueEqual(a: Value, b: Value): boolean {
  if (a.kind !== b.kind) return false;
  switch (a.kind) {
    case "null":
      return true;
    case "int":
      return a.int === (b as { int: bigint }).int;
    case "bool":
      return a.value === (b as { value: boolean }).value;
    case "text":
      return a.text === (b as { text: string }).text;
    case "timestamp":
    case "timestamptz":
      return a.micros === (b as { micros: bigint }).micros;
    case "decimal":
      return a.dec.cmpValue((b as { dec: typeof a.dec }).dec) === 0;
    default:
      return false;
  }
}

function rowsEqual(a: Value[][], b: Value[][]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i]!.length !== b[i]!.length) return false;
    for (let j = 0; j < a[i]!.length; j++) {
      if (!valueEqual(a[i]![j]!, b[i]![j]!)) return false;
    }
  }
  return true;
}

test("spilling sort matches the in-memory rows and cost", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-spill-match-"));
  try {
    // The source of truth: the same data + queries against a pure in-memory database, which never
    // spills (spill.md §2).
    const mem = new Engine();
    seedSpill(mem, 200);

    // A file-backed database with a tiny workMem so every shape spills many runs and k-way-merges.
    const db = create(join(dir, "spill_match.jed"), {});
    seedSpill(db, 200);
    db.setWorkMem(128); // ~2-3 rows per run → dozens of runs, deep merge

    for (const sql of SHAPES) {
      const want = runQuery(mem, sql);
      const got = runQuery(db, sql);
      assert.ok(rowsEqual(got.rows, want.rows), `rows diverged under spill for: ${sql}`);
      assert.equal(got.cost, want.cost, `cost diverged under spill for: ${sql}`);
    }

    // The same file-backed database with spill DISABLED (workMem 0 = unlimited) must also match.
    db.setWorkMem(0);
    for (const sql of SHAPES) {
      const want = runQuery(mem, sql);
      const got = runQuery(db, sql);
      assert.ok(rowsEqual(got.rows, want.rows), `rows diverged with spill off for: ${sql}`);
      assert.equal(got.cost, want.cost, `cost diverged with spill off for: ${sql}`);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("spill leaves no temp files", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-spill-clean-"));
  try {
    const db = create(join(dir, "spill_cleanup.jed"), {});
    seedSpill(db, 150);
    db.setWorkMem(64); // force heavy spilling

    runQuery(db, "SELECT id FROM t ORDER BY k, id");
    runQuery(db, "SELECT id FROM t ORDER BY k, id LIMIT 3");
    const leaked = readdirSync(dir).filter((n) => n.startsWith("jed-spill-"));
    assert.equal(leaked.length, 0, `spill run files leaked: ${leaked.join(", ")}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("spilling sort is stable on ties", () => {
  // Every row shares the same key, so the whole result is one big tie: a stable sort keeps the scan
  // order (primary key = id ascending). The external sort reproduces it only if the merge tie-breaks
  // by (run, position) = input order (spill.md §6).
  const dir = mkdtempSync(join(tmpdir(), "jed-spill-stable-"));
  try {
    const db = create(join(dir, "spill_stable.jed"), {});
    execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, k i32)");
    for (let id = 0; id < 100; id++) execute(db, `INSERT INTO t VALUES (${id}, 5)`);
    db.setWorkMem(96); // force spilling so the merge tie-break is exercised

    const { rows } = runQuery(db, "SELECT id FROM t ORDER BY k");
    for (let i = 0; i < 100; i++) {
      const v = rows[i]![0]!;
      assert.ok(v.kind === "int" && v.int === BigInt(i), `row ${i}: expected id ${i}`);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
