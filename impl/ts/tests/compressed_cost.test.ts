// Compression cost accrual (spec/design/cost.md §3 "the compression units";
// spec/design/large-values.md §13). valueDecompress joins a scan's up-front block —
// ceil(raw/C) slabs per compressed stored value the bound admits — and valueCompress meters
// every disposition-plan compress ATTEMPT (adopted or rejected) at the INSERT/UPDATE write
// site. The conformance corpus cannot exercise this (its 8 KiB pages never trigger the plan),
// so these tests pin the accrual at page_size 256 (cap C = 240, RECORD_MAX = 114) with
// spill-vs-control table deltas. Mirrors impl/rust/tests/compressed_cost.rs and
// impl/go/compressed_cost_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import type { Session } from "../src/tooling.ts";
import { type Handle, fillerText, queryOutcome } from "./util.ts";
import { memDb } from "./mem_db.ts";

const PAGE_SIZE = 256;
// A 600-byte payload = ceil(600/240) = 3 slabs (compress at write, decompress at scan); a
// 400-byte payload = 2 slabs.
const SLABS_600 = 3n;
const SLABS_400 = 2n;

function smallPageDb(): Session {
  const db = memDb(PAGE_SIZE).session();
  return db;
}

function cost(db: Handle, sql: string): bigint {
  return queryOutcome(db, sql).cost;
}

// `comp` row 1 carries a 600-char "x" run → 0x03 inline-compressed (LZ4 shrinks it far under
// RECORD_MAX, so no chain); `control` is the same shape fully inline-plain. Row 2 is inline in
// both. Same tree shape (one leaf each), so cost deltas isolate the compression units.
function twoTables(): Session {
  const db = smallPageDb();
  db.execute("CREATE TABLE comp (id i32 PRIMARY KEY, body text)");
  db.execute(`INSERT INTO comp VALUES (1, '${"x".repeat(600)}'), (2, 'small')`);
  db.execute("CREATE TABLE control (id i32 PRIMARY KEY, body text)");
  // control row 1 is `plain` (5 chars), not a 4-char `tiny`: it must be at least as long as the
  // `small` probe value the correlated test compares against, so `probe.body = body` charges the
  // SAME varlen_compare (min(5, len) = 5) on both tables — keeping the comp−control delta the pure
  // compression cost, not a length-of-comparison artifact (cost.md §3 "varlen_compare").
  db.execute("INSERT INTO control VALUES (1, 'plain'), (2, 'small')");
  return db;
}

test("scan charges decompress slabs for an inline-compressed value", () => {
  const db = twoTables();
  // Identical plans, rows, and tree shape — the only difference is the ceil(600/240) = 3
  // value_decompress slabs (no chain: the compressed form fits inline, so page_read is equal).
  assert.equal(cost(db, "SELECT * FROM comp"), cost(db, "SELECT * FROM control") + SLABS_600);
});

test("external-compressed charges chain pages plus decompress slabs", () => {
  // A 400-char half-filler/half-run text compresses to ~212 B — smaller than plain but still
  // over RECORD_MAX → 0x04 external-compressed: ceil(212/240) = 1 chain page_read PLUS
  // ceil(400/240) = 2 value_decompress slabs.
  const db = smallPageDb();
  db.execute("CREATE TABLE comp (id i32 PRIMARY KEY, body text)");
  db.execute(`INSERT INTO comp VALUES (1, '${fillerText(200)}${"y".repeat(200)}')`);
  db.execute("CREATE TABLE control (id i32 PRIMARY KEY, body text)");
  db.execute("INSERT INTO control VALUES (1, 'tiny')");
  assert.equal(cost(db, "SELECT * FROM comp"), cost(db, "SELECT * FROM control") + 1n + SLABS_400);
});

test("bounded scan charges only admitted values and LIMIT does not lower", () => {
  const db = twoTables();
  // The point lookup that admits the compressed record pays its slabs ...
  assert.equal(
    cost(db, "SELECT * FROM comp WHERE id = 1"),
    cost(db, "SELECT * FROM control WHERE id = 1") + SLABS_600,
  );
  // ... the one that admits only the inline record pays nothing extra ...
  assert.equal(
    cost(db, "SELECT * FROM comp WHERE id = 2"),
    cost(db, "SELECT * FROM control WHERE id = 2"),
  );
  // ... and LIMIT does not lower the up-front block (cost.md §3 "LIMIT short-circuit").
  assert.equal(
    cost(db, "SELECT * FROM comp LIMIT 1"),
    cost(db, "SELECT * FROM control LIMIT 1") + SLABS_600,
  );
});

test("INSERT meters compress attempts, adopted or rejected", () => {
  const db = smallPageDb();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, body text)");
  // A fully-inline row attempts nothing: INSERT stays zero-cost.
  assert.equal(cost(db, "INSERT INTO t VALUES (1, 'small')"), 0n);
  // An adopted compression (the "x" run) costs its ceil(600/240) = 3 attempt slabs ...
  assert.equal(cost(db, `INSERT INTO t VALUES (2, '${"x".repeat(600)}')`), SLABS_600);
  // ... and a REJECTED attempt (incompressible filler → external-plain) costs the same
  // slabs — the encoder ran either way (cost.md §3).
  assert.equal(cost(db, `INSERT INTO t VALUES (3, '${fillerText(600)}')`), SLABS_600);
});

test("UPDATE meters compress attempts per rewritten row", () => {
  const db = twoTables();
  // Same bounded scan and evals both times; the only delta is the new value's compress
  // attempt: 3 slabs vs 0 (see the Rust mirror for the full reasoning).
  const big = cost(db, `UPDATE comp SET body = '${"x".repeat(600)}' WHERE id = 1`);
  const small = cost(db, "UPDATE comp SET body = 'small' WHERE id = 1");
  assert.equal(big, small + SLABS_600);
});

test("decimal payloads compress too", () => {
  // A long-coefficient decimal's body is a spillable payload like text/bytea
  // (large-values.md §12/§13): 801 digits → 201 base-10⁴ groups → a 407-byte payload,
  // ceil(407/240) = 2 slabs both ways.
  const db = smallPageDb();
  const digits = "12".repeat(400) + ".5";
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, d numeric)");
  assert.equal(
    cost(db, `INSERT INTO t VALUES (1, ${digits})`),
    2n,
    "the compress attempt is metered",
  );
  db.execute("CREATE TABLE control (id i32 PRIMARY KEY, d numeric)");
  db.execute("INSERT INTO control VALUES (1, 7)");
  assert.equal(
    cost(db, "SELECT * FROM t"),
    cost(db, "SELECT * FROM control") + 2n,
    "the decompress slabs are metered",
  );
});

test("untouched compressed columns charge no slabs", () => {
  // The touched set (cost.md §3 "The touched set"): a query that never references the
  // compressed column pays no decompress slabs; an aggregate's ARGUMENT is a touch.
  const db = twoTables();
  assert.equal(cost(db, "SELECT id FROM comp"), cost(db, "SELECT id FROM control"));
  assert.equal(cost(db, "SELECT count(*) FROM comp"), cost(db, "SELECT count(*) FROM control"));
  assert.equal(
    cost(db, "SELECT min(body) FROM comp"),
    cost(db, "SELECT min(body) FROM control") + SLABS_600,
  );
});

test("a correlated outer reference is a touch", () => {
  // A nested subquery's outer reference back into the scanned relation counts as a touch
  // (collected depth-aware — cost.md §3). `probe` holds the one value that matches both
  // tables' row 2, so the two queries emit identical row counts and differ only in the
  // outer table's storage — isolating the SLABS_600 the outer reference charges.
  const db = twoTables();
  db.execute("CREATE TABLE probe (id i32 PRIMARY KEY, body text)");
  db.execute("INSERT INTO probe VALUES (1, 'small')");
  const comp = cost(
    db,
    "SELECT id FROM comp WHERE EXISTS (SELECT 1 FROM probe WHERE probe.body = comp.body)",
  );
  const control = cost(
    db,
    "SELECT id FROM control WHERE EXISTS (SELECT 1 FROM probe WHERE probe.body = control.body)",
  );
  assert.equal(comp, control + SLABS_600);
});
