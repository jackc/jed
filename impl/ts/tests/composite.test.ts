// Composite (row) types — CREATE/DROP TYPE, the catalog type registry, on-disk persistence, and
// (S3) storable composite columns: the ROW(…) constructor, the recursive value codec, the
// INSERT/SELECT round-trip, and record_out rendering (spec/design/composite.md). Mirrors
// impl/rust/tests/composite.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute, loadDatabase, toImage } from "../src/lib.ts";
import { arrayT, compositeT, scalarT } from "../src/types.ts";
import { errCode, query } from "./util.ts";

function run(db: Database, sql: string): void {
  execute(db, sql);
}

test("CREATE TYPE registers fields", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text NOT NULL, zip i32)");
  const ct = db.compositeType("addr");
  assert.ok(ct, "type addr");
  assert.equal(ct!.name, "addr");
  assert.equal(ct!.fields.length, 2);
  assert.equal(ct!.fields[0]!.name, "street");
  assert.deepStrictEqual(ct!.fields[0]!.type, scalarT("text"));
  assert.equal(ct!.fields[0]!.notNull, true);
  assert.equal(ct!.fields[1]!.name, "zip");
  assert.equal(ct!.fields[1]!.notNull, false);
  // Case-insensitive lookup.
  assert.ok(db.compositeType("ADDR"), "ADDR resolves case-insensitively");
});

test("DROP TYPE removes it", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (a i32)");
  run(db, "DROP TYPE addr");
  assert.equal(db.compositeType("addr"), undefined);
});

// A nested composite value round-trips and renders with the inner record quoted.
test("nested composite value round-trip", () => {
  const db = new Database();
  run(db, "CREATE TYPE point AS (x i32, y i32)");
  run(db, "CREATE TYPE seg AS (a point, b point)");
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, s seg)");
  run(db, "INSERT INTO t VALUES (1, ROW(ROW(1, 2), ROW(3, 4)))");
  assert.deepStrictEqual(query(db, "SELECT s FROM t"), [['("(1,2)","(3,4)")']]);
});

// Composite values survive a serialize → load round-trip (the v9 recursive value codec).
test("composite values persist through the on-disk image", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip i32)");
  run(db, "CREATE TABLE p (id i32 PRIMARY KEY, home addr)");
  run(db, "INSERT INTO p VALUES (1, ROW('Main', 90210))");
  run(db, "INSERT INTO p VALUES (2, ROW('Oak', NULL))");
  const image = toImage(db, 256, 1n);
  const loaded = loadDatabase(image);
  assert.deepStrictEqual(query(loaded, "SELECT id, home FROM p ORDER BY id"), [
    ["1", "(Main,90210)"],
    ["2", "(Oak,)"],
  ]);
});

test("DROP TYPE ... CASCADE is 0A000", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (a i32)");
  assert.equal(errCode(() => run(db, "DROP TYPE addr CASCADE")), "0A000");
});

test("nested type self- or forward-reference is 42704", () => {
  const db = new Database();
  // Forward reference (point not yet defined) — and self-reference — are unknown types.
  assert.equal(errCode(() => run(db, "CREATE TYPE line AS (a point)")), "42704");
  assert.equal(errCode(() => run(db, "CREATE TYPE t AS (a t)")), "42704");
});

// Round-trip through the on-disk image: a composite type (and a nested one) survives serialize →
// load, byte-backed by the v9 catalog type-definition section.
test("types persist through the on-disk image", () => {
  const db = new Database();
  run(db, "CREATE TYPE point AS (x i32 NOT NULL, y i32 NOT NULL)");
  run(db, "CREATE TYPE line AS (a point, b point)");
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, n i32)");
  run(db, "INSERT INTO t VALUES (1, 10)");

  const image = toImage(db, 256, 1n);
  const loaded = loadDatabase(image);

  const point = loaded.compositeType("point");
  assert.ok(point, "point persists");
  assert.equal(point!.fields.length, 2);
  assert.equal(point!.fields[0]!.notNull, true);

  const line = loaded.compositeType("line");
  assert.ok(line, "line persists");
  assert.equal(line!.fields.length, 2);
  // A nested field references its composite by name.
  assert.deepStrictEqual(line!.fields[0]!.type, compositeT("point"));
  // The table and its row survive too.
  assert.equal(loaded.table("t")!.columns.length, 2);
});

// S4: `(expr).field` selects one field; the output column is named after the field. Works on a
// parenthesized column, a ROW(…) literal, and chains through a nested composite.
test("field access selects a field", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip i32)");
  run(db, "CREATE TABLE person (id i32 PRIMARY KEY, home addr)");
  run(db, "INSERT INTO person VALUES (1, ROW('Main', 90210))");
  // Parenthesized-column field access.
  assert.deepStrictEqual(query(db, "SELECT (home).zip, (home).street FROM person"), [["90210", "Main"]]);
  // Field access on an anonymous ROW(…) literal (fields named f1, f2, …), no FROM.
  assert.deepStrictEqual(query(db, "SELECT (ROW('x', 7)).f2"), [["7"]]);
});

// S5: composite equality is element-wise 3VL (PG row comparison). `=` is FALSE if any field is
// FALSE; else UNKNOWN if any field is UNKNOWN; else TRUE.
test("composite equality 3VL", () => {
  const db = new Database();
  run(db, "CREATE TYPE rec AS (a i32, b i32)");
  // Equal rows.
  assert.deepStrictEqual(query(db, "SELECT ROW(1, 2) = ROW(1, 2)"), [["true"]]);
  // A NULL field with all-else-equal → UNKNOWN (renders NULL).
  assert.deepStrictEqual(query(db, "SELECT ROW(1, NULL) = ROW(1, 2)"), [["NULL"]]);
  // A FALSE field dominates a NULL field → FALSE.
  assert.deepStrictEqual(query(db, "SELECT ROW(1, NULL) = ROW(2, 2)"), [["false"]]);
  // The 3VL negation via NOT (jed has no `<>` operator).
  assert.deepStrictEqual(query(db, "SELECT NOT (ROW(1, 2) = ROW(1, 3))"), [["true"]]);
});

// S5: a composite column compares against a ROW(…) value in WHERE (element-wise), and ORDER BY
// over the composite column sorts lexicographically.
test("composite column compare and order", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip i32)");
  run(db, "CREATE TABLE p (id i32 PRIMARY KEY, home addr)");
  run(db, "INSERT INTO p VALUES (1, ROW('Oak', 30))");
  run(db, "INSERT INTO p VALUES (2, ROW('Oak', 10))");
  run(db, "INSERT INTO p VALUES (3, ROW('Elm', 99))");
  // WHERE composite = ROW(...).
  assert.deepStrictEqual(query(db, "SELECT id FROM p WHERE home = ROW('Oak', 10)"), [["2"]]);
  // ORDER BY composite column — lexicographic: Elm/99, Oak/10, Oak/30.
  assert.deepStrictEqual(query(db, "SELECT id FROM p ORDER BY home"), [["3"], ["2"], ["1"]]);
});

// S5: the all-fields `IS NULL` rule is ONE LEVEL DEEP, not recursive (the empirically-probed PG
// behavior — the differential oracle). A composite-valued field is a non-NULL value, so it counts
// as PRESENT: a nested all-NULL row is therefore `IS NULL` = FALSE and `IS NOT NULL` = TRUE.
test("composite IS NULL non recursive", () => {
  const db = new Database();
  run(db, "CREATE TYPE point AS (x i32, y i32)");
  run(db, "CREATE TYPE seg AS (a point, b point)");
  // The two inner rows are non-null values → the outer row is NOT all-(SQL-)null → IS NULL false,
  // IS NOT NULL true. PG does NOT recurse into the inner all-NULL rows.
  assert.deepStrictEqual(
    query(
      db,
      "SELECT ROW(ROW(NULL, NULL), ROW(NULL, NULL)) IS NULL, ROW(ROW(NULL, NULL), ROW(NULL, NULL)) IS NOT NULL",
    ),
    [["false", "true"]],
  );
  // A SQL-NULL field + a composite field → IS NULL false (not all null), IS NOT NULL false (the
  // NULL field is not present).
  assert.deepStrictEqual(
    query(db, "SELECT ROW(NULL, ROW(1, 2)) IS NULL, ROW(NULL, ROW(1, 2)) IS NOT NULL"),
    [["false", "false"]],
  );
});

// --- a composite type with an array-typed field (spec/design/array.md §12 — the mirror of an
// array-of-composite element). The catalog persists the array field as type_code 15 + the inline
// element descriptor; the value codec / comparison / text-I/O all recurse. Mirrors the Rust tests. ---

test("CREATE TYPE with an array field registers it", () => {
  const db = new Database();
  run(db, "CREATE TYPE poly AS (name text, pts i32[])");
  const ct = db.compositeType("poly");
  assert.ok(ct);
  assert.equal(ct!.fields.length, 2);
  assert.equal(ct!.fields[1]!.name, "pts");
  assert.deepStrictEqual(ct!.fields[1]!.type, arrayT(scalarT("i32")));
});

test("composite with an array field persists through the on-disk image", () => {
  const db = new Database();
  run(db, "CREATE TYPE poly AS (name text, pts i32[])");
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, p poly)");
  run(db, "INSERT INTO t VALUES (1, ROW('a', ARRAY[1, 2, 3]))");
  run(db, "INSERT INTO t VALUES (2, ROW('b', NULL))");
  const image = toImage(db, 256, 1n);
  const loaded = loadDatabase(image);
  const ct = loaded.compositeType("poly");
  assert.ok(ct);
  assert.deepStrictEqual(ct!.fields[1]!.type, arrayT(scalarT("i32")));
  assert.deepStrictEqual(query(loaded, "SELECT id, p FROM t ORDER BY id"), [
    ["1", `(a,"{1,2,3}")`],
    ["2", "(b,)"],
  ]);
});

test("composite with an array-of-composite field (homes addr[])", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip i32)");
  run(db, "CREATE TYPE person AS (name text, homes addr[])");
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, who person)");
  run(db, `INSERT INTO t VALUES (1, ROW('jo', '{"(Main,1)","(Oak,2)"}'))`);
  const image = toImage(db, 256, 1n);
  const loaded = loadDatabase(image);
  assert.deepStrictEqual(query(loaded, "SELECT (who).homes[1] FROM t WHERE id = 1"), [["(Main,1)"]]);
});

test("DROP TYPE is blocked by an array-typed field dependent", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip i32)");
  run(db, "CREATE TYPE person AS (name text, homes addr[])");
  assert.equal(errCode(() => run(db, "DROP TYPE addr")), "2BP01");
  run(db, "DROP TYPE person");
  run(db, "DROP TYPE addr");
});

test("DROP TYPE is blocked by an array-typed column dependent", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip i32)");
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, items addr[])");
  assert.equal(errCode(() => run(db, "DROP TYPE addr")), "2BP01");
});

test("array field errors: typmod 0A000, unknown element 42704", () => {
  const db = new Database();
  assert.equal(errCode(() => run(db, "CREATE TYPE t AS (xs decimal(10,2)[])")), "0A000");
  assert.equal(errCode(() => run(db, "CREATE TYPE t2 AS (xs nope[])")), "42704");
});
