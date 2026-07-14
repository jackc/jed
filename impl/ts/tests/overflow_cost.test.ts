// Overflow-chain page_read accrual (spec/design/large-values.md §8.1/§12; cost.md §3 "page_read").
// A scan's up-front page_read block counts the B-tree nodes the bound intersects PLUS one per
// overflow chain page of every record the bound admits. The conformance corpus cannot exercise
// this (its tables use the 8 KiB default page, where nothing spills), so these tests pin the
// accrual at page_size 256 by comparing a spilling table against a control table of identical
// shape (same schema, same keys, same row count, one leaf each) whose values stay inline — the
// cost delta is exactly the chain pages. Payloads are incompressible filler so Slice B's
// compress pass rejects them (store-smaller) and they genuinely spill plain — compression's
// own costs are pinned in compressed_cost.test.ts. Mirrored in Rust (tests/overflow_cost.rs) and Go
// (overflow_cost_test.go).

import assert from "node:assert/strict";
import { test } from "node:test";
import type { Session } from "../src/tooling.ts";
import { type Handle, fillerBytesHex, fillerText, queryOutcome } from "./util.ts";
import { memDb } from "./mem_db.ts";

// page_size 256 ⇒ cap = 240, RECORD_MAX = 114. A 600-byte text payload spills into
// ceil(600/240) = 3 overflow pages; a 300-byte bytea into ceil(300/240) = 2.
const PAGE_SIZE = 256;
const TEXT_CHAIN_PAGES = 3n;
const BYTEA_CHAIN_PAGES = 2n;

function smallPageDb(): Session {
  const db = memDb(PAGE_SIZE).session();
  return db;
}

function cost(db: Handle, sql: string): bigint {
  return queryOutcome(db, sql).cost;
}

function streamCost(db: Session, sql: string): bigint {
  const cursor = db.query(sql);
  for (const _ of cursor) void _;
  const result = cursor.cost;
  cursor.close();
  return result;
}

// Two tables of identical shape: `spill` row 1 carries a 600-char text (3-page chain), `control`
// keeps every value inline. Row 2 is inline in both.
function overflowTables(): Session {
  const db = smallPageDb();
  const big = fillerText(600);
  db.execute("CREATE TABLE spill (id i32 PRIMARY KEY, body text)");
  db.execute(`INSERT INTO spill VALUES (1, '${big}'), (2, 'small')`);
  db.execute("CREATE TABLE control (id i32 PRIMARY KEY, body text)");
  db.execute("INSERT INTO control VALUES (1, 'tiny'), (2, 'small')");
  return db;
}

test("a full scan charges the chain pages", () => {
  const db = overflowTables();
  const spill = cost(db, "SELECT * FROM spill");
  const control = cost(db, "SELECT * FROM control");
  // Identical plans, rows, and tree shape — the only difference is the 3-page chain.
  assert.strictEqual(spill, control + TEXT_CHAIN_PAGES);
});

test("a bounded scan charges only the admitted records' chains", () => {
  const db = overflowTables();
  // The point lookup that admits the spilled record pays its chain ...
  const spillHit = cost(db, "SELECT * FROM spill WHERE id = 1");
  const controlHit = cost(db, "SELECT * FROM control WHERE id = 1");
  assert.strictEqual(spillHit, controlHit + TEXT_CHAIN_PAGES);
  assert.strictEqual(streamCost(db, "SELECT * FROM spill WHERE id = 1"), spillHit);
  assert.strictEqual(streamCost(db, "SELECT * FROM control WHERE id = 1"), controlHit);
  // ... the one that admits only the inline record pays nothing extra.
  const spillInline = cost(db, "SELECT * FROM spill WHERE id = 2");
  const controlInline = cost(db, "SELECT * FROM control WHERE id = 2");
  assert.strictEqual(spillInline, controlInline);
});

test("LIMIT does not lower the block", () => {
  // The spilled record is row 2, so LIMIT 1 emits only the inline row 1 — yet the page_read block
  // (which never short-circuits — cost.md §3 "LIMIT short-circuit") still counts the bound's
  // chain pages.
  const db = smallPageDb();
  const big = fillerText(600);
  db.execute("CREATE TABLE spill (id i32 PRIMARY KEY, body text)");
  db.execute(`INSERT INTO spill VALUES (1, 'small'), (2, '${big}')`);
  db.execute("CREATE TABLE control (id i32 PRIMARY KEY, body text)");
  db.execute("INSERT INTO control VALUES (1, 'small'), (2, 'tiny')");
  const spill = cost(db, "SELECT * FROM spill LIMIT 1");
  const control = cost(db, "SELECT * FROM control LIMIT 1");
  assert.strictEqual(spill, control + TEXT_CHAIN_PAGES);
});

test("mutation scans charge only touched chains", () => {
  // A DELETE whose filter READS the spilled column pays its chain (the touched set —
  // cost.md §3); a bare DELETE reads no column, so dropping the rows charges nothing extra.
  const db = overflowTables();
  const spillTouch = cost(db, "DELETE FROM spill WHERE body = 'nope'");
  const controlTouch = cost(db, "DELETE FROM control WHERE body = 'nope'");
  assert.strictEqual(spillTouch, controlTouch + TEXT_CHAIN_PAGES);
  const spillBare = cost(db, "DELETE FROM spill");
  const controlBare = cost(db, "DELETE FROM control");
  assert.strictEqual(spillBare, controlBare);
});

test("untouched columns charge nothing", () => {
  // The touched set (cost.md §3 "The touched set"): a query that never references the spilled
  // column pays neither its chain pages nor anything else for it — the large-values.md §7
  // headline case — while one that does still pays.
  const db = overflowTables();
  // Projection-only touch ...
  assert.strictEqual(cost(db, "SELECT id FROM spill"), cost(db, "SELECT id FROM control"));
  // ... an aggregate touches only its argument (count(*) touches nothing) ...
  assert.strictEqual(
    cost(db, "SELECT count(*) FROM spill"),
    cost(db, "SELECT count(*) FROM control"),
  );
  // ... a WHERE reference is a touch even when only `id` is projected ...
  assert.strictEqual(
    cost(db, "SELECT id FROM spill WHERE body = 'nope'"),
    cost(db, "SELECT id FROM control WHERE body = 'nope'") + TEXT_CHAIN_PAGES,
  );
  // ... and an UPDATE that ASSIGNS the spilled column without reading it (a constant
  // source) skips its chain too — only assignment sources touch, not targets.
  assert.strictEqual(
    cost(db, "UPDATE spill SET body = 'tiny2' WHERE id = 2"),
    cost(db, "UPDATE control SET body = 'tiny2' WHERE id = 2"),
  );
});

test("multiple chains in one record sum", () => {
  // One record with two externalized values charges the sum of both chains: 3 + 2 = 5.
  const db = smallPageDb();
  const bigText = fillerText(600);
  const bigHex = fillerBytesHex(300);
  db.execute("CREATE TABLE spill (id i32 PRIMARY KEY, body text, blob bytea)");
  db.execute(`INSERT INTO spill VALUES (1, '${bigText}', '\\x${bigHex}')`);
  db.execute("CREATE TABLE control (id i32 PRIMARY KEY, body text, blob bytea)");
  db.execute("INSERT INTO control VALUES (1, 'tiny', '\\xcafe')");
  const spill = cost(db, "SELECT * FROM spill");
  const control = cost(db, "SELECT * FROM control");
  assert.strictEqual(spill, control + TEXT_CHAIN_PAGES + BYTEA_CHAIN_PAGES);
});
