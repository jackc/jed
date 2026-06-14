// Exact base-10 decimal / numeric (spec/design/decimal.md).
//
// A value is (neg, coefficient, scale) = (-1)^neg · coefficient · 10^(-scale). The
// coefficient is a hand-rolled big integer in BASE-10^4 limbs, least-significant-first (the
// limb base/order is internal — only the rendered value and on-disk bytes are cross-core
// contracts, CLAUDE.md §2). Base 10^4 (not 10^9 like Rust/Go) keeps a limb product within JS's
// safe-integer range (2^53) WITHOUT bigint on the value path (bigint is permitted only as a
// test oracle, never here — the design forbids it so division rounding can't silently diverge).
// Always finite (no NaN/±Infinity) and normalized (no high zero limbs, no negative zero).
// Rounding is half away from zero (spec/design/decimal.md §3).

import { engineError, type EngineError } from "./errors.ts";

const BASE = 10000; // 10^4: a limb holds 4 digits; products fit JS safe integers
const BASE_DIGITS = 4;
// Max DECLARABLE precision of numeric(p,s), and the division display-scale clamp —
// spec/types/scalars.toml max_precision (PG NUMERIC_MAX_PRECISION, which is also its
// NUMERIC_MAX_DISPLAY_SCALE). NOT a cap on what an unconstrained value may carry.
export const MAX_PRECISION = 1000;
// Max integer-part digits ANY value may carry — spec/types/scalars.toml max_int_digits
// (PG (NUMERIC_WEIGHT_MAX + 1) * DEC_DIGITS; spec/design/decimal.md §2).
export const MAX_INT_DIGITS = 131072;
// Max digits after the point ANY value may carry — spec/types/scalars.toml max_scale
// (PG NUMERIC_DSCALE_MAX; spec/design/decimal.md §2).
export const MAX_SCALE = 16383;

// The magnitude clamp for a decimal literal's scientific e-notation exponent, tied to the format
// caps so lexing/parsing stays bounded — 1e9999999999 must not materialize a gigabyte of
// coefficient zeros — without changing any outcome: an exponent this large already drives the
// value past the caps (so it traps 22003 at resolve), and a zero coefficient still normalizes to 0
// (spec/design/grammar.md §14). Callers clamp the exponent magnitude to ±EXP_LIMIT while scanning.
export const EXP_LIMIT = MAX_INT_DIGITS + MAX_SCALE + 2;

// decimalFromParts is the canonical [coefficient digits, scale] for a decimal literal, from its
// mantissa (intPart+frac) and an optional scientific exponent (already clamped to ±EXP_LIMIT by the
// caller's scanner; null means no exponent). The display scale is max(0, fracLen-exp); when the
// exponent drives it below zero the coefficient absorbs the surplus as trailing zeros at scale 0, so
// the value still reads coefficient × 10^(-scale). Shared by the lexer (bare 1.5e3) and the
// text→decimal coercion (numeric '1.5e3') so both spell the SAME value (spec/design/grammar.md §14);
// the result is fed to Decimal.fromDigitsScale and cap-checked at resolve.
export function decimalFromParts(intPart: string, frac: string, exp: number | null): [string, number] {
  const fracLen = frac.length;
  if (exp === null) return [intPart + frac, fracLen];
  const effScale = fracLen - exp;
  if (effScale >= 0) return [intPart + frac, effScale];
  return [intPart + frac + "0".repeat(-effScale), 0];
}

function overflow(): EngineError {
  return engineError("numeric_value_out_of_range", "value out of range for type decimal");
}

function divByZero(): EngineError {
  return engineError("division_by_zero", "division by zero");
}

// Decimal is an exact base-10 decimal. `neg` is the sign (always false for zero — no negative
// zero); `scale` is the display scale; `limbs` is the coefficient magnitude (base 10^4,
// LSB-first, no high zero limbs; empty == zero).
export class Decimal {
  readonly neg: boolean;
  readonly scale: number;
  readonly limbs: number[];

  private constructor(neg: boolean, scale: number, limbs: number[]) {
    this.neg = neg;
    this.scale = scale;
    this.limbs = limbs;
  }

  // fromParts constructs from raw parts, normalizing (trim high zero limbs; force neg=false
  // for zero). The single choke-point every constructor ends with.
  static fromParts(neg: boolean, scale: number, limbs: number[]): Decimal {
    const trimmed = magTrim(limbs);
    return new Decimal(trimmed.length === 0 ? false : neg, scale, trimmed);
  }

  // zero is zero at the given display scale.
  static zero(scale: number): Decimal {
    return new Decimal(false, scale, []);
  }

  // fromBigInt is the exact decimal of an integer (the lossless int→decimal cast, scale 0).
  static fromBigInt(v: bigint): Decimal {
    const neg = v < 0n;
    let mag = neg ? -v : v;
    const limbs: number[] = [];
    const base = BigInt(BASE);
    while (mag !== 0n) {
      limbs.push(Number(mag % base));
      mag /= base;
    }
    return Decimal.fromParts(neg, 0, limbs);
  }

  // fromDigitsScale builds from a sign, an unscaled coefficient as a decimal-digit string
  // (leading zeros allowed), and a scale. The literal/parse entry point — it does NOT enforce
  // the precision/scale caps (the caller checks them at resolve, trapping 22003).
  static fromDigitsScale(neg: boolean, digits: string, scale: number): Decimal {
    return Decimal.fromParts(neg, scale, magFromDecimalStr(digits));
  }

  // exactFromFloat64 builds the EXACT base-10 decimal equal to a finite IEEE binary64
  // (spec/design/float.md §6; the cross-core float→decimal contract). A binary64 is
  // mantissa·2^exp; for exp ≥ 0 the value is mantissa·2^exp (an integer, scale 0); for exp < 0
  // it is mantissa·5^|exp| · 10^(-|exp|) (since 2^-k = 5^k·10^-k), an exact terminating decimal
  // of scale |exp|. Computed with the limb machinery (magMulSmall by 2 or 5) so it is
  // byte-identical across cores — NOT via Number#toString's shortest round-trip form. Matches Go
  // `exactDecimalFromFloat64`. The caller must reject NaN/±Infinity (→ 22003) before calling.
  static exactFromFloat64(f: number): Decimal {
    if (f === 0) return Decimal.zero(0); // +0 and -0 both → exact 0
    const buf = new DataView(new ArrayBuffer(8));
    buf.setFloat64(0, f, false); // big-endian (layout is irrelevant — we read the same bits back)
    const bits = buf.getBigUint64(0, false);
    return exactFromBits(bits, 11, 52, 1075, -1074);
  }

  // exactFromFloat32 is exactFromFloat64 on the IEEE binary32 significand/exponent (24-bit
  // mantissa: 23 stored + the implicit leading 1; 8-bit exponent, bias 127). The exact value of a
  // binary32 is identical whether the source is read as 32-bit or widened to 64-bit, so the cast
  // operates on the binary32 bit pattern directly (spec/design/float.md §6).
  static exactFromFloat32(f: number): Decimal {
    if (f === 0) return Decimal.zero(0);
    const buf = new DataView(new ArrayBuffer(4));
    buf.setFloat32(0, f, false);
    const bits = BigInt(buf.getUint32(0, false));
    return exactFromBits(bits, 8, 23, 150, -149);
  }

  isZero(): boolean {
    return this.limbs.length === 0;
  }

  // precision is the number of significant digits in the coefficient (0 for zero).
  precision(): number {
    return magDigitCount(this.limbs);
  }

  // checkCap traps 22003 if this (unconstrained) value exceeds the numeric-format caps
  // (spec/design/decimal.md §2): more than MAX_INT_DIGITS integer-part digits or a scale
  // over MAX_SCALE — PG's make_result / numeric_in checks.
  checkCap(): Decimal {
    const p = this.precision();
    const intDigits = p > this.scale ? p - this.scale : 0;
    if (intDigits > MAX_INT_DIGITS || this.scale > MAX_SCALE) throw overflow();
    return this;
  }

  // canonicalString is a collision-free string of the value-canonical form (trailing
  // fractional zeros stripped), for DISTINCT dedup — so 1.5 and 1.50 collapse (decimal.md §5).
  canonicalString(): string {
    if (this.limbs.length === 0) return "+0e0";
    let digits = magToDecimalStr(this.limbs);
    let scale = this.scale;
    while (scale > 0 && digits.endsWith("0")) {
      digits = digits.slice(0, -1);
      scale--;
    }
    return `${this.neg ? "-" : "+"}${digits}e${scale}`;
  }

  // cmpValue is the total order over finite decimals by value: <0, 0, >0.
  cmpValue(o: Decimal): number {
    if (this.neg !== o.neg) return this.neg ? -1 : 1; // neg < non-neg; zero is non-neg
    const s = Math.max(this.scale, o.scale);
    const a = magMulPow10(this.limbs, s - this.scale);
    const b = magMulPow10(o.limbs, s - o.scale);
    const m = magCmp(a, b);
    return this.neg ? -m : m;
  }

  // render is the canonical decimal string (spec/design/decimal.md §6): optional '-', the
  // integer digits, and — iff scale > 0 — '.' and exactly `scale` fractional digits.
  render(): string {
    let digits = magToDecimalStr(this.limbs); // "0" for zero
    const sign = this.neg ? "-" : "";
    if (this.scale === 0) return sign + digits;
    const want = this.scale + 1;
    if (digits.length < want) digits = "0".repeat(want - digits.length) + digits;
    const point = digits.length - this.scale;
    return `${sign}${digits.slice(0, point)}.${digits.slice(point)}`;
  }

  // negate flips the sign (zero stays +0).
  negate(): Decimal {
    return Decimal.fromParts(!this.neg, this.scale, this.limbs);
  }

  // addUncapped is exact addition, result scale max(s1,s2), WITHOUT the §2 format-cap check —
  // the running form for the SUM/AVG accumulator, which (like PG) checks the cap only on the
  // FINAL result, not each intermediate (spec/design/decimal.md §2, determinism.md §7). That
  // makes the trap order-independent: whether a fold overflows no longer depends on the order
  // rows are summed. Standalone arithmetic uses add (capped).
  addUncapped(o: Decimal): Decimal {
    const s = Math.max(this.scale, o.scale);
    const a = magMulPow10(this.limbs, s - this.scale);
    const b = magMulPow10(o.limbs, s - o.scale);
    if (this.neg === o.neg) return Decimal.fromParts(this.neg, s, magAdd(a, b));
    const c = magCmp(a, b);
    if (c === 0) return Decimal.zero(s);
    if (c > 0) return Decimal.fromParts(this.neg, s, magSub(a, b));
    return Decimal.fromParts(o.neg, s, magSub(b, a));
  }

  // add is exact addition, result scale max(s1,s2); traps 22003 at the cap.
  add(o: Decimal): Decimal {
    return this.addUncapped(o).checkCap();
  }

  // sub is this - o (= this + (-o)).
  sub(o: Decimal): Decimal {
    return this.add(o.negate());
  }

  // mul is exact multiplication, result scale s1+s2; traps 22003 at the integer-digit cap.
  // A product scale over MAX_SCALE ROUNDS to it instead of trapping (PG numeric_mul rounds
  // the exact product — spec/design/decimal.md §2).
  mul(o: Decimal): Decimal {
    const scale = this.scale + o.scale;
    let exact = Decimal.fromParts(this.neg !== o.neg, scale, magMul(this.limbs, o.limbs));
    if (scale > MAX_SCALE) exact = exact.roundToScale(MAX_SCALE);
    return exact.checkCap();
  }

  // div is this / o with PG's select_div_scale result scale, rounded half away from zero
  // (spec/design/decimal.md §4). Traps 22012 on a zero divisor, 22003 at the cap.
  div(o: Decimal): Decimal {
    if (o.isZero()) throw divByZero();
    const rscale = selectDivScale(this, o);
    if (this.isZero()) return Decimal.zero(rscale);
    const e = rscale + o.scale - this.scale; // >= 0 since rscale >= s1
    const numer = magMulPow10(this.limbs, e);
    const [q, r] = magDivMod(numer, o.limbs);
    // Round half away from zero: if 2*r >= |divisor|, round the magnitude up.
    let quo = q;
    if (magCmp(magAdd(r, r), o.limbs) >= 0) quo = magAdd(quo, [1]);
    return Decimal.fromParts(this.neg !== o.neg, rscale, quo).checkCap();
  }

  // rem is this % o — remainder of truncated division; result scale max(s1,s2), sign of the
  // dividend (matches the integer %). Traps 22012 on a zero divisor.
  rem(o: Decimal): Decimal {
    if (o.isZero()) throw divByZero();
    const s = Math.max(this.scale, o.scale);
    const a = magMulPow10(this.limbs, s - this.scale);
    const b = magMulPow10(o.limbs, s - o.scale);
    const [, r] = magDivMod(a, b);
    return Decimal.fromParts(this.neg, s, r);
  }

  // roundToScale rounds to target scale, half away from zero (spec/design/decimal.md §3).
  // Increasing the scale only appends zeros (exact).
  roundToScale(target: number): Decimal {
    if (target >= this.scale) {
      return Decimal.fromParts(this.neg, target, magMulPow10(this.limbs, target - this.scale));
    }
    const pow = magPow10(this.scale - target);
    const [q, r] = magDivMod(this.limbs, pow);
    let quo = q;
    if (magCmp(magAdd(r, r), pow) >= 0) quo = magAdd(quo, [1]);
    return Decimal.fromParts(this.neg, target, quo);
  }

  // abs is the magnitude, preserving scale — the abs(numeric) scalar function
  // (spec/design/functions.md §9). Cannot overflow.
  abs(): Decimal {
    return Decimal.fromParts(false, this.scale, this.limbs.slice());
  }

  // roundPlaces is PG round(numeric, n) (spec/design/functions.md §9): round half away from zero
  // to n fractional places. n >= 0 rounds to scale n (delegating to roundToScale, with n clamped
  // at MAX_SCALE like PG numeric_round); n < 0 rounds to the LEFT of the point — result scale 0,
  // value a multiple of 10^-n (round(150, -2) = 200). roundPlaces(0) is round(x). Traps 22003
  // when the round-up carry pushes a value at the integer-digit cap over it (decimal.md §4).
  roundPlaces(n: number): Decimal {
    if (n >= 0) {
      return this.roundToScale(Math.min(n, MAX_SCALE)).checkCap();
    }
    // Drop this.scale + k digits of the magnitude (rounding half away), then re-append the k
    // integer zeros. k is capped at the digit count + 1: beyond that every value rounds to 0
    // (or a single carried 1), so the clamp changes no result but bounds the work.
    const k = Math.min(-n, this.precision() + 1);
    const pow = magPow10(this.scale + k);
    const [q, r] = magDivMod(this.limbs, pow);
    let quo = q;
    if (magCmp(magAdd(r, r), pow) >= 0) quo = magAdd(quo, [1]);
    return Decimal.fromParts(this.neg, 0, magMulPow10(quo, k)).checkCap();
  }

  // coerceToTypmod coerces into numeric(precision, scale): round to scale (half away), then
  // trap 22003 if the integer-part digits exceed precision-scale (spec/design/decimal.md §2).
  coerceToTypmod(precision: number, scale: number): Decimal {
    const rounded = this.roundToScale(scale);
    const sig = rounded.precision();
    const intDigits = sig > scale ? sig - scale : 0;
    if (intDigits > precision - scale) throw overflow();
    return rounded;
  }

  // toBigIntRound rounds to an integer (scale 0, half away) and returns it as a bigint, or
  // null if it would exceed the int64 range (the decimal→int cast; caller range-checks).
  toBigIntRound(): bigint | null {
    const r = this.roundToScale(0);
    let v = 0n;
    const base = BigInt(BASE);
    for (let i = r.limbs.length - 1; i >= 0; i--) {
      v = v * base + BigInt(r.limbs[i]!);
    }
    const signed = r.neg ? -v : v;
    if (signed < -9223372036854775808n || signed > 9223372036854775807n) return null;
    return signed;
  }

  // toCodec returns [neg, scale, base-10^4 coefficient groups MS-first] for the value codec.
  // Zero → no groups (spec/fileformat/format.md). The limbs ARE base-10^4, so the groups are
  // the limbs reversed (MS-first).
  toCodec(): [boolean, number, number[]] {
    return [this.neg, this.scale, this.limbs.slice().reverse()];
  }

  // fromCodec is the inverse of toCodec (used on load).
  static fromCodec(neg: boolean, scale: number, groups: number[]): Decimal {
    return Decimal.fromParts(neg, scale, magTrim(groups.slice().reverse()));
  }
}

// selectDivScale is PG's select_div_scale (spec/design/decimal.md §4): >=16 significant
// quotient digits, no fewer fractional digits than either input, in PG's base-10^4 units.
function selectDivScale(a: Decimal, b: Decimal): number {
  const [w1, f1] = nbase4WeightLead(a);
  const [w2, f2] = nbase4WeightLead(b);
  let qweight = w1 - w2;
  if (f1 <= f2) qweight--;
  let rscale = 16 - 4 * qweight;
  rscale = Math.max(rscale, a.scale, b.scale, 0);
  // PG's display-scale clamp: NUMERIC_MAX_DISPLAY_SCALE = NUMERIC_MAX_PRECISION (1000),
  // deliberately NOT the MAX_SCALE value cap (spec/design/decimal.md §4).
  rscale = Math.min(rscale, MAX_PRECISION);
  return rscale;
}

// nbase4WeightLead returns a decimal value's PG base-10^4 weight (the power of 10^4 of the
// most-significant digit group) and the leading group f (0..9999). Zero → [0, 0].
function nbase4WeightLead(d: Decimal): [number, number] {
  if (d.isZero()) return [0, 0];
  const digits = d.precision();
  const e = digits - 1 - d.scale; // exponent of the leading significant digit
  const w = Math.floor(e / 4);
  const g = e - 4 * w + 1; // 1..4 leading-group decimal digits
  const s = magToDecimalStr(d.limbs);
  let f = 0;
  for (let i = 0; i < g; i++) {
    f = f * 10 + (i < s.length ? s.charCodeAt(i) - 48 : 0);
  }
  return [w, f];
}

// ============================================================================
// decimal_work group counts (spec/design/cost.md §3 "decimal_work") — an operation's work W
// in base-10⁴ digit groups, computed from LOGICAL significant-digit counts, never from this
// core's internal limb count (the cross-core contract, decimal.md §7 #11; this core's limbs
// happen to be base-10⁴ too, but the contract is the logical digit count). All return
// W >= 1; the evaluator charges decimal_work × (W − 1) as a bigint.
// ============================================================================

// max(1, ceil(n/4)) — the base-10⁴ group count of an n-digit coefficient.
function decGroups(n: number): number {
  return Math.max(1, Math.ceil(n / 4));
}

// Both operands' digit counts after aligning to the common scale max(s1, s2) (the digit count
// once the lower-scale coefficient is multiplied up — exactly the add/sub/cmp work).
function alignedDigits(a: Decimal, b: Decimal): [number, number] {
  const s = Math.max(a.scale, b.scale);
  return [a.precision() + (s - a.scale), b.precision() + (s - b.scale)];
}

// W for add/sub/compare: the larger aligned operand.
export function workLinear(a: Decimal, b: Decimal): number {
  const [a1, a2] = alignedDigits(a, b);
  return Math.max(decGroups(a1), decGroups(a2));
}

// W for mul: the product of the (unaligned) operand group counts — schoolbook-quadratic.
export function workMul(a: Decimal, b: Decimal): number {
  return decGroups(a.precision()) * decGroups(b.precision());
}

// W for div: numerator groups (dividend digits + the rescale shift E) × divisor groups,
// E = rscale + s2 − s1 with the same selectDivScale as the result. A zero divisor returns 1 —
// the operation traps 22012 before any work (cost.md §3).
export function workDiv(a: Decimal, b: Decimal): number {
  if (b.isZero()) return 1;
  const rscale = selectDivScale(a, b);
  const e = rscale + b.scale - a.scale; // >= 0 since rscale >= s1
  return decGroups(a.precision() + e) * decGroups(b.precision());
}

// W for mod: the aligned divmod — the product of the aligned group counts. A zero divisor
// returns 1.
export function workMod(a: Decimal, b: Decimal): number {
  if (b.isZero()) return 1;
  const [a1, a2] = alignedDigits(a, b);
  return decGroups(a1) * decGroups(a2);
}

// ============================================================================
// Magnitude helpers — base 10^4, LSB-first, normalized (no high zero limbs).
// ============================================================================

// exactFromBits builds the exact decimal of a finite IEEE float from its raw bit pattern, shared
// by exactFromFloat64/32. `expBits`/`mantBits` are the field widths; `normBias` is the exponent
// adjustment for a normal value (bias + mantBits) and `subExp` the true exponent of a subnormal
// (1 − bias − mantBits). The implicit leading 1 is added for normals only (subnormals omit it).
// value = mant·2^exp: for exp ≥ 0 it is the integer mant·2^exp (scale 0); for exp < 0 it is
// mant·5^|exp| at scale |exp| (since 2^exp = 5^|exp|/10^|exp|), then trailing-zero-trimmed to the
// minimal display scale so the form matches PG's float→numeric (0.5, not 0.500…0). Mirrors Go
// `exactDecimalFromFloat64`.
function exactFromBits(
  bits: bigint,
  expBits: number,
  mantBits: number,
  normBias: number,
  subExp: number,
): Decimal {
  const neg = bits >> BigInt(expBits + mantBits) !== 0n;
  const expMask = (1n << BigInt(expBits)) - 1n;
  const mantMask = (1n << BigInt(mantBits)) - 1n;
  let biasedExp = Number((bits >> BigInt(mantBits)) & expMask);
  let mant = bits & mantMask;
  let exp: number;
  if (biasedExp === 0) {
    exp = subExp; // subnormal: no implicit leading 1
  } else {
    mant |= 1n << BigInt(mantBits); // implicit leading 1
    exp = biasedExp - normBias;
  }
  let mag = magFromBigUint(mant);
  if (exp >= 0) {
    for (let i = 0; i < exp; i++) mag = magMulSmall(mag, 2); // value = mant·2^exp, integer
    return Decimal.fromParts(neg, 0, mag);
  }
  const k = -exp;
  for (let i = 0; i < k; i++) mag = magMulSmall(mag, 5); // value = mant·5^k at scale k
  const d = Decimal.fromParts(neg, k, mag);
  // Trim trailing fractional zeros to the minimal display scale (value unchanged — only zero
  // digits removed), matching PG's float→numeric canonical form.
  const cs = d.canonicalString(); // "+digits e scale" (trailing fractional zeros stripped)
  const ePos = cs.indexOf("e");
  const csNeg = cs[0] === "-";
  const digits = cs.slice(1, ePos);
  const scale = Number(cs.slice(ePos + 1));
  return Decimal.fromDigitsScale(csNeg, digits, scale);
}

// magFromBigUint converts a non-negative bigint into LSB-first base-10^4 limbs (the IEEE
// significand → magnitude step). bigint is permitted here only for the bit extraction; the value
// math below stays on the limb path.
function magFromBigUint(v: bigint): number[] {
  const out: number[] = [];
  const base = BigInt(BASE);
  while (v !== 0n) {
    out.push(Number(v % base));
    v /= base;
  }
  return out;
}

function magTrim(limbs: number[]): number[] {
  let n = limbs.length;
  while (n > 0 && limbs[n - 1] === 0) n--;
  return n === limbs.length ? limbs : limbs.slice(0, n);
}

// magFromDecimalStr parses a decimal-digit string (leading zeros allowed) into LSB-first
// base-10^4 limbs.
function magFromDecimalStr(s: string): number[] {
  let digits = "";
  for (const ch of s) if (ch >= "0" && ch <= "9") digits += ch;
  const out: number[] = [];
  let end = digits.length;
  while (end > 0) {
    const start = Math.max(0, end - BASE_DIGITS);
    out.push(Number(digits.slice(start, end)));
    end = start;
  }
  return magTrim(out);
}

// magToDecimalStr renders LSB-first limbs to a decimal-digit string with no leading zeros
// ("0" for zero).
function magToDecimalStr(limbs: number[]): string {
  if (limbs.length === 0) return "0";
  let out = String(limbs[limbs.length - 1]);
  for (let i = limbs.length - 2; i >= 0; i--) {
    out += String(limbs[i]).padStart(BASE_DIGITS, "0");
  }
  return out;
}

function magDigitCount(limbs: number[]): number {
  if (limbs.length === 0) return 0;
  return String(limbs[limbs.length - 1]).length + (limbs.length - 1) * BASE_DIGITS;
}

function magCmp(a: number[], b: number[]): number {
  if (a.length !== b.length) return a.length < b.length ? -1 : 1;
  for (let i = a.length - 1; i >= 0; i--) {
    if (a[i] !== b[i]) return a[i]! < b[i]! ? -1 : 1;
  }
  return 0;
}

function magAdd(a: number[], b: number[]): number[] {
  const n = Math.max(a.length, b.length);
  const out: number[] = [];
  let carry = 0;
  for (let i = 0; i < n; i++) {
    const sum = (a[i] ?? 0) + (b[i] ?? 0) + carry;
    out.push(sum % BASE);
    carry = Math.floor(sum / BASE);
  }
  if (carry !== 0) out.push(carry);
  return magTrim(out);
}

// magSub is a - b assuming a >= b.
function magSub(a: number[], b: number[]): number[] {
  const out: number[] = [];
  let borrow = 0;
  for (let i = 0; i < a.length; i++) {
    let diff = a[i]! - (b[i] ?? 0) - borrow;
    if (diff < 0) {
      diff += BASE;
      borrow = 1;
    } else {
      borrow = 0;
    }
    out.push(diff);
  }
  return magTrim(out);
}

function magMul(a: number[], b: number[]): number[] {
  if (a.length === 0 || b.length === 0) return [];
  const out = new Array<number>(a.length + b.length).fill(0);
  for (let i = 0; i < a.length; i++) {
    let carry = 0;
    for (let j = 0; j < b.length; j++) {
      const cur = out[i + j]! + a[i]! * b[j]! + carry;
      out[i + j] = cur % BASE;
      carry = Math.floor(cur / BASE);
    }
    out[i + b.length] += carry;
  }
  return magTrim(out);
}

// magMulSmall multiplies a magnitude by a single small factor s (0 <= s < BASE).
function magMulSmall(a: number[], s: number): number[] {
  if (s === 0 || a.length === 0) return [];
  const out: number[] = [];
  let carry = 0;
  for (let i = 0; i < a.length; i++) {
    const cur = a[i]! * s + carry;
    out.push(cur % BASE);
    carry = Math.floor(cur / BASE);
  }
  while (carry !== 0) {
    out.push(carry % BASE);
    carry = Math.floor(carry / BASE);
  }
  return magTrim(out);
}

// magMulPow10 multiplies by 10^e.
function magMulPow10(a: number[], e: number): number[] {
  if (a.length === 0 || e === 0) return a.slice();
  const full = Math.floor(e / BASE_DIGITS);
  const rem = e % BASE_DIGITS;
  let shifted = new Array<number>(full).fill(0).concat(a); // prepend `full` zero limbs
  if (rem > 0) shifted = magMulSmall(shifted, 10 ** rem);
  return magTrim(shifted);
}

function magPow10(e: number): number[] {
  return magMulPow10([1], e);
}

// magDivMod is long division: [quotient, remainder] of num / den (den != 0). Each quotient
// limb is found by binary search in [0, BASE) — boring and identical across cores.
function magDivMod(num: number[], den: number[]): [number[], number[]] {
  if (magCmp(num, den) < 0) return [[], num.slice()];
  const quo = new Array<number>(num.length).fill(0);
  let rem: number[] = [];
  for (let i = num.length - 1; i >= 0; i--) {
    rem = magTrim([num[i]!, ...rem]); // rem = rem*BASE + num[i]
    let lo = 0;
    let hi = BASE - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (magCmp(magMulSmall(den, mid), rem) <= 0) lo = mid;
      else hi = mid - 1;
    }
    quo[i] = lo;
    rem = magSub(rem, magMulSmall(den, lo));
  }
  return [magTrim(quo), rem];
}
