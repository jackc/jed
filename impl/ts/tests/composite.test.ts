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
  run(db, "CREATE TYPE addr AS (street text NOT NULL, zip int32)");
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

test("duplicate type name is 42710", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (a int32)");
  assert.equal(errCode(() => run(db, "CREATE TYPE addr AS (b int32)")), "42710");
});

test("unknown field type is 42704", () => {
  const db = new Database();
  assert.equal(errCode(() => run(db, "CREATE TYPE t AS (a nosuchtype)")), "42704");
});

test("duplicate field name is 42701", () => {
  const db = new Database();
  assert.equal(errCode(() => run(db, "CREATE TYPE t AS (a int32, a int64)")), "42701");
});

test("DROP TYPE removes it", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (a int32)");
  run(db, "DROP TYPE addr");
  assert.equal(db.compositeType("addr"), undefined);
});

test("drop missing type is 42704 unless IF EXISTS", () => {
  const db = new Database();
  assert.equal(errCode(() => run(db, "DROP TYPE nope")), "42704");
  run(db, "DROP TYPE IF EXISTS nope"); // no-op success
});

test("DROP TYPE with dependent field is 2BP01", () => {
  const db = new Database();
  run(db, "CREATE TYPE point AS (x int32, y int32)");
  run(db, "CREATE TYPE line AS (a point, b point)");
  // point is referenced by line's fields.
  assert.equal(errCode(() => run(db, "DROP TYPE point")), "2BP01");
  // Dropping the dependent first frees it.
  run(db, "DROP TYPE line");
  run(db, "DROP TYPE point");
});

// S3: a composite column is storable. ROW(…) INSERT then SELECT round-trips the value and
// record_out renders it (Main,90210).
test("composite column ROW round-trip", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  run(db, "CREATE TABLE person (id int32 PRIMARY KEY, home addr)");
  run(db, "INSERT INTO person VALUES (1, ROW('Main', 90210))");
  assert.deepStrictEqual(query(db, "SELECT id, home FROM person"), [["1", "(Main,90210)"]]);
});

// A composite PRIMARY KEY stays rejected (0A000) — the key encoding is authored but unexercised
// (spec/design/composite.md §6).
test("composite primary key is 0A000", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (a int32)");
  assert.equal(errCode(() => run(db, "CREATE TABLE t (home addr PRIMARY KEY)")), "0A000");
});

// record_out field quoting (spec/design/composite.md §8, PG-exact): a field containing a delimiter /
// quote / whitespace is double-quoted; inside the quotes PG DOUBLES an embedded `"` → `""` and
// `\` → `\\` (NOT backslash-escaping). A NULL field is empty; the empty string is "".
test("record_out quoting and NULLs", () => {
  const db = new Database();
  run(db, "CREATE TYPE rec AS (a text, b int32)");
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, r rec)");
  run(db, "INSERT INTO t VALUES (1, ROW('a b', 1))"); // space → quoted
  run(db, "INSERT INTO t VALUES (2, ROW('x,y', 2))"); // comma → quoted
  run(db, "INSERT INTO t VALUES (3, ROW('', 3))"); // empty string → quoted ""
  run(db, "INSERT INTO t VALUES (4, ROW('q\"s', 4))"); // embedded quote → doubled
  run(db, "INSERT INTO t VALUES (5, ROW('plain', NULL))"); // NULL field → empty
  run(db, "INSERT INTO t VALUES (6, ROW('a\\b', 7))"); // embedded backslash → doubled
  const rows = query(db, "SELECT r FROM t ORDER BY id");
  assert.equal(rows[0]![0], '("a b",1)');
  assert.equal(rows[1]![0], '("x,y",2)');
  assert.equal(rows[2]![0], '("",3)');
  assert.equal(rows[3]![0], '("q""s",4)'); // PG: doubled quote
  assert.equal(rows[4]![0], "(plain,)");
  assert.equal(rows[5]![0], '("a\\\\b",7)'); // PG: doubled backslash
});

// S6: record_in round-trips record_out. A `'(…)'::type` cast and the `type '(…)'` typed literal
// parse a composite text literal back into the value (the inverse of record_out).
test("record_in round-trip", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  // The cast spelling and the typed-literal spelling are equivalent.
  assert.deepStrictEqual(query(db, "SELECT '(Main,90210)'::addr"), [["(Main,90210)"]]);
  assert.deepStrictEqual(query(db, "SELECT addr '(Main,90210)'"), [["(Main,90210)"]]);
  // Quoted field with comma; unquoted-empty → NULL; quoted-empty → empty string; doubled quote.
  assert.deepStrictEqual(query(db, "SELECT '(\"x,y\",2)'::addr"), [['("x,y",2)']]);
  assert.deepStrictEqual(query(db, "SELECT ('(,5)'::addr).street IS NULL"), [["true"]]);
  // Field access on a parsed literal pulls the coerced field value.
  assert.deepStrictEqual(query(db, "SELECT ('(Main,90210)'::addr).zip"), [["90210"]]);
});

// S6: a nested composite text literal parses recursively (the inner record is a quoted token).
test("record_in nested", () => {
  const db = new Database();
  run(db, "CREATE TYPE point AS (x int32, y int32)");
  run(db, "CREATE TYPE seg AS (a point, b point)");
  assert.deepStrictEqual(query(db, `SELECT '("(1,2)","(3,4)")'::seg`), [['("(1,2)","(3,4)")']]);
});

// S6 errors: a malformed composite literal / wrong field count is 22P02; a bad field value surfaces
// that field's parse error (e.g. 22P02 for a non-integer zip).
test("record_in errors", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  assert.equal(errCode(() => run(db, "SELECT '(Main)'::addr")), "22P02"); // too few fields
  assert.equal(errCode(() => run(db, "SELECT '(a,b,c)'::addr")), "22P02"); // too many fields
  assert.equal(errCode(() => run(db, "SELECT 'not a record'::addr")), "22P02"); // no parens
  assert.equal(errCode(() => run(db, "SELECT '(Main,notanint)'::addr")), "22P02"); // bad field
});

// A nested composite value round-trips and renders with the inner record quoted.
test("nested composite value round-trip", () => {
  const db = new Database();
  run(db, "CREATE TYPE point AS (x int32, y int32)");
  run(db, "CREATE TYPE seg AS (a point, b point)");
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, s seg)");
  run(db, "INSERT INTO t VALUES (1, ROW(ROW(1, 2), ROW(3, 4)))");
  assert.deepStrictEqual(query(db, "SELECT s FROM t"), [['("(1,2)","(3,4)")']]);
});

// A whole-value-NULL composite column stores and renders as NULL (an omitted column).
test("whole composite NULL", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, home addr)");
  run(db, "INSERT INTO t (id) VALUES (1)"); // home omitted → NULL
  assert.deepStrictEqual(query(db, "SELECT home FROM t"), [["NULL"]]);
});

// Composite values survive a serialize → load round-trip (the v9 recursive value codec).
test("composite values persist through the on-disk image", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  run(db, "CREATE TABLE p (id int32 PRIMARY KEY, home addr)");
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
  run(db, "CREATE TYPE addr AS (a int32)");
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
  run(db, "CREATE TYPE point AS (x int32 NOT NULL, y int32 NOT NULL)");
  run(db, "CREATE TYPE line AS (a point, b point)");
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, n int32)");
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
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  run(db, "CREATE TABLE person (id int32 PRIMARY KEY, home addr)");
  run(db, "INSERT INTO person VALUES (1, ROW('Main', 90210))");
  // Parenthesized-column field access.
  assert.deepStrictEqual(query(db, "SELECT (home).zip, (home).street FROM person"), [["90210", "Main"]]);
  // Field access on an anonymous ROW(…) literal (fields named f1, f2, …), no FROM.
  assert.deepStrictEqual(query(db, "SELECT (ROW('x', 7)).f2"), [["7"]]);
});

// S4: field access on a column is PARENS-REQUIRED (PostgreSQL): `(home).zip` and `(t.home).zip`
// work; the unparenthesized `home.zip` / `t.home.zip` are NOT field access — they resolve as
// (multi-part) column references and fail (`home` is no relation → 42P01). A bare qualified column
// `person.home` (no field) reads the whole composite column.
test("field access requires parens", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  run(db, "CREATE TABLE person (id int32 PRIMARY KEY, home addr)");
  run(db, "INSERT INTO person VALUES (1, ROW('Main', 90210))");
  // `(home).zip`: parenthesized base → field access.
  assert.deepStrictEqual(query(db, "SELECT (home).zip FROM person"), [["90210"]]);
  // `person.home`: `person` IS the relation → reads the whole composite column.
  assert.deepStrictEqual(query(db, "SELECT person.home FROM person"), [["(Main,90210)"]]);
  // `(t.home).zip`: parenthesized qualified column → field access.
  assert.deepStrictEqual(query(db, "SELECT (t.home).zip FROM person t"), [["90210"]]);
  // Unparenthesized `home.zip`: `home` is no relation → 42P01 (NOT field access — PG-exact).
  assert.equal(errCode(() => run(db, "SELECT home.zip FROM person")), "42P01");
});

// S4: `(expr).*` expands a composite into one output column per field, in declaration order.
test("field star expands all fields", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  run(db, "CREATE TABLE person (id int32 PRIMARY KEY, home addr)");
  run(db, "INSERT INTO person VALUES (1, ROW('Main', 90210))");
  assert.deepStrictEqual(query(db, "SELECT id, (home).* FROM person"), [["1", "Main", "90210"]]);
});

// S4 errors: an unknown field is 42703; field access on a non-composite is 42809; a bare qualifier
// that is neither a relation nor a column is still a missing-FROM-entry (42P01).
test("field access errors", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  run(db, "CREATE TABLE person (id int32 PRIMARY KEY, home addr)");
  run(db, "INSERT INTO person VALUES (1, ROW('Main', 90210))");
  assert.equal(errCode(() => run(db, "SELECT (home).nope FROM person")), "42703");
  assert.equal(errCode(() => run(db, "SELECT (id).zip FROM person")), "42809");
  assert.equal(errCode(() => run(db, "SELECT nosuch.col FROM person")), "42P01");
});

// S5: composite equality is element-wise 3VL (PG row comparison). `=` is FALSE if any field is
// FALSE; else UNKNOWN if any field is UNKNOWN; else TRUE.
test("composite equality 3VL", () => {
  const db = new Database();
  run(db, "CREATE TYPE rec AS (a int32, b int32)");
  // Equal rows.
  assert.deepStrictEqual(query(db, "SELECT ROW(1, 2) = ROW(1, 2)"), [["true"]]);
  // A NULL field with all-else-equal → UNKNOWN (renders NULL).
  assert.deepStrictEqual(query(db, "SELECT ROW(1, NULL) = ROW(1, 2)"), [["NULL"]]);
  // A FALSE field dominates a NULL field → FALSE.
  assert.deepStrictEqual(query(db, "SELECT ROW(1, NULL) = ROW(2, 2)"), [["false"]]);
  // The 3VL negation via NOT (jed has no `<>` operator).
  assert.deepStrictEqual(query(db, "SELECT NOT (ROW(1, 2) = ROW(1, 3))"), [["true"]]);
});

// S5: composite ordering `< <= > >=` is lexicographic — the first non-equal field decides.
test("composite ordering lexicographic", () => {
  const db = new Database();
  run(db, "CREATE TYPE rec AS (a int32, b int32)");
  assert.deepStrictEqual(query(db, "SELECT ROW(1, 2) < ROW(1, 3)"), [["true"]]);
  assert.deepStrictEqual(query(db, "SELECT ROW(2, 1) < ROW(1, 9)"), [["false"]]);
  assert.deepStrictEqual(query(db, "SELECT ROW(1, 2) >= ROW(1, 2)"), [["true"]]);
});

// S5: a composite column compares against a ROW(…) value in WHERE (element-wise), and ORDER BY
// over the composite column sorts lexicographically.
test("composite column compare and order", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  run(db, "CREATE TABLE p (id int32 PRIMARY KEY, home addr)");
  run(db, "INSERT INTO p VALUES (1, ROW('Oak', 30))");
  run(db, "INSERT INTO p VALUES (2, ROW('Oak', 10))");
  run(db, "INSERT INTO p VALUES (3, ROW('Elm', 99))");
  // WHERE composite = ROW(...).
  assert.deepStrictEqual(query(db, "SELECT id FROM p WHERE home = ROW('Oak', 10)"), [["2"]]);
  // ORDER BY composite column — lexicographic: Elm/99, Oak/10, Oak/30.
  assert.deepStrictEqual(query(db, "SELECT id FROM p ORDER BY home"), [["3"], ["2"], ["1"]]);
});

// S5: PG's all-fields `IS NULL` / `IS NOT NULL` rule — they are NOT negations. A partially-NULL
// row is FALSE for both; an all-NULL row IS NULL; a whole-value NULL IS NULL.
test("composite IS NULL all fields", () => {
  const db = new Database();
  run(db, "CREATE TYPE rec AS (a int32, b int32)");
  // All fields present → IS NOT NULL true, IS NULL false.
  assert.deepStrictEqual(query(db, "SELECT ROW(1, 2) IS NULL, ROW(1, 2) IS NOT NULL"), [["false", "true"]]);
  // Partially NULL → FALSE for both (the PG gotcha).
  assert.deepStrictEqual(query(db, "SELECT ROW(1, NULL) IS NULL, ROW(1, NULL) IS NOT NULL"), [["false", "false"]]);
  // All fields NULL → IS NULL true, IS NOT NULL false.
  assert.deepStrictEqual(query(db, "SELECT ROW(NULL, NULL) IS NULL, ROW(NULL, NULL) IS NOT NULL"), [["true", "false"]]);
});

// S5: the all-fields `IS NULL` rule is ONE LEVEL DEEP, not recursive (the empirically-probed PG
// behavior — the differential oracle). A composite-valued field is a non-NULL value, so it counts
// as PRESENT: a nested all-NULL row is therefore `IS NULL` = FALSE and `IS NOT NULL` = TRUE.
test("composite IS NULL non recursive", () => {
  const db = new Database();
  run(db, "CREATE TYPE point AS (x int32, y int32)");
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

// S5: DISTINCT and GROUP BY over a composite column use the recursive value key (NULL-safe).
test("composite DISTINCT and GROUP BY", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  run(db, "CREATE TABLE p (id int32 PRIMARY KEY, home addr)");
  run(db, "INSERT INTO p VALUES (1, ROW('Oak', 10))");
  run(db, "INSERT INTO p VALUES (2, ROW('Oak', 10))");
  run(db, "INSERT INTO p VALUES (3, ROW('Elm', 20))");
  // DISTINCT collapses the two identical Oak/10 rows → 2 distinct composites.
  assert.deepStrictEqual(query(db, "SELECT DISTINCT home FROM p ORDER BY home"), [["(Elm,20)"], ["(Oak,10)"]]);
  // GROUP BY the composite column → count per group.
  assert.deepStrictEqual(query(db, "SELECT home, count(*) FROM p GROUP BY home ORDER BY home"), [
    ["(Elm,20)", "1"],
    ["(Oak,10)", "2"],
  ]);
});

// S5: a composite compared with a non-composite, or with a different-arity row, is 42804.
test("composite comparison type errors", () => {
  const db = new Database();
  run(db, "CREATE TYPE rec AS (a int32, b int32)");
  run(db, "CREATE TABLE p (id int32 PRIMARY KEY, r rec)");
  run(db, "INSERT INTO p VALUES (1, ROW(1, 2))");
  // Composite vs scalar.
  assert.equal(errCode(() => run(db, "SELECT r = 1 FROM p")), "42804");
  // Different row sizes.
  assert.equal(errCode(() => run(db, "SELECT ROW(1, 2) = ROW(1, 2, 3)")), "42804");
});

// --- a composite type with an array-typed field (spec/design/array.md §12 — the mirror of an
// array-of-composite element). The catalog persists the array field as type_code 15 + the inline
// element descriptor; the value codec / comparison / text-I/O all recurse. Mirrors the Rust tests. ---

test("CREATE TYPE with an array field registers it", () => {
  const db = new Database();
  run(db, "CREATE TYPE poly AS (name text, pts int32[])");
  const ct = db.compositeType("poly");
  assert.ok(ct);
  assert.equal(ct!.fields.length, 2);
  assert.equal(ct!.fields[1]!.name, "pts");
  assert.deepStrictEqual(ct!.fields[1]!.type, arrayT(scalarT("int32")));
});

test("composite with an array field round-trips (INSERT/SELECT, record_in, field access)", () => {
  const db = new Database();
  run(db, "CREATE TYPE poly AS (name text, pts int32[])");
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, p poly)");
  run(db, "INSERT INTO t VALUES (1, ROW('a', '{1,2,3}'))");
  run(db, "INSERT INTO t VALUES (2, ROW('b', ARRAY[4, 5]))");
  run(db, "INSERT INTO t VALUES (3, ROW('c', '{}'))");
  run(db, "INSERT INTO t VALUES (4, ROW('d', NULL))");
  assert.deepStrictEqual(query(db, "SELECT id, p FROM t ORDER BY id"), [
    ["1", `(a,"{1,2,3}")`],
    ["2", `(b,"{4,5}")`],
    ["3", "(c,{})"],
    ["4", "(d,)"],
  ]);
  assert.deepStrictEqual(query(db, `SELECT '(z,"{7,8}")'::poly`), [[`(z,"{7,8}")`]]);
  assert.deepStrictEqual(query(db, "SELECT (p).pts FROM t WHERE id = 1"), [["{1,2,3}"]]);
  assert.deepStrictEqual(query(db, "SELECT (p).pts[2] FROM t WHERE id = 1"), [["2"]]);
});

test("composite with an array field persists through the on-disk image", () => {
  const db = new Database();
  run(db, "CREATE TYPE poly AS (name text, pts int32[])");
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, p poly)");
  run(db, "INSERT INTO t VALUES (1, ROW('a', ARRAY[1, 2, 3]))");
  run(db, "INSERT INTO t VALUES (2, ROW('b', NULL))");
  const image = toImage(db, 256, 1n);
  const loaded = loadDatabase(image);
  const ct = loaded.compositeType("poly");
  assert.ok(ct);
  assert.deepStrictEqual(ct!.fields[1]!.type, arrayT(scalarT("int32")));
  assert.deepStrictEqual(query(loaded, "SELECT id, p FROM t ORDER BY id"), [
    ["1", `(a,"{1,2,3}")`],
    ["2", "(b,)"],
  ]);
});

test("composite with an array field: comparison and ordering", () => {
  const db = new Database();
  run(db, "CREATE TYPE poly AS (name text, pts int32[])");
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, p poly)");
  run(db, "INSERT INTO t VALUES (1, ROW('a', ARRAY[1, 2]))");
  run(db, "INSERT INTO t VALUES (2, ROW('a', ARRAY[1, 3]))");
  run(db, "INSERT INTO t VALUES (3, ROW('a', ARRAY[1]))");
  assert.deepStrictEqual(query(db, "SELECT id FROM t ORDER BY p"), [["3"], ["1"], ["2"]]);
  assert.deepStrictEqual(
    query(db, "SELECT ROW('a', ARRAY[1,2]) = ROW('a', ARRAY[1,2]), ROW('a', ARRAY[1,2]) = ROW('a', ARRAY[1,3])"),
    [["true", "false"]],
  );
});

test("composite with an array-of-composite field (homes addr[])", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  run(db, "CREATE TYPE person AS (name text, homes addr[])");
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, who person)");
  run(db, `INSERT INTO t VALUES (1, ROW('jo', '{"(Main,1)","(Oak,2)"}'))`);
  const image = toImage(db, 256, 1n);
  const loaded = loadDatabase(image);
  assert.deepStrictEqual(query(loaded, "SELECT (who).homes[1] FROM t WHERE id = 1"), [["(Main,1)"]]);
});

test("DROP TYPE is blocked by an array-typed field dependent", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  run(db, "CREATE TYPE person AS (name text, homes addr[])");
  assert.equal(errCode(() => run(db, "DROP TYPE addr")), "2BP01");
  run(db, "DROP TYPE person");
  run(db, "DROP TYPE addr");
});

test("DROP TYPE is blocked by an array-typed column dependent", () => {
  const db = new Database();
  run(db, "CREATE TYPE addr AS (street text, zip int32)");
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, items addr[])");
  assert.equal(errCode(() => run(db, "DROP TYPE addr")), "2BP01");
});

test("array field errors: typmod 0A000, unknown element 42704", () => {
  const db = new Database();
  assert.equal(errCode(() => run(db, "CREATE TYPE t AS (xs decimal(10,2)[])")), "0A000");
  assert.equal(errCode(() => run(db, "CREATE TYPE t2 AS (xs nope[])")), "42704");
});
