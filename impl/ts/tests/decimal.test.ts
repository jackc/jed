// Phase 3: the exact decimal / numeric type — unit tests on the Decimal class and end-to-end
// tests through execute (spec/design/decimal.md). End-to-end assertions are on RENDERED output
// (the cross-core contract), since decimal value-equality (1.5 == 1.50) is scale-insensitive.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Decimal, MAX_INT_DIGITS } from "../src/decimal.ts";
import { loadDatabase, toImage } from "../src/format.ts";
import { execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

const execD = execute;

// dec parses "[-]int[.frac]" into a Decimal (mirrors the lexer/parser).
function dec(s: string): Decimal {
  let neg = false;
  if (s.startsWith("-")) {
    neg = true;
    s = s.slice(1);
  }
  const dot = s.indexOf(".");
  const intPart = dot < 0 ? s : s.slice(0, dot);
  const frac = dot < 0 ? "" : s.slice(dot + 1);
  return Decimal.fromDigitsScale(neg, intPart + frac, frac.length);
}

test("decimal render preserves display scale", () => {
  const cases: Record<string, string> = {
    "1.50": "1.50", "1.5": "1.5", "0.00": "0.00", "0": "0",
    "-0.013": "-0.013", "123": "123", ".5": "0.5", "100": "100",
  };
  for (const [inp, want] of Object.entries(cases)) {
    assert.equal(dec(inp).render(), want, inp);
  }
});

test("decimal has no negative zero", () => {
  for (const s of ["0", "-0", "-0.00"]) assert.equal(dec(s).neg, false, s);
  const r = dec("1.0").sub(dec("1.0"));
  assert.equal(r.render(), "0.0");
  assert.equal(r.neg, false);
});

test("decimal value equality ignores scale", () => {
  assert.equal(dec("1.5").cmpValue(dec("1.50")), 0);
  assert.equal(dec("10").cmpValue(dec("10.0")), 0);
  assert.notEqual(dec("1.5").cmpValue(dec("1.6")), 0);
});

test("decimal ordering is numeric", () => {
  const asc = ["-10", "-1", "0", "0.5", "1", "10"];
  for (let i = 0; i + 1 < asc.length; i++) {
    assert.ok(dec(asc[i]!).cmpValue(dec(asc[i + 1]!)) < 0, `${asc[i]} < ${asc[i + 1]}`);
  }
  assert.ok(dec("1.23").cmpValue(dec("1.2")) > 0);
});

test("decimal add / sub / mul scale rules", () => {
  assert.equal(dec("1.50").add(dec("1.5")).render(), "3.00");
  assert.equal(dec("1.234").sub(dec("1.2")).render(), "0.034");
  assert.equal(dec("1.50").mul(dec("1.5")).render(), "2.250");
  assert.equal(dec("2.0").mul(dec("3.000")).render(), "6.0000");
});

test("decimal division scale + half-away rounding", () => {
  const cases: [string, string, string][] = [
    ["1", "3", "0.33333333333333333333"],
    ["2", "3", "0.66666666666666666667"],
    ["1", "7", "0.14285714285714285714"],
    ["10.0", "4.0", "2.5000000000000000"],
    ["1.0", "8.0", "0.12500000000000000000"],
    ["100", "7", "14.2857142857142857"],
  ];
  for (const [a, b, want] of cases) {
    assert.equal(dec(a).div(dec(b)).render(), want, `${a}/${b}`);
  }
});

test("decimal modulo", () => {
  assert.equal(dec("5.5").rem(dec("2")).render(), "1.5");
  assert.equal(dec("-5.5").rem(dec("2")).render(), "-1.5");
  assert.equal(dec("5.50").rem(dec("2.0")).render(), "1.50");
});

test("decimal rounding half away from zero", () => {
  const cases: [string, number, string][] = [
    ["0.125", 2, "0.13"], ["-0.125", 2, "-0.13"],
    ["2.5", 0, "3"], ["-2.5", 0, "-3"],
    ["2.45", 1, "2.5"], ["9.5", 0, "10"],
  ];
  for (const [inp, scale, want] of cases) {
    assert.equal(dec(inp).roundToScale(scale).render(), want, `round(${inp},${scale})`);
  }
});

test("decimal div/mod by zero traps 22012", () => {
  assert.equal(errCode(() => void dec("1").div(dec("0"))), "22012");
  assert.equal(errCode(() => void dec("1").rem(dec("0"))), "22012");
});

test("decimal to i64 rounds half away", () => {
  assert.equal(dec("2.5").toBigIntRound(), 3n);
  assert.equal(dec("-2.5").toBigIntRound(), -3n);
  assert.equal(dec("2.4").toBigIntRound(), 2n);
  assert.equal(dec("100").toBigIntRound(), 100n);
  assert.equal(dec("100000000000000000000000000000").toBigIntRound(), null);
});

test("decimal on-disk codec round trip", () => {
  for (const s of ["0", "1.50", "-12345.6789", "100000000.000001", "999999999999"]) {
    const d = dec(s);
    const [neg, scale, groups] = d.toCodec();
    assert.equal(Decimal.fromCodec(neg, scale, groups).render(), d.render(), s);
  }
  assert.equal(dec("0.00").toCodec()[2].length, 0);
});

test("decimal big multiplication is exact (76 digits, no float)", () => {
  const a = dec("12345678901234567890123456789012345678");
  const b = dec("99999999999999999999999999999999999999");
  assert.equal(a.mul(b).precision(), 76);
});

// --- end-to-end through execute ---------------------------------------------

function one(db: ReturnType<typeof dbWith>, sql: string): string {
  const rows = query(db, sql);
  assert.equal(rows.length, 1, sql);
  assert.equal(rows[0]!.length, 1, sql);
  return rows[0]![0]!;
}

test("decimal on-disk round trip persists values + typmod", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, money numeric(10,2), free numeric)",
    "INSERT INTO t VALUES (1, 1.5, -12345.6789), (2, 0, 0.00), (3, 100, NULL)",
  ]);
  const image = toImage(db, 8192, 1n);
  const loaded = loadDatabase(image);
  assert.deepStrictEqual(toImage(loaded, 8192, 1n), image, "re-serialization byte-identical");
  assert.equal(one(loaded, "SELECT free FROM t WHERE id = 1"), "-12345.6789");
  execD(loaded, "INSERT INTO t VALUES (4, 9.999, 9.999)");
  assert.equal(one(loaded, "SELECT money FROM t WHERE id = 4"), "10.00"); // typmod persisted
});

// The SUM/AVG accumulator's addUncapped path (spec/design/decimal.md §2, determinism.md §7): the
// running sum may cross the §2 format cap mid-fold without trapping; only the FINAL result is
// cap-checked — the order-independent-trap fix. Too large to reach through SQL literals (a
// 131072-digit value is ~74 KB), so pinned here. a is exactly at the cap (131072 nines); a + a is
// one digit over it.
test("decimal SUM accumulator checks only the final cap", () => {
  const a = Decimal.fromDigitsScale(false, "9".repeat(MAX_INT_DIGITS), 0);
  a.checkCap(); // exactly at the cap — does not throw
  // Capped add (standalone arithmetic) still traps at the cap — unchanged contract.
  assert.equal(
    errCode(() => {
      a.add(a);
    }),
    "22003",
  );
  // Uncapped fold may exceed the cap intermediately and NOT trap...
  const over = a.addUncapped(a); // 2·a, one digit over the cap
  // ...then come back in range, so the FINAL check passes and the value is exact.
  const back = over.addUncapped(a.negate()).checkCap();
  assert.equal(back.cmpValue(a), 0);
  // A final result genuinely over the cap still traps 22003 (PG's make_result).
  assert.equal(
    errCode(() => {
      over.checkCap();
    }),
    "22003",
  );
});

// PG numeric_mul's rounding: an exact product whose scale exceeds max_scale (16383) ROUNDS to
// it, half away from zero, instead of trapping (spec/design/decimal.md §2).
test("decimal mul rounds its result scale at max_scale", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1)"]);
  const tiny1 = "0." + "0".repeat(8191) + "1"; // 1e-8192 (scale 8192)
  const tiny5 = "0." + "0".repeat(8191) + "5"; // 5e-8192
  // 1e-8192 * 1e-8192 = 1e-16384: the dropped digit is 1 -> rounds DOWN to 0 at scale 16383.
  assert.equal(one(db, `SELECT ${tiny1} * ${tiny1} = 0 FROM t`), "true");
  // 5e-8192 * 1e-8192 = 5e-16384: the dropped digit is 5 -> rounds UP to 1e-16383, nonzero.
  assert.equal(one(db, `SELECT ${tiny5} * ${tiny1} = 0 FROM t`), "false");
});

// decimal_work is charged and GUARDED before the limb work runs (spec/design/cost.md §3/§6),
// so a ceiling aborts a pathological multiply up front (CLAUDE.md §13). ~20000 digits is
// ~5000 groups; the mul W is ~25,000,000 — far over the tiny ceiling.
test("decimal cost ceiling aborts ahead of a big multiply", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1)"]);
  const big = "9".repeat(20000) + ".5";
  db.setMaxCost(1000n);
  assert.equal(errCode(() => void execD(db, `SELECT ${big} * ${big} FROM t`)), "54P01");
});
