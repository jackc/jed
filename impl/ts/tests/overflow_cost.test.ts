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
import { Database, execute } from "../src/lib.ts";
import { fillerBytesHex, fillerText } from "./util.ts";

// page_size 256 ⇒ cap = 244, RECORD_MAX = 116. A 600-byte text payload spills into
// ceil(600/244) = 3 overflow pages; a 300-byte bytea into ceil(300/244) = 2.
const PAGE_SIZE = 256;
const TEXT_CHAIN_PAGES = 3n;
const BYTEA_CHAIN_PAGES = 2n;

function smallPageDb(): Database {
  const db = new Database();
  db.pageSize = PAGE_SIZE;
  return db;
}

function cost(db: Database, sql: string): bigint {
  return execute(db, sql).cost;
}

// Two tables of identical shape: `spill` row 1 carries a 600-char text (3-page chain), `control`
// keeps every value inline. Row 2 is inline in both.
function overflowTables(): Database {
  const db = smallPageDb();
  const big = fillerText(600);
  execute(db, "CREATE TABLE spill (id int32 PRIMARY KEY, body text)");
  execute(db, `INSERT INTO spill VALUES (1, '${big}'), (2, 'small')`);
  execute(db, "CREATE TABLE control (id int32 PRIMARY KEY, body text)");
  execute(db, "INSERT INTO control VALUES (1, 'tiny'), (2, 'small')");
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
  execute(db, "CREATE TABLE spill (id int32 PRIMARY KEY, body text)");
  execute(db, `INSERT INTO spill VALUES (1, 'small'), (2, '${big}')`);
  execute(db, "CREATE TABLE control (id int32 PRIMARY KEY, body text)");
  execute(db, "INSERT INTO control VALUES (1, 'small'), (2, 'tiny')");
  const spill = cost(db, "SELECT * FROM spill LIMIT 1");
  const control = cost(db, "SELECT * FROM control LIMIT 1");
  assert.strictEqual(spill, control + TEXT_CHAIN_PAGES);
});

test("mutation scans charge the chain pages", () => {
  const db = overflowTables();
  const spill = cost(db, "DELETE FROM spill");
  const control = cost(db, "DELETE FROM control");
  assert.strictEqual(spill, control + TEXT_CHAIN_PAGES);
});

test("multiple chains in one record sum", () => {
  // One record with two externalized values charges the sum of both chains: 3 + 2 = 5.
  const db = smallPageDb();
  const bigText = fillerText(600);
  const bigHex = fillerBytesHex(300);
  execute(db, "CREATE TABLE spill (id int32 PRIMARY KEY, body text, blob bytea)");
  execute(db, `INSERT INTO spill VALUES (1, '${bigText}', '\\x${bigHex}')`);
  execute(db, "CREATE TABLE control (id int32 PRIMARY KEY, body text, blob bytea)");
  execute(db, "INSERT INTO control VALUES (1, 'tiny', '\\xcafe')");
  const spill = cost(db, "SELECT * FROM spill");
  const control = cost(db, "SELECT * FROM control");
  assert.strictEqual(spill, control + TEXT_CHAIN_PAGES + BYTEA_CHAIN_PAGES);
});
