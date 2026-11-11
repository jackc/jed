// The decimal hot path's shortcuts (packed-leaf.md §11 Track A5; decimal.md §1) against a BigInt
// oracle — the one place a bigint is allowed near decimals (decimal.ts header). Each shortcut must be
// indistinguishable from the general path it replaces: fromBigInt's number split below 2^53, the
// digit-count and number comparisons of cmpValue, addUncapped's no-copy operand at the common scale,
// the string-free digit count (precision, hence workLinear's decimal_work), the value codec's direct
// limb decode, and decodeIntAt's single-conversion form. Magnitudes straddle every boundary those
// shortcuts test: a limb (10^4), three limbs (10^12), 10^15, 2^53, 2^63.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { type ColType, fixedDecimalWidth } from "../src/catalog.ts";
import { Decimal, workLinear } from "../src/decimal.ts";
import { decodeIntAt, encodeInt } from "../src/encoding.ts";
import { encodeValue, readInlineBody } from "../src/format.ts";
import { createDatabase, openDatabase } from "../src/tooling.ts";
import { decimalValue, render, type Value } from "../src/value.ts";

const MAGNITUDES: bigint[] = [
  0n,
  1n,
  9n,
  10n,
  99n,
  9999n,
  10000n,
  10001n,
  99999999n,
  100000000n,
  999999999999n,
  1000000000000n,
  1000000000001n,
  999999999999999n,
  1000000000000000n,
  1000000000000001n,
  (1n << 53n) - 1n,
  1n << 53n,
  (1n << 53n) + 1n,
  9999999999999999n,
  10000000000000000n,
  (1n << 63n) - 1n,
  1n << 63n,
  10n ** 19n,
  10n ** 30n + 7n,
  10n ** 40n - 1n,
];
const SCALES = [0, 1, 2, 3, 4, 5, 8, 15, 16, 20];

// A decimal with coefficient `coef` (signed) at `scale`, through the digit-string constructor (the
// general path).
function dec(coef: bigint, scale: number): Decimal {
  const neg = coef < 0n;
  return Decimal.fromDigitsScale(neg, (neg ? -coef : coef).toString(), scale);
}

// renderOracle is coefficient × 10^-scale in the canonical rendering (decimal.md §6).
function renderOracle(coef: bigint, scale: number): string {
  const neg = coef < 0n;
  let digits = (neg ? -coef : coef).toString();
  if (scale > 0) {
    if (digits.length <= scale) digits = "0".repeat(scale + 1 - digits.length) + digits;
    digits = `${digits.slice(0, digits.length - scale)}.${digits.slice(digits.length - scale)}`;
  }
  return (neg ? "-" : "") + digits;
}

const digitsOf = (coef: bigint): number =>
  coef === 0n ? 0 : (coef < 0n ? -coef : coef).toString().length;
const groupsOf = (n: number): number => Math.max(1, Math.ceil(n / 4));

function values(): { coef: bigint; scale: number; d: Decimal }[] {
  const out: { coef: bigint; scale: number; d: Decimal }[] = [];
  for (const m of MAGNITUDES) {
    for (const scale of SCALES) {
      for (const coef of m === 0n ? [0n] : [m, -m]) out.push({ coef, scale, d: dec(coef, scale) });
    }
  }
  return out;
}

test("fromBigInt equals the digit-string constructor on both sides of 2^53", () => {
  for (const m of MAGNITUDES) {
    for (const v of m === 0n ? [0n] : [m, -m]) {
      const got = Decimal.fromBigInt(v);
      const want = dec(v, 0);
      assert.equal(got.render(), want.render(), `${v}`);
      assert.deepEqual(got.limbs, want.limbs, `${v}`);
      assert.equal(got.neg, want.neg, `${v}`);
      assert.equal(got.scale, 0, `${v}`);
    }
  }
});

test("precision is the coefficient's digit count", () => {
  for (const { coef, scale, d } of values()) {
    assert.equal(d.precision(), digitsOf(coef), `${coef}e-${scale}`);
  }
});

test("cmpValue, addUncapped and workLinear match the BigInt oracle for every pair", () => {
  const vs = values();
  // Every value against a spread of the others (the full square is 150k pairs; every boundary
  // magnitude still meets every scale difference and both sign pairings).
  for (let i = 0; i < vs.length; i++) {
    for (let j = i % 3; j < vs.length; j += 3) {
      const a = vs[i]!;
      const b = vs[j]!;
      const s = Math.max(a.scale, b.scale);
      const ca = a.coef * 10n ** BigInt(s - a.scale);
      const cb = b.coef * 10n ** BigInt(s - b.scale);
      const what = `${a.coef}e-${a.scale} vs ${b.coef}e-${b.scale}`;
      // (An equal negative pair compares as -0, as it always has.)
      assert.equal(
        Math.sign(a.d.cmpValue(b.d)) || 0,
        ca < cb ? -1 : ca > cb ? 1 : 0,
        `cmp ${what}`,
      );
      const sum = a.d.addUncapped(b.d);
      assert.equal(sum.render(), renderOracle(ca + cb, s), `add ${what}`);
      assert.equal(sum.scale, s, `add scale ${what}`);
      assert.equal(sum.neg, ca + cb < 0n, `add sign ${what}`);
      // The operands are untouched (the no-copy operand must only be read).
      assert.equal(a.d.render(), renderOracle(a.coef, a.scale), `operand ${what}`);
      assert.equal(b.d.render(), renderOracle(b.coef, b.scale), `operand ${what}`);
      // decimal_work: the larger aligned operand's digit groups (cost.md §3).
      const wa = digitsOf(a.coef) + (s - a.scale);
      const wb = digitsOf(b.coef) + (s - b.scale);
      assert.equal(workLinear(a.d, b.d), Math.max(groupsOf(wa), groupsOf(wb)), `work ${what}`);
    }
  }
});

test("cmpValue separates neighbours at different scales around its number shortcut", () => {
  // The same digits written at two scales, one unit apart in the last place: equal aligned digit
  // counts, so only an exact comparison orders them — below, at, and above the 15-digit bound where
  // the number shortcut stops and the limb path takes over.
  for (const base of [
    10n ** 13n + 1n,
    10n ** 14n - 1n,
    10n ** 14n,
    10n ** 15n - 1n,
    (1n << 53n) - 1n,
    1n << 53n,
    (1n << 53n) + 1n,
    10n ** 16n - 1n,
    10n ** 17n + 3n,
  ]) {
    for (const shift of [1, 2, 3]) {
      for (const delta of [-1n, 0n, 1n]) {
        for (const sign of [1n, -1n]) {
          const a = dec(sign * base, 0);
          const scaled = sign * (base * 10n ** BigInt(shift) + delta);
          const b = dec(scaled, shift);
          const ca = sign * base * 10n ** BigInt(shift);
          const want = ca < scaled ? -1 : ca > scaled ? 1 : 0;
          const what = `${sign * base} vs ${scaled}e-${shift}`;
          assert.equal(Math.sign(a.cmpValue(b)) || 0, want, what);
          assert.equal(Math.sign(b.cmpValue(a)) || 0, -want || 0, `reversed ${what}`);
        }
      }
    }
  }
});

test("cmpValue orders a zero against a value at a far scale", () => {
  // A scale difference past what a JS number can hold as a power of ten (10^400 is Infinity).
  for (const scale of [1, 15, 16, 400, 16383]) {
    for (const zeroScale of [0, 3, 500]) {
      const zero = dec(0n, zeroScale);
      for (const sign of [1n, -1n]) {
        const tiny = dec(sign, scale);
        assert.equal(Math.sign(tiny.cmpValue(zero)), Number(sign), `${sign}e-${scale} vs 0`);
        assert.equal(Math.sign(zero.cmpValue(tiny)), -Number(sign), `0 vs ${sign}e-${scale}`);
      }
      assert.equal(Math.sign(zero.cmpValue(dec(0n, scale))) || 0, 0);
    }
  }
});

test("the value codec decodes a decimal to the value that was stored", () => {
  // Through a stored column, so the bytes take the inline decode both lanes and row reads use.
  const db = createDatabase({});
  try {
    const s = db.session();
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY, d numeric)");
    const vs = values();
    vs.forEach(({ coef, scale }, i) => {
      s.execute(`INSERT INTO t VALUES (${i}, '${renderOracle(coef, scale)}'::numeric)`);
    });
    const got = new Map<bigint, string>();
    for (const row of s.query("SELECT id, d FROM t")) {
      if (row[0]!.kind === "int") got.set(row[0]!.int, render(row[1]!));
    }
    vs.forEach(({ coef, scale, d }, i) => {
      assert.equal(got.get(BigInt(i)), renderOracle(coef, scale), `${coef}e-${scale}`);
      // And the stored bytes are the general path's.
      assert.deepEqual(
        encodeValue({ kind: "scalar", scalar: "decimal" }, decimalValue(d)),
        encodeValue({ kind: "scalar", scalar: "decimal" }, decimalValue(dec(coef, scale))),
      );
    });
  } finally {
    db.close();
  }
});

// The fixed-width numeric(p,s) leaf slot (format.md v51 "Fixed-width column slot"): the slot is the
// integer body of the value's unscaled integer, and the decode builds the decimal with no bigint.
const SLOT_INT = { 2: "i16", 4: "i32", 8: "i64" } as const;

function slotType(width: 2 | 4 | 8, scale: number): ColType {
  return { kind: "scalar", scalar: "decimal", fixedDec: { width, scale } };
}

test("a fixed-width decimal slot is the integer body of the unscaled value", () => {
  const edges: bigint[] = [0n, 1n, 9n, 9999n, 10000n, 99999999n, 100000000n, 999999999999n];
  for (const bits of [15n, 31n, 32n, 52n, 53n, 54n, 63n]) {
    const e = 1n << bits;
    edges.push(e - 1n, e, e + 1n);
  }
  edges.push(10n ** 15n, 10n ** 16n - 1n, 10n ** 16n, 10n ** 18n - 1n);
  for (const width of [2, 4, 8] as const) {
    const max = (1n << BigInt(8 * width - 1)) - 1n;
    for (const m of edges) {
      for (const u of m === 0n ? [0n] : [m, -m]) {
        if (u > max || u < -max - 1n) continue;
        for (const scale of [0, 2, 6]) {
          const ty = slotType(width, scale);
          const enc = encodeValue(ty, decimalValue(dec(u, scale)));
          // tag + the i16/i32/i64 order-preserving body of the unscaled integer
          assert.equal(enc[0], 0x00);
          assert.deepEqual(enc.subarray(1), encodeInt(SLOT_INT[width], u), `${u} w${width}`);
          // and it reads back as that integer at the column's scale
          const padded = new Uint8Array(3 + width);
          padded.set(enc.subarray(1), 3);
          const cur = { pos: 3 };
          const back = readInlineBody(ty, padded, cur, "construct");
          assert.equal(cur.pos, 3 + width);
          assert.equal(render(back), renderOracle(u, scale), `${u}e-${scale} w${width}`);
          assert.ok(back.kind === "decimal" && back.dec.scale === scale);
          assert.ok(back.kind === "decimal" && back.dec.cmpValue(dec(u, scale)) === 0);
        }
      }
    }
  }
});

test("a fixed-width decimal slot widens a lower-scale value exactly and refuses the rest", () => {
  // A value below the column's scale is the same number: 1.5 in a scale-2 slot is 150.
  assert.deepEqual(
    encodeValue(slotType(4, 2), decimalValue(dec(15n, 1))).subarray(1),
    encodeInt("i32", 150n),
  );
  // NULL is the lone tag, as for every type (the leaf writes a bitmap bit and a zero slot instead).
  assert.deepEqual(encodeValue(slotType(4, 2), { kind: "null" }), Uint8Array.of(0x01));
  // A value store-coercion did not bring to the typmod is refused, never cut: too many fractional
  // digits, a magnitude outside the slot's integer range, or a non-decimal.
  const refused: [ColType, Value][] = [
    [slotType(4, 2), decimalValue(dec(1505n, 3))],
    [slotType(2, 0), decimalValue(dec(32768n, 0))],
    [slotType(2, 2), decimalValue(dec(-32769n, 2))],
    [slotType(4, 0), decimalValue(dec(1n << 31n, 0))],
    [slotType(8, 0), decimalValue(dec(1n << 63n, 0))],
    [slotType(8, 0), decimalValue(dec(-(1n << 63n) - 1n, 0))],
    [slotType(8, 0), decimalValue(dec(10n ** 30n, 0))],
    [slotType(8, 4), decimalValue(dec(1n << 62n, 0))],
    [slotType(4, 2), { kind: "int", int: 5n }],
  ];
  for (const [ty, v] of refused) {
    assert.throws(() => encodeValue(ty, v), /BUG/, render(v));
  }
  // The extremes of each slot are values, not errors.
  assert.deepEqual(
    encodeValue(slotType(8, 0), decimalValue(dec(-(1n << 63n), 0))).subarray(1),
    encodeInt("i64", -(1n << 63n)),
  );
  assert.deepEqual(
    encodeValue(slotType(2, 0), decimalValue(dec(-32768n, 0))).subarray(1),
    encodeInt("i16", -32768n),
  );
});

test("a numeric(p,s) column stores fixed-width slots and reads the values back", () => {
  // p <= 4, <= 9, <= 18 take 2-, 4- and 8-byte slots; p = 19 and an unconstrained column keep the
  // variable-width codec.
  assert.deepEqual(
    [1, 4, 5, 9, 10, 18, 19, 38].map((p) => fixedDecimalWidth(p)),
    [2, 2, 4, 4, 8, 8, null, null],
  );
  // Through a file, reopened, so the rows come off packed leaves. The third value of each row is
  // stored three ways: an 8-byte slot, numeric(19,2) (variable-width), and unconstrained.
  const rows: [string, string, string, string][] = [
    ["99.99", "999999999", "999999999999.999999", "1000000000000.00"],
    ["-99.99", "-999999999", "-999999999999.999999", "-1000000000000.00"],
    ["0.00", "0", "0.000000", "0.00"],
    ["0.01", "-1", "9007199254.740993", "9007199254.74"], // 2^53 + 1 unscaled: past the safe integers
    ["-0.50", "42", "-9007199254.740992", "-9007199254.74"],
  ];
  const dir = mkdtempSync(join(tmpdir(), "jed-fixed-decimal-"));
  try {
    const path = join(dir, "t.jed");
    let db = createDatabase({ path });
    let s = db.session();
    s.execute(
      "CREATE TABLE t (id i32 PRIMARY KEY, a numeric(4,2), b numeric(9,0), c numeric(18,6), " +
        "w numeric(19,2), u numeric)",
    );
    rows.forEach(([a, b, c], i) => {
      s.execute(`INSERT INTO t VALUES (${i}, ${a}, ${b}, ${c}, ${c}, ${c})`);
    });
    s.execute("INSERT INTO t VALUES (99, NULL, NULL, NULL, NULL, NULL)");
    s.close();
    db.close();
    db = openDatabase(path);
    s = db.session();
    const got: string[][] = [];
    for (const row of s.query("SELECT id, a, b, c, w, u FROM t ORDER BY id")) {
      got.push(row.map((v) => render(v)));
    }
    assert.deepEqual(got, [
      ...rows.map(([a, b, c, w], i) => [String(i), a, b, c, w, c]),
      ["99", "NULL", "NULL", "NULL", "NULL", "NULL"],
    ]);
    // The aggregate lanes read the same slots: the sums are exact.
    const sums = [...s.query("SELECT sum(a), sum(b), sum(c) FROM t")][0]!.map((v) => render(v));
    assert.deepEqual(sums, ["-0.49", "41", "0.000001"]);
    s.close();
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("decodeIntAt inverts encodeInt across the 2^53 boundary", () => {
  const edges = [0n, 1n, (1n << 31n) - 1n, 1n << 31n, 1n << 32n, (1n << 53n) - 1n, 1n << 53n];
  const all: bigint[] = [(1n << 63n) - 1n, -(1n << 63n)];
  for (const e of edges) all.push(e, -e, e + 1n, -(e + 1n), e - 1n, -(e - 1n));
  for (const v of all) {
    if (v < -(1n << 63n) || v >= 1n << 63n) continue;
    const bytes = encodeInt("i64", v);
    const padded = new Uint8Array(11);
    padded.set(bytes, 3);
    assert.equal(decodeIntAt("i64", padded, 3), v, `${v}`);
  }
  for (const v of [0n, 1n, -1n, 2147483647n, -2147483648n]) {
    assert.equal(decodeIntAt("i32", encodeInt("i32", v), 0), v, `i32 ${v}`);
  }
  for (const v of [0n, 1n, -1n, 32767n, -32768n]) {
    assert.equal(decodeIntAt("i16", encodeInt("i16", v), 0), v, `i16 ${v}`);
  }
});
