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

// goldenDb is an in-memory handle serializing at the golden page size. The page-backed B-tree's
// fan-out tracks the page size (spec/fileformat/format.md), so the in-memory tree must be built at
// the size it will serialize to.
function goldenDb(): Database {
  const db = new Database();
  db.pageSize = GOLDEN_PAGE_SIZE;
  return db;
}

// pkTableDB: CREATE TABLE t (id int32 PRIMARY KEY, v int16) with 20 rows (id 3's v is
// NULL) — enough to span more than one data page at page_size 256.
function pkTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)");
  for (let i = 1; i <= 20; i++) {
    const v = i === 3 ? "NULL" : `${i * 10}`;
    run(db, `INSERT INTO t VALUES (${i}, ${v})`);
  }
  return db;
}

function oneTableEmptyDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)");
  return db;
}

// nopkTableDB has no primary key — exercises the stored synthetic int64 rowid key.
function nopkTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE r (a int16, b int64)");
  for (const [a, b] of [[7, 70], [8, 80], [9, 90]]) {
    run(db, `INSERT INTO r VALUES (${a}, ${b})`);
  }
  return db;
}

// tallTreeDB's wide text padding forces a HEIGHT-2 tree (an interior node whose children are
// themselves interior nodes) at page_size 256 — exercises interior-of-interior child pointers and
// post-order page allocation across a deeper tree (spec/fileformat/format.md).
function tallTreeDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, pad text)");
  for (let i = 1; i <= 18; i++) {
    const pad = `row-${String(i).padStart(2, "0")}-${"x".repeat(48)}`;
    run(db, `INSERT INTO t VALUES (${i}, '${pad}')`);
  }
  return db;
}

// textTableDB has a text column — exercises the value codec's text branch (u16 length +
// UTF-8 bytes): the empty string, an embedded quote, a 2-byte char (é), a NULL text value,
// and a 4-byte astral char (😀). The PK stays int32 (no text key this slice).
function textTableDB(): Database {
  const db = goldenDb();
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
  const db = goldenDb();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, flag boolean)");
  run(db, "INSERT INTO t VALUES (1, TRUE)");
  run(db, "INSERT INTO t VALUES (2, FALSE)");
  run(db, "INSERT INTO t VALUES (3, NULL)");
  return db;
}

// decimalTableDB has a decimal column — exercises the value codec's decimal branch (flags +
// u16 scale + u16 ndigits + base-10^4 groups) and the catalog typmod: an unconstrained numeric
// column `d` and a constrained numeric(10,2) column `m` (values already at scale 2, a no-op
// coercion). Covers positive, negative, zero, a multi-group coefficient, and a NULL.
function decimalTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, d numeric, m numeric(10,2))");
  run(db, "INSERT INTO t VALUES (1, 1.50, 1.50), (2, -12345.6789, -12.34), " +
    "(3, 0.00, 0.00), (4, 100000000.000001, 100.00), (5, NULL, NULL)");
  return db;
}

// byteaTableDB exercises the value codec's bytea branch (u16 length + raw bytes): a multi-
// byte value (a-f hex), the empty byte string, embedded 0x00 bytes, a high byte (0xFF), a
// NULL, and a lone 0x00. The PK stays int32 (no bytea key this slice). Literals are the `\x`
// hex input form, adapting to the bytea column (spec/design/types.md §6).
function byteaTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, b bytea)");
  run(db, "INSERT INTO t VALUES (1, '\\xdeadbeef')");
  run(db, "INSERT INTO t VALUES (2, '\\x')");
  run(db, "INSERT INTO t VALUES (3, '\\x000102')");
  run(db, "INSERT INTO t VALUES (4, '\\xff')");
  run(db, "INSERT INTO t VALUES (5, NULL)");
  run(db, "INSERT INTO t VALUES (6, '\\x00')");
  return db;
}

// uuidTableDB has a uuid PRIMARY KEY (the first golden with a NON-integer stored key — the
// load-bearing §8 cross-core key-path proof) plus a nullable uuid column. Exercises the value
// codec's fixed-16-byte uuid branch (no length prefix), the uuid key encoding (bare 16 bytes),
// a present and a NULL uuid value, and the nil/max boundary UUIDs. Must match the Ruby
// reference's UUID_TABLE (spec/fileformat/verify.rb).
function uuidTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id uuid PRIMARY KEY, ref uuid)");
  run(
    db,
    "INSERT INTO t VALUES " +
      "('00000000-0000-0000-0000-000000000000', '550e8400-e29b-41d4-a716-446655440000'), " +
      "('550e8400-e29b-41d4-a716-446655440000', NULL), " +
      "('f47ac10b-58cc-4372-a567-0e02b2c3d479', '00000000-0000-0000-0000-000000000000'), " +
      "('ffffffff-ffff-ffff-ffff-ffffffffffff', 'ffffffff-ffff-ffff-ffff-ffffffffffff')",
  );
  return db;
}

// defaultTableDB exercises the DEFAULT column constraint on disk — the catalog flags bit2 + the
// pre-evaluated default value (written after the typmod). Covers an int default, a text default,
// a DEFAULT NULL, a NOT NULL column with a default, a decimal default coerced to numeric(6,2),
// and a plain no-default column. Row 1 takes every default; row 2 provides all values.
function defaultTableDB(): Database {
  const db = goldenDb();
  run(
    db,
    "CREATE TABLE t (id int32 PRIMARY KEY, n int32 DEFAULT 0, note text DEFAULT 'none', " +
      "maybe int32 DEFAULT NULL, req int32 NOT NULL DEFAULT 7, amt numeric(6,2) DEFAULT 1.5, plain int16)",
  );
  run(db, "INSERT INTO t (id) VALUES (1)");
  run(db, "INSERT INTO t VALUES (2, 42, 'hi', 5, 9, 2.00, 100)");
  return db;
}

// timestampTableDB exercises the value codec's int64-instant branch (type code 8): a positive
// instant, a pre-1970 negative one, a BC-era one, the ±infinity sentinels, and a NULL. The
// literals parse to the same micros the golden stores. The PK stays int32.
function timestampTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, ts timestamp)");
  run(db, "INSERT INTO t VALUES (1, '2024-01-01 12:00:00')");
  run(db, "INSERT INTO t VALUES (2, '1969-12-31 23:59:59.5')");
  run(db, "INSERT INTO t VALUES (3, '0001-01-01 00:00:00 BC')");
  run(db, "INSERT INTO t VALUES (4, '-infinity')");
  run(db, "INSERT INTO t VALUES (5, 'infinity')");
  run(db, "INSERT INTO t VALUES (6, NULL)");
  return db;
}

// timestamptzTableDB exercises the same 8-byte branch under type code 9; the +05 literal
// normalizes to UTC before storage.
function timestamptzTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, ts timestamptz)");
  run(db, "INSERT INTO t VALUES (1, '2024-01-01 12:00:00+00')");
  run(db, "INSERT INTO t VALUES (2, '2024-01-01 12:00:00+05')");
  run(db, "INSERT INTO t VALUES (3, '1969-12-31 23:59:59.5+00')");
  run(db, "INSERT INTO t VALUES (4, '-infinity')");
  run(db, "INSERT INTO t VALUES (5, 'infinity')");
  run(db, "INSERT INTO t VALUES (6, NULL)");
  return db;
}

// WRITE side: serializing the in-memory database reproduces the golden byte-exactly.
test("write matches goldens (byte-identical to Rust/Go/Ruby)", () => {
  const cases: { name: string; build: () => Database }[] = [
    { name: "empty_db.jed", build: () => goldenDb() },
    { name: "one_table_empty.jed", build: oneTableEmptyDB },
    { name: "pk_table.jed", build: pkTableDB },
    { name: "text_table.jed", build: textTableDB },
    { name: "bool_table.jed", build: boolTableDB },
    { name: "decimal_table.jed", build: decimalTableDB },
    { name: "bytea_table.jed", build: byteaTableDB },
    { name: "uuid_table.jed", build: uuidTableDB },
    { name: "default_table.jed", build: defaultTableDB },
    { name: "timestamp_table.jed", build: timestampTableDB },
    { name: "timestamptz_table.jed", build: timestamptzTableDB },
    { name: "nopk_table.jed", build: nopkTableDB },
    { name: "tall_tree.jed", build: tallTreeDB },
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
    { name: "decimal_table.jed", build: decimalTableDB, table: "t" },
    { name: "bytea_table.jed", build: byteaTableDB, table: "t" },
    { name: "uuid_table.jed", build: uuidTableDB, table: "t" },
    { name: "default_table.jed", build: defaultTableDB, table: "t" },
    { name: "timestamp_table.jed", build: timestampTableDB, table: "t" },
    { name: "timestamptz_table.jed", build: timestamptzTableDB, table: "t" },
    { name: "nopk_table.jed", build: nopkTableDB, table: "r" },
    { name: "tall_tree.jed", build: tallTreeDB, table: "t" },
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
  assert.deepStrictEqual(id, { name: "id", type: "int32", decimal: null, primaryKey: true, notNull: true, default: null });
  assert.deepStrictEqual(v, { name: "v", type: "int16", decimal: null, primaryKey: false, notNull: false, default: null });
  // A NULL value round-trips (id 3's v).
  const rows = loaded.rowsInKeyOrder("t");
  assert.deepStrictEqual(rows[2], [{ kind: "int", int: 3n }, { kind: "null" }]);
});

// A no-PK table's monotonic rowid counter must be reconstructed on load, so inserts
// after a load don't collide with persisted rowids (the step-6 mutation fix).
test("rowid counter survives load", () => {
  const image = toImage(nopkTableDB(), GOLDEN_PAGE_SIZE, 1n); // existing rows take rowids 0, 1, 2
  const loaded = loadDatabase(image);
  // The next insert must get rowid 3, not 0 — otherwise it collides (23505).
  execute(loaded, "INSERT INTO r VALUES (10, 100)");
  assert.equal(loaded.rowsInKeyOrder("r").length, 4);
});

// A column DEFAULT survives serialize→load: a fresh INSERT omitting the defaulted columns
// applies the *persisted* defaults — proving the default value (not just its byte length)
// round-trips through the catalog (constraints.md §2).
test("default survives load", () => {
  const loaded = loadDatabase(fixture("default_table.jed"));
  run(loaded, "INSERT INTO t (id) VALUES (3)");
  const rows = loaded.rowsInKeyOrder("t")!;
  const last = rows[rows.length - 1]!;
  // id=3 takes every persisted default: n=0, note='none', maybe=NULL, req=7, plain=NULL.
  assert.deepStrictEqual(last[0], { kind: "int", int: 3n });
  assert.deepStrictEqual(last[1], { kind: "int", int: 0n });
  assert.deepStrictEqual(last[2], { kind: "text", text: "none" });
  assert.deepStrictEqual(last[3], { kind: "null" });
  assert.deepStrictEqual(last[4], { kind: "int", int: 7n });
  assert.deepStrictEqual(last[6], { kind: "null" });
});

// The default 8 KiB page size also round-trips, and re-serializing is deterministic. Built at 8192
// so the in-memory tree is sized for it (fan-out tracks the page size — format.md).
test("round trip at default page size", () => {
  const db = new Database();
  db.pageSize = 8192;
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)");
  for (let i = 1; i <= 20; i++) {
    const v = i === 3 ? "NULL" : `${i * 10}`;
    run(db, `INSERT INTO t VALUES (${i}, ${v})`);
  }
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
  assert.ok(bytesEqual(toImage(db, GOLDEN_PAGE_SIZE, 1n), toImage(db, GOLDEN_PAGE_SIZE, 1n)));
});

test("corrupt image is rejected with XX001", () => {
  const image = toImage(pkTableDB(), GOLDEN_PAGE_SIZE, 1n);
  image[0] ^= 0xff; // smash slot 0 magic
  image[GOLDEN_PAGE_SIZE] ^= 0xff; // smash slot 1 magic
  assert.throws(
    () => loadDatabase(image),
    (e: unknown) => e instanceof Error && e.message.startsWith("XX001"),
  );
});
