// Golden-file cross-core test (CLAUDE.md §8). The load-bearing honesty test for the
// on-disk format: this core must (a) READ a checked-in golden into the expected catalog
// + rows, and (b) WRITE the same logical database to bytes equal to the golden EXACTLY.
// Because the format is deterministic, rust-bytes == go-bytes == golden == ts-bytes, so
// every core reads the others' output. Goldens are authored at page_size 256 by
// spec/fileformat/verify.rb (the independent Ruby reference).

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { Database, execute } from "../src/lib.ts";
import { crc32Ieee, loadDatabase, toImage } from "../src/format.ts";
import { specPath } from "./tomlmini.ts";
import { bytesEqual } from "./util.ts";

const GOLDEN_PAGE_SIZE = 256;

function fixture(name: string): Uint8Array {
  // Copy into a fresh, zero-offset Uint8Array (Node Buffers can be pool-backed slices).
  return new Uint8Array(readFileSync(specPath(`fileformat/fixtures/${name}`)));
}

function run(db: Database, sql: string): void {
  execute(db, sql);
}

// pkTableDB: CREATE TABLE t (id int32 PRIMARY KEY, v int16) with 20 rows (id 3's v is
// NULL) — enough to span more than one data page at page_size 256.
function pkTableDB(): Database {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)");
  for (let i = 1; i <= 20; i++) {
    const v = i === 3 ? "NULL" : `${i * 10}`;
    run(db, `INSERT INTO t VALUES (${i}, ${v})`);
  }
  return db;
}

function oneTableEmptyDB(): Database {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)");
  return db;
}

// nopkTableDB has no primary key — exercises the stored synthetic int64 rowid key.
function nopkTableDB(): Database {
  const db = new Database();
  run(db, "CREATE TABLE r (a int16, b int64)");
  for (const [a, b] of [[7, 70], [8, 80], [9, 90]]) {
    run(db, `INSERT INTO r VALUES (${a}, ${b})`);
  }
  return db;
}

// textTableDB has a text column — exercises the value codec's text branch (u16 length +
// UTF-8 bytes): the empty string, an embedded quote, a 2-byte char (é), a NULL text value,
// and a 4-byte astral char (😀). The PK stays int32 (no text key this slice).
function textTableDB(): Database {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, s text)");
  run(db, "INSERT INTO t VALUES (1, 'alice')");
  run(db, "INSERT INTO t VALUES (2, '')");
  run(db, "INSERT INTO t VALUES (3, 'O''Brien')");
  run(db, "INSERT INTO t VALUES (4, 'café')");
  run(db, "INSERT INTO t VALUES (5, NULL)");
  run(db, "INSERT INTO t VALUES (6, '😀')");
  return db;
}

// boolTableDB has a boolean column — exercises the value codec's boolean branch (a single
// bool-byte, 0x00 false / 0x01 true) plus a NULL boolean. The PK stays int32 (no boolean
// key this slice).
function boolTableDB(): Database {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, flag boolean)");
  run(db, "INSERT INTO t VALUES (1, TRUE)");
  run(db, "INSERT INTO t VALUES (2, FALSE)");
  run(db, "INSERT INTO t VALUES (3, NULL)");
  return db;
}

// WRITE side: serializing the in-memory database reproduces the golden byte-exactly.
test("write matches goldens (byte-identical to Rust/Go/Ruby)", () => {
  const cases: { name: string; build: () => Database }[] = [
    { name: "empty_db.jed", build: () => new Database() },
    { name: "one_table_empty.jed", build: oneTableEmptyDB },
    { name: "pk_table.jed", build: pkTableDB },
    { name: "text_table.jed", build: textTableDB },
    { name: "bool_table.jed", build: boolTableDB },
    { name: "nopk_table.jed", build: nopkTableDB },
  ];
  for (const c of cases) {
    const image = toImage(c.build(), GOLDEN_PAGE_SIZE, 1n);
    const want = fixture(c.name);
    assert.ok(
      bytesEqual(image, want),
      `${c.name}: serialized bytes differ (got ${image.length} B, want ${want.length} B)`,
    );
  }
});

// READ side: loading a golden reproduces the same rows the builder produced. The
// torn-meta goldens must read through the valid slot to the pk_table content.
test("read goldens reproduces rows", () => {
  const cases: { name: string; build: () => Database; table: string }[] = [
    { name: "one_table_empty.jed", build: oneTableEmptyDB, table: "t" },
    { name: "pk_table.jed", build: pkTableDB, table: "t" },
    { name: "text_table.jed", build: textTableDB, table: "t" },
    { name: "bool_table.jed", build: boolTableDB, table: "t" },
    { name: "nopk_table.jed", build: nopkTableDB, table: "r" },
    { name: "torn_meta_slot0.jed", build: pkTableDB, table: "t" },
    { name: "torn_meta_slot1.jed", build: pkTableDB, table: "t" },
  ];
  for (const c of cases) {
    const loaded = loadDatabase(fixture(c.name));
    assert.deepStrictEqual(
      loaded.rowsInKeyOrder(c.table),
      c.build().rowsInKeyOrder(c.table),
      `${c.name}: rows`,
    );
  }
  // Empty database: zero tables, and a missing table reads as absent.
  const empty = loadDatabase(fixture("empty_db.jed"));
  assert.equal(empty.table("t"), undefined, "empty_db should have no tables");
});

// READ side, catalog detail: column names, types, and flags survive exactly.
test("read golden reconstructs catalog", () => {
  const loaded = loadDatabase(fixture("pk_table.jed"));
  const tbl = loaded.table("t");
  assert.ok(tbl, "table t missing");
  assert.equal(tbl!.name, "t");
  assert.equal(tbl!.columns.length, 2);
  const [id, v] = tbl!.columns;
  assert.deepStrictEqual(id, { name: "id", type: "int32", primaryKey: true, notNull: true });
  assert.deepStrictEqual(v, { name: "v", type: "int16", primaryKey: false, notNull: false });
  // A NULL value round-trips (id 3's v).
  const rows = loaded.rowsInKeyOrder("t");
  assert.deepStrictEqual(rows[2], [{ kind: "int", int: 3n }, { kind: "null" }]);
});

// A no-PK table's monotonic rowid counter must be reconstructed on load, so inserts
// after a load don't collide with persisted rowids (the step-6 mutation fix).
test("rowid counter survives load", () => {
  const image = toImage(nopkTableDB(), 8192, 1n); // existing rows take rowids 0, 1, 2
  const loaded = loadDatabase(image);
  // The next insert must get rowid 3, not 0 — otherwise it collides (23505).
  execute(loaded, "INSERT INTO r VALUES (10, 100)");
  assert.equal(loaded.rowsInKeyOrder("r").length, 4);
});

// The default 8 KiB page size also round-trips, and re-serializing is deterministic.
test("round trip at default page size", () => {
  const db = pkTableDB();
  const image = toImage(db, 8192, 1n);
  const loaded = loadDatabase(image);
  assert.deepStrictEqual(loaded.rowsInKeyOrder("t"), db.rowsInKeyOrder("t"));
  assert.ok(bytesEqual(toImage(loaded, 8192, 1n), image), "re-serialized bytes differ");
});

test("crc32 known vector", () => {
  assert.equal(crc32Ieee(new TextEncoder().encode("123456789")), 0xcbf43926);
});

test("serialize is deterministic", () => {
  const db = pkTableDB();
  assert.ok(bytesEqual(toImage(db, 8192, 1n), toImage(db, 8192, 1n)));
});

test("corrupt image is rejected with XX001", () => {
  const image = toImage(pkTableDB(), 8192, 1n);
  image[0] ^= 0xff; // smash slot 0 magic
  image[8192] ^= 0xff; // smash slot 1 magic
  assert.throws(
    () => loadDatabase(image),
    (e: unknown) => e instanceof Error && e.message.startsWith("XX001"),
  );
});
