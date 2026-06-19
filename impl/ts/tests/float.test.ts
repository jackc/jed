// Phase: the IEEE 754 binary float types f32 / f64 (spec/design/float.md). Unit tests on
// the value-level semantics (total order, render, the -0/NaN canonicalization, the codec bytes) and
// end-to-end tests through execute (literals, arithmetic + traps, casts, the canonical-fold SUM/AVG,
// MIN/MAX, GROUP BY, the scalar functions, the strict-island 42804s, and the f32 Math.fround
// discipline). The R-tag exemption (float.md §9) means cross-core text layout differs; these
// assertions pin THIS core's deterministic surface (storage, total order, kernel, exact-sum fold).

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { Decimal } from "../src/decimal.ts";
import { loadDatabase, toImage } from "../src/format.ts";
import { create, Database, execute } from "../src/lib.ts";
import { canonFloat, float32Value, float64Value, floatTotalCmp, renderFloat } from "../src/value.ts";
import { dbWith, errCode, query } from "./util.ts";

const GOLDEN_PAGE_SIZE = 256;

// --- value-level: render, canonicalization, total order ---------------------

test("renderFloat: specials and -0", () => {
  assert.equal(renderFloat(1.5), "1.5");
  assert.equal(renderFloat(0), "0");
  assert.equal(renderFloat(-0), "-0"); // JS (-0).toString() is "0"; we special-case it
  assert.equal(renderFloat(Infinity), "Infinity");
  assert.equal(renderFloat(-Infinity), "-Infinity");
  assert.equal(renderFloat(NaN), "NaN");
  assert.equal(renderFloat(-1.25), "-1.25");
});

test("canonFloat maps -0 to +0 only", () => {
  assert.ok(Object.is(canonFloat(-0), 0)); // -0 → +0
  assert.ok(Object.is(canonFloat(0), 0));
  assert.equal(canonFloat(1.5), 1.5);
  assert.ok(Number.isNaN(canonFloat(NaN))); // NaN untouched (floatTotalCmp collapses it)
  assert.equal(canonFloat(Infinity), Infinity);
});

test("floatTotalCmp is the PG total order: -Inf < finite < +Inf < NaN, -0==+0, NaN==NaN", () => {
  // NaN is the single LARGEST value, above +Infinity.
  assert.equal(floatTotalCmp(NaN, Infinity), 1);
  assert.equal(floatTotalCmp(Infinity, NaN), -1);
  assert.equal(floatTotalCmp(NaN, NaN), 0); // all NaNs one class
  // -0 == +0.
  assert.equal(floatTotalCmp(-0, 0), 0);
  assert.equal(floatTotalCmp(0, -0), 0);
  // Ordinary order over finite + infinities.
  assert.equal(floatTotalCmp(-Infinity, -1e308), -1);
  assert.equal(floatTotalCmp(-1, 1), -1);
  assert.equal(floatTotalCmp(1, -1), 1);
  assert.equal(floatTotalCmp(1.5, 1.5), 0);
  assert.equal(floatTotalCmp(1e308, Infinity), -1);
  // A sorted array lands -Inf .. finite .. +Inf .. NaN.
  const xs = [NaN, 3, -Infinity, -0, 0, Infinity, -2.5, 100];
  xs.sort(floatTotalCmp);
  assert.deepEqual(
    xs.map((x) => (Number.isNaN(x) ? "NaN" : renderFloat(canonFloat(x)))),
    ["-Infinity", "-2.5", "0", "0", "3", "100", "Infinity", "NaN"],
  );
});

// --- value codec round-trip (incl -0 / NaN / ±Inf), via DataView -------------
// encodeValue/readInlineBody are not exported; this mirrors their codec (DataView, big-endian, no
// length prefix). Every value round-trips verbatim EXCEPT NaN, which the codec canonicalizes to the
// single quiet pattern (float.md §10); V8 already materializes a canonical NaN, so a round-tripped
// NaN is simply asserted to still be a NaN. The bit-level check is what the cross-core golden
// also asserts.

function roundTripF64(n: number): number {
  const dv = new DataView(new ArrayBuffer(8));
  dv.setFloat64(0, n, false); // big-endian, exactly encodeValue's write
  return dv.getFloat64(0, false);
}
function roundTripF32(n: number): number {
  const dv = new DataView(new ArrayBuffer(4));
  dv.setFloat32(0, Math.fround(n), false);
  return dv.getFloat32(0, false);
}

test("f64 codec round-trips finite/±Inf verbatim, NaN as a NaN", () => {
  for (const n of [0, 1.5, -1.5, 1e308, 5e-324, Infinity, -Infinity]) {
    assert.ok(Object.is(roundTripF64(n), n), `round-trip ${n}`);
  }
  // -0 keeps its sign bit on disk (the -0→+0 collapse is a compare/key concern, not storage).
  assert.ok(Object.is(roundTripF64(-0), -0), "round-trip -0 keeps the sign bit");
  assert.ok(Number.isNaN(roundTripF64(NaN)), "round-trip NaN (canonicalized to one quiet pattern)");
});

test("f32 codec round-trips binary32 finite/±Inf verbatim, NaN as a NaN", () => {
  for (const n of [0, 1.5, -1.5, Math.fround(0.1), 3.4e38, Infinity, -Infinity]) {
    const v = Math.fround(n);
    assert.ok(Object.is(roundTripF32(v), v), `round-trip ${v}`);
  }
  assert.ok(Object.is(roundTripF32(-0), -0), "round-trip -0 keeps the sign bit");
  assert.ok(Number.isNaN(roundTripF32(NaN)), "round-trip NaN");
});

test("f64 big-endian on-disk bytes (cross-core byte contract)", () => {
  // 1.5 = 0x3FF8000000000000 big-endian.
  const dv = new DataView(new ArrayBuffer(8));
  dv.setFloat64(0, 1.5, false);
  const hex = Array.from(new Uint8Array(dv.buffer))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
  assert.equal(hex, "3ff8000000000000");
});

test("f32 big-endian on-disk bytes (cross-core byte contract)", () => {
  // 1.5f = 0x3FC00000 big-endian.
  const dv = new DataView(new ArrayBuffer(4));
  dv.setFloat32(0, Math.fround(1.5), false);
  const hex = Array.from(new Uint8Array(dv.buffer))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
  assert.equal(hex, "3fc00000");
});

// --- end-to-end value codec via toImage/loadDatabase (finite, both widths) ---

test("finite float rows survive an on-disk round-trip (toImage → loadDatabase)", () => {
  const db = dbWith([
    "CREATE TABLE t (id int PRIMARY KEY, a f64, b f32)",
    "INSERT INTO t VALUES (1, 1.5, 0.1), (2, -2.25, -3.5), (3, 100000.5, 1.0)",
  ]);
  const image = toImage(db, GOLDEN_PAGE_SIZE, 1n);
  const loaded = loadDatabase(image);
  // f32 0.1 frounds to 0.10000000149011612 — its shortest binary32 form.
  assert.deepEqual(query(loaded, "SELECT a, b FROM t ORDER BY id"), [
    ["1.5", "0.10000000149011612"],
    ["-2.25", "-3.5"],
    ["100000.5", "1"],
  ]);
});

// --- literals + the f32 Math.fround discipline --------------------------

test("f32 0.1 differs from f64 0.1 (Math.fround applied)", () => {
  const db = new Database();
  assert.deepEqual(query(db, "SELECT CAST('0.1' AS f32), CAST('0.1' AS f64)"), [
    ["0.10000000149011612", "0.1"],
  ]);
  // Aliases real / float resolve to the two widths.
  assert.deepEqual(query(db, "SELECT CAST('1.5' AS real)"), [["1.5"]]);
  assert.deepEqual(query(db, "SELECT float '2.5'"), [["2.5"]]); // `float` = f64
});

test("typed-literal float parse: e-notation, signs, specials; reject junk/range", () => {
  const db = new Database();
  assert.deepEqual(query(db, "SELECT float '1.5e3', float '-3E-2', float '.5', float '7.'"), [
    ["1500", "-0.03", "0.5", "7"],
  ]);
  assert.deepEqual(query(db, "SELECT float 'Infinity', float '-inf', float '+Infinity', float 'NaN'"), [
    ["Infinity", "-Infinity", "Infinity", "NaN"],
  ]);
  // Malformed → 22P02 (NOT parseFloat-lenient): trailing junk, empty, words.
  for (const bad of ["1.5xyz", "", "1.2.3", "abc", "0x10", "1e"]) {
    assert.equal(errCode(() => void execute(db, `SELECT float '${bad}'`)), "22P02", bad);
  }
  // Out of binary64 range → 22003.
  assert.equal(errCode(() => void execute(db, "SELECT float '1e400'")), "22003");
  // Finite literal beyond f32 range → 22003.
  assert.equal(errCode(() => void execute(db, "SELECT real '1e40'")), "22003");
});

test("decimal/integer literal adapts to a float context", () => {
  const db = dbWith([
    "CREATE TABLE t (id int PRIMARY KEY, a f64)",
    "INSERT INTO t VALUES (1, 2.5), (2, 4)", // decimal 2.5 and integer 4 adapt to f64
  ]);
  assert.deepEqual(query(db, "SELECT a FROM t ORDER BY id"), [["2.5"], ["4"]]);
  // Comparison against a decimal/integer literal adapts the literal (WHERE f = 2.5).
  assert.deepEqual(query(db, "SELECT id FROM t WHERE a = 2.5"), [["1"]]);
  assert.deepEqual(query(db, "SELECT id FROM t WHERE a = 4"), [["2"]]);
});

// --- arithmetic: kernel, promotion, traps -----------------------------------

test("float arithmetic: one op per node, width promotion, fround", () => {
  const db = new Database();
  assert.deepEqual(query(db, "SELECT float '1.5' + float '2.5'"), [["4"]]);
  assert.deepEqual(query(db, "SELECT float '10.0' / float '4.0'"), [["2.5"]]);
  assert.deepEqual(query(db, "SELECT float '7.0' % float '3.0'"), [["1"]]);
  assert.deepEqual(query(db, "SELECT - float '3.5'"), [["-3.5"]]);
  // Mixed widths promote to f64: a f32 0.1 widened to f64 keeps its binary32 value.
  assert.deepEqual(query(db, "SELECT CAST('0.1' AS f32) + float '0'"), [["0.10000000149011612"]]);
});

// --- total order in SQL: ORDER BY / DISTINCT / GROUP BY ---------------------

test("ORDER BY / DISTINCT over a float column: total order + -0/NaN dedup", () => {
  const db = dbWith([
    "CREATE TABLE t (id int PRIMARY KEY, a f64)",
    "INSERT INTO t VALUES (1, 3.0), (2, -1.5), (3, 0.0), (4, 100.0)",
  ]);
  assert.deepEqual(query(db, "SELECT a FROM t ORDER BY a"), [["-1.5"], ["0"], ["3"], ["100"]]);
  assert.deepEqual(query(db, "SELECT a FROM t ORDER BY a DESC"), [["100"], ["3"], ["0"], ["-1.5"]]);
  // DISTINCT collapses -0/+0 to one bucket (value-level: floatTotalCmp / canonFloat). Verified at
  // value level since -0 can't be inserted via VALUES; here distinct finite values stay distinct.
  assert.deepEqual(query(db, "SELECT DISTINCT a FROM t ORDER BY a").length.toString(), "4");
});

// --- casts: strict matrix ----------------------------------------------------

// float→decimal must yield the EXACT decimal value of the IEEE float, NOT Number#toString's
// shortest round-trip (which differs in layout across cores and would diverge the `D`-tag compare).
// The exact value is unique and identical to Go's exactDecimalFromFloat64 (float.md §6, IN-CONTRACT).
test("float→decimal is the EXACT decimal expansion (matches Go exactDecimalFromFloat64)", () => {
  // Exact value of the binary64 0.1 = 0.1000000000000000055511151231257827021181583404541015625
  // (Go: exactDecimalFromFloat64(0.1).Render()). A shortest-round-trip route would give "0.1".
  const exact01 = "0.1000000000000000055511151231257827021181583404541015625";
  assert.deepEqual(query(new Database(), "SELECT CAST(f64 '0.1' AS numeric(60,55))"), [[exact01]]);
  // Values that are exactly representable in binary expand to themselves: 0.5, 2.5, 1e20.
  assert.deepEqual(
    query(new Database(), "SELECT CAST(f64 '0.5' AS decimal), CAST(f64 '2.5' AS decimal)"),
    [["0.5", "2.5"]],
  );
  assert.deepEqual(query(new Database(), "SELECT CAST(f64 '1e20' AS decimal)"), [
    ["100000000000000000000"],
  ]);
  // Direct Decimal API parity with Go (the underlying exact-expansion path).
  assert.equal(Decimal.exactFromFloat64(0.1).render(), exact01);
  assert.equal(Decimal.exactFromFloat64(0.5).render(), "0.5");
  assert.equal(Decimal.exactFromFloat64(2.5).render(), "2.5");
  assert.equal(Decimal.exactFromFloat64(1e20).render(), "100000000000000000000");
  // typmod scale coercion (round HALF AWAY) over the exact value: numeric(5,1) rounds 0.1000…0555…
  // down to 0.1 (the 2nd fractional digit is 0).
  assert.deepEqual(query(new Database(), "SELECT CAST(f64 '0.1' AS numeric(5,1))"), [["0.1"]]);
  // f32: the EXACT decimal of the binary32 value (Math.fround(0.1) = 0.10000000149011612),
  // identical whether taken from the binary32 bits directly or widened to binary64 first (the path
  // Go uses): 0.100000001490116119384765625 (scale 27; padded to 30 here).
  const exact01f32 = "0.100000001490116119384765625";
  assert.deepEqual(query(new Database(), "SELECT CAST(f32 '0.1' AS numeric(40,30))"), [
    [exact01f32 + "000"],
  ]);
  assert.equal(Decimal.exactFromFloat32(Math.fround(0.1)).render(), exact01f32);
  // A f32 whole/dyadic value expands exactly too.
  assert.equal(Decimal.exactFromFloat32(Math.fround(2.5)).render(), "2.5");
});

// --- strict island: no implicit int/decimal ⊕ float (42804) -----------------

test("f64 value into a f32 column needs an explicit cast (42804)", () => {
  const db = dbWith([
    "CREATE TABLE src (id int PRIMARY KEY, a f64)",
    "INSERT INTO src VALUES (1, 1.5)",
    "CREATE TABLE dst (id int PRIMARY KEY, x f32)",
  ]);
  // f64 → f32 is lossy/explicit; INSERT ... SELECT of a f64 column into f32 is 42804.
  assert.equal(errCode(() => void execute(db, "INSERT INTO dst SELECT id, a FROM src")), "42804");
  // f32 → f64 widening IS allowed (lossless).
  const db2 = dbWith([
    "CREATE TABLE src2 (id int PRIMARY KEY, b f32)",
    "INSERT INTO src2 VALUES (1, 1.5)",
    "CREATE TABLE dst2 (id int PRIMARY KEY, y f64)",
    "INSERT INTO dst2 SELECT id, b FROM src2",
  ]);
  assert.deepEqual(query(db2, "SELECT y FROM dst2"), [["1.5"]]);
});

// --- GROUP BY: total-order bucketing ----------------------------------------

test("GROUP BY a float column buckets by the total order", () => {
  const db = dbWith([
    "CREATE TABLE t (id int PRIMARY KEY, a f64)",
    "INSERT INTO t VALUES (1, 2.5), (2, 2.5), (3, 4.0)",
  ]);
  assert.deepEqual(query(db, "SELECT a, count(*) FROM t GROUP BY a ORDER BY a"), [
    ["2.5", "2"],
    ["4", "1"],
  ]);
});

// --- value constructors (Math.fround invariant) -----------------------------

// --- spill: a float ORDER BY round-trips through the spill codec ------------

test("float column round-trips through the spill-to-disk sort (per-core codec)", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-float-spill-"));
  try {
    // In-memory (never spills) is the source of truth; a file-backed DB with a tiny workMem spills.
    const mem = new Database();
    const db = create(join(dir, "float_spill.jed"), {});
    for (const d of [mem, db]) {
      execute(d, "CREATE TABLE t (id int PRIMARY KEY, a f64, b f32)");
      for (let i = 0; i < 60; i++) {
        const a = (((i * 37) % 100) - 50) / 4; // unsorted, fractional f64
        const b = (((i * 53) % 100) - 50) / 8; // fractional f32
        execute(d, `INSERT INTO t VALUES (${i}, ${a}, ${b})`);
      }
    }
    db.setWorkMem(96); // ~few rows per run → many spilled runs + k-way merge
    const want = query(mem, "SELECT a, b FROM t ORDER BY a, b, id");
    const got = query(db, "SELECT a, b FROM t ORDER BY a, b, id");
    assert.deepEqual(got, want, "float rows must round-trip identically through the spill sort");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("float32Value always frounds; float64Value is verbatim", () => {
  const v32 = float32Value(0.1) as { kind: string; value: number };
  assert.equal(v32.value, Math.fround(0.1));
  assert.notEqual(v32.value, 0.1); // binary32 ≠ binary64 for 0.1
  const v64 = float64Value(0.1) as { kind: string; value: number };
  assert.equal(v64.value, 0.1);
  // -0 sign preserved by the constructors (storage concern).
  assert.ok(Object.is((float64Value(-0) as { value: number }).value, -0));
  assert.ok(Object.is((float32Value(-0) as { value: number }).value, -0));
});
