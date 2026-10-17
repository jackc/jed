// The text-building VARIADIC "any" kernels (spec/design/string-functions.md §6; mirrors
// impl/rust/src/executor/text_build.rs): concat, concat_ws, and format, over their already-evaluated
// argument values.

import type { Meter } from "./cost.ts";
import { COSTS } from "./costs.ts";
import { type EngineError, engineError } from "./errors.ts";
import {
  growthResult,
  MAX_RESULT_CHARS,
  quoteIdentText,
  quoteLiteralText,
  type RExpr,
  type VariadicFuncName,
} from "./executor.ts";
import { parseIntLiteral } from "./kernels.ts";
import { utf8Length } from "./memsize.ts";
import { overflow } from "./store.ts";
import type { ZoneRef } from "./timezone.ts";
import { nullValue, renderIn, textValue, type Value, zoneRenderCount } from "./value.ts";

// The hint PostgreSQL attaches to a malformed format() specifier (varlena.c).
const FORMAT_HINT = 'For a single "%" use "%%".';

// textVariadic finishes a concat / concat_ws / format node over its evaluated values (the caller
// charged its operator_eval and evaluated every argument). vals are the fixed parameter (none for
// concat; the separator / format string) followed by the spread arguments, or by the one array of
// the VARIADIC-array form. A NULL separator / format string yields NULL. A NULL VARIADIC array yields
// NULL for concat / concat_ws and is no arguments for format, as in PostgreSQL.
export function textVariadic(
  func: VariadicFuncName,
  args: RExpr[],
  vals: Value[],
  arrayForm: boolean,
  zone: ZoneRef,
  m: Meter,
): Value {
  const fixed = func === "concat" ? 0 : 1;
  let head = "";
  if (fixed === 1) {
    const h = vals[0]!;
    if (h.kind === "null") return nullValue();
    if (h.kind !== "text") throw new Error("resolver restricts the fixed parameter to text");
    head = h.text;
  }
  let rest: Value[];
  if (arrayForm) {
    const a = vals[fixed]!;
    if (a.kind === "null") {
      if (func !== "format") return nullValue();
      rest = [];
    } else if (a.kind === "array") {
      rest = a.elements;
    } else {
      throw new Error("resolver restricts a VARIADIC operand to an array");
    }
  } else {
    rest = vals.slice(fixed);
  }
  if (func === "concat") {
    // A growth kernel (cost.md §8.1), free over constant operands.
    return growthResult(m, args, textValue(concatValues(rest, "", false, zone, m)));
  }
  if (func === "concat_ws") return textValue(concatValues(rest, head, true, zone, m));
  return textValue(formatText(head, rest, zone, m));
}

// outputText is one argument's OUTPUT text (string-functions.md §6): the text PostgreSQL's type
// output function produces. That is the value's rendering, except a boolean, which is t/f (boolout)
// rather than the true/false of the ::text cast. A finite timestamptz at any depth renders in the
// session zone and charges one timezone unit, charged and guarded before rendering. The caller
// handles NULL.
function outputText(v: Value, zone: ZoneRef, m: Meter): string {
  if (v.kind === "text") return v.text;
  if (v.kind === "bool") return v.value ? "t" : "f";
  const zoned = zoneRenderCount(v);
  if (zoned > 0) {
    m.charge(COSTS.timezone * BigInt(zoned));
    m.guard();
  }
  return renderIn(v, zone);
}

// concatValues is concat(VARIADIC "any") / concat_ws(sep, VARIADIC "any") (string-functions.md §6):
// the non-NULL values' output text joined by sep ("" for concat). For concat_ws (amplify) the kernel
// is an amplifier (cost.md §8.1): it sizes the result exactly in UTF-8 bytes, caps it at
// MAX_RESULT_CHARS (54000), charges scalar_byte per output byte, guards, and reserves the bytes
// before building. concat is a growth kernel and its caller charges the built result.
function concatValues(
  vals: Value[],
  sep: string,
  amplify: boolean,
  zone: ZoneRef,
  m: Meter,
): string {
  const parts: string[] = [];
  for (const v of vals) if (v.kind !== "null") parts.push(outputText(v, zone, m));
  if (amplify) {
    let size = 0n;
    for (const p of parts) size += BigInt(utf8Length(p));
    if (parts.length > 1) size += BigInt(parts.length - 1) * BigInt(utf8Length(sep));
    if (size > MAX_RESULT_CHARS)
      throw engineError("program_limit_exceeded", "requested length too large");
    m.charge(COSTS.scalarByte * size);
    m.guard();
    m.reserveScalar(size);
  }
  return parts.join(sep);
}

// A piece of a format() result: a literal span [lo, hi) of the format string, or a converted
// argument with `pad` spaces (on the right when `left`).
type FormatPiece =
  | { lit: true; lo: number; hi: number }
  | { lit: false; text: string; pad: number; left: boolean };

function formatUnterminated(): EngineError {
  return engineError("invalid_parameter_value", "unterminated format() type specifier").hinted(
    FORMAT_HINT,
  );
}

function formatNumberOutOfRange(): EngineError {
  return engineError("numeric_value_out_of_range", "number is out of range");
}

function formatTooFew(): EngineError {
  return engineError("invalid_parameter_value", "too few arguments for format()");
}

function formatArgZero(): EngineError {
  return engineError(
    "invalid_parameter_value",
    "format specifies argument 0, but arguments are numbered from 1",
  );
}

// formatAdvance advances past code unit i of the format string; reaching its end is the
// unterminated-specifier error (PostgreSQL's ADVANCE_PARSE_POINTER). The format syntax is ASCII, so
// stepping UTF-16 code units visits the same specifier characters PostgreSQL steps as bytes.
function formatAdvance(b: string, i: number): number {
  if (i + 1 >= b.length) throw formatUnterminated();
  return i + 1;
}

function isDigit(c: number): boolean {
  return c >= 48 && c <= 57;
}

// formatDigits parses a run of ASCII digits at i as a nonnegative int32 (PostgreSQL's
// text_format_parse_digits): `val` is null when no digit was read. An int32 overflow is 22003; digits
// that run to the end of the string are unterminated.
function formatDigits(b: string, start: number): { val: number | null; next: number } {
  let i = start;
  let val = 0;
  let found = false;
  while (isDigit(b.charCodeAt(i))) {
    val = val * 10 + (b.charCodeAt(i) - 48);
    if (val > 0x7fffffff) throw formatNumberOutOfRange();
    i = formatAdvance(b, i);
    found = true;
  }
  return { val: found ? val : null, next: i };
}

// A parsed %… specifier, positioned at its conversion character (PostgreSQL's
// text_format_parse_format). argpos and widthpos are 1-based, null when absent; widthpos 0 means a *
// width taken from the next argument.
interface FormatSpec {
  argpos: number | null;
  widthpos: number | null;
  minus: boolean;
  width: number;
  at: number;
}

function formatParseSpec(b: string, start: number): FormatSpec {
  const spec: FormatSpec = { argpos: null, widthpos: null, minus: false, width: 0, at: start };
  let d = formatDigits(b, start);
  let i = d.next;
  if (d.val !== null) {
    if (b[i] !== "$") {
      // Just a width and the conversion character.
      spec.width = d.val;
      spec.at = i;
      return spec;
    }
    if (d.val === 0) throw formatArgZero();
    spec.argpos = d.val;
    i = formatAdvance(b, i);
  }
  while (b[i] === "-") {
    spec.minus = true;
    i = formatAdvance(b, i);
  }
  if (b[i] === "*") {
    i = formatAdvance(b, i);
    d = formatDigits(b, i);
    i = d.next;
    if (d.val !== null) {
      if (b[i] !== "$")
        throw engineError(
          "invalid_parameter_value",
          'width argument position must be ended by "$"',
        );
      if (d.val === 0) throw formatArgZero();
      spec.widthpos = d.val;
      i = formatAdvance(b, i);
    } else {
      spec.widthpos = 0;
    }
  } else {
    d = formatDigits(b, i);
    i = d.next;
    if (d.val !== null) spec.width = d.val;
  }
  spec.at = i;
  return spec;
}

// formatWidthValue reads a * width argument as an int32 (PostgreSQL reads an int2/int4 directly and
// converts any other type through its output text). NULL is width 0; an integer outside int32 is
// 22003; any other value goes through the text → i32 cast, so a non-integer is that cast's 22P02 /
// 22003.
function formatWidthValue(v: Value, zone: ZoneRef, m: Meter): number {
  if (v.kind === "null") return 0;
  if (v.kind === "int") {
    if (v.int < -0x80000000n || v.int > 0x7fffffffn) throw overflow("i32");
    return Number(v.int);
  }
  return Number(parseIntLiteral(outputText(v, zone, m), "i32"));
}

// formatText is format(formatstr, VARIADIC "any") (string-functions.md §6), PostgreSQL's text_format
// over args (the values after the format string). An amplifier (cost.md §8.1): the format string's
// UTF-8 bytes are charged as the input scan and guarded; one pass parses it and renders each
// converted argument (the specifier errors surface in scan order); the exact UTF-8 output size is
// then capped at MAX_RESULT_CHARS (54000), charged, guarded, and reserved before the result is
// built. Widths count code points, as PostgreSQL counts characters.
function formatText(b: string, args: Value[], zone: ZoneRef, m: Meter): string {
  m.charge(COSTS.scalarByte * BigInt(utf8Length(b)));
  m.guard();
  const nargs = args.length + 1; // the format string is argument 0
  let arg = 1; // the next argument to print
  const pieces: FormatPiece[] = [];
  let lit = 0;
  let i = 0;
  while (i < b.length) {
    if (b[i] !== "%") {
      i++;
      continue;
    }
    if (lit < i) pieces.push({ lit: true, lo: lit, hi: i });
    i = formatAdvance(b, i);
    if (b[i] === "%") {
      pieces.push({ lit: true, lo: i, hi: i + 1 });
      i++;
      lit = i;
      continue;
    }
    const spec = formatParseSpec(b, i);
    i = spec.at;
    const conv = b[i]!;
    if (conv !== "s" && conv !== "I" && conv !== "L") {
      const ch = String.fromCodePoint(b.codePointAt(i)!);
      throw engineError(
        "invalid_parameter_value",
        `unrecognized format() type specifier "${ch}"`,
      ).hinted(FORMAT_HINT);
    }
    let width = spec.width;
    if (spec.widthpos !== null) {
      if (spec.widthpos > 0) arg = spec.widthpos;
      if (arg >= nargs) throw formatTooFew();
      width = formatWidthValue(args[arg - 1]!, zone, m);
      arg++;
    }
    if (spec.argpos !== null) arg = spec.argpos;
    if (arg >= nargs) throw formatTooFew();
    const v = args[arg - 1]!;
    arg++;
    let text: string;
    if (v.kind === "null") {
      if (conv === "s") text = "";
      else if (conv === "L") text = "NULL";
      else
        throw engineError(
          "null_value_not_allowed",
          "null values cannot be formatted as an SQL identifier",
        );
    } else {
      const out = outputText(v, zone, m);
      text = conv === "I" ? quoteIdentText(out) : conv === "L" ? quoteLiteralText(out) : out;
    }
    let left = spec.minus;
    if (width < 0) {
      // A negative width is an implicit - flag; -INT_MIN is unrepresentable.
      if (width === -0x80000000) throw formatNumberOutOfRange();
      left = true;
      width = -width;
    }
    let chars = 0;
    for (const _ of text) chars++;
    pieces.push({ lit: false, text, pad: width > chars ? width - chars : 0, left });
    i++;
    lit = i;
  }
  if (lit < b.length) pieces.push({ lit: true, lo: lit, hi: b.length });
  let size = 0n;
  for (const p of pieces) {
    size += p.lit ? BigInt(utf8Length(b.slice(p.lo, p.hi))) : BigInt(utf8Length(p.text) + p.pad);
  }
  if (size > MAX_RESULT_CHARS)
    throw engineError("program_limit_exceeded", "requested length too large");
  m.charge(COSTS.scalarByte * size);
  m.guard();
  m.reserveScalar(size);
  const out: string[] = [];
  for (const p of pieces) {
    if (p.lit) out.push(b.slice(p.lo, p.hi));
    else if (p.left) out.push(p.text, " ".repeat(p.pad));
    else out.push(" ".repeat(p.pad), p.text);
  }
  return out.join("");
}
