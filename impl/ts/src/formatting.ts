// The data-type formatting functions — to_char, to_timestamp, to_date, to_number
// (spec/design/formatting.md). A transcription of PostgreSQL 18's formatting.c: the template parser
// (parse_format), the datetime renderer (DCH_to_char) and reader (DCH_from_char + do_to_timestamp).
// The number processor (NUM_processor) is formatting_num.ts. The template vocabulary is shared spec
// data (formatting_gen.ts from spec/types/formatting.toml); everything here is the hand-written logic
// that consumes it. jed has no locale, so every locale-dependent piece behaves as PostgreSQL does
// under the C locale: TM renders the English names.
//
// BYTES, not UTF-16: formatting.c works on UTF-8 bytes, and so do the Rust and Go cores. A JS string
// is UTF-16, so every template and input is first converted to a "byte string" — a string holding
// one char per UTF-8 byte (char codes 0..255) — and the algorithms run over that, index for index as
// the Go code runs over its byte strings; results are converted back with fromBytes. C int
// arithmetic is reproduced with int32 wrapping (`| 0`, Math.imul; PostgreSQL builds with -fwrapv) and
// unsigned int with `>>> 0`; i64 quantities (microseconds, strtol) are bigint.

import type { Meter } from "./cost.ts";
import { COSTS } from "./costs.ts";
import { engineError, type EngineError } from "./errors.ts";
import {
  DCH_A_D,
  DCH_A_M,
  DCH_AD,
  DCH_AM,
  DCH_B_C,
  DCH_BC,
  DCH_CC,
  DCH_D,
  DCH_DAY,
  DCH_DD,
  DCH_DDD,
  DCH_DY,
  DCH_Day,
  DCH_Dy,
  DCH_FF1,
  DCH_FF2,
  DCH_FF3,
  DCH_FF4,
  DCH_FF5,
  DCH_FF6,
  DCH_FX,
  DCH_HH,
  DCH_HH12,
  DCH_HH24,
  DCH_I,
  DCH_ID,
  DCH_IDDD,
  DCH_INDEX,
  DCH_IW,
  DCH_IY,
  DCH_IYY,
  DCH_IYYY,
  DCH_J,
  DCH_KEYWORDS,
  DCH_MI,
  DCH_MM,
  DCH_MON,
  DCH_MONTH,
  DCH_MS,
  DCH_Mon,
  DCH_Month,
  DCH_OF,
  DCH_P_M,
  DCH_PM,
  DCH_Q,
  DCH_RM,
  DCH_SS,
  DCH_SSSS,
  DCH_SUFFIXES,
  DCH_TZ,
  DCH_TZH,
  DCH_TZM,
  DCH_US,
  DCH_W,
  DCH_WW,
  DCH_Y,
  DCH_Y_YYY,
  DCH_YY,
  DCH_YYY,
  DCH_YYYY,
  DCH_a_d,
  DCH_a_m,
  DCH_ad,
  DCH_am,
  DCH_b_c,
  DCH_bc,
  DCH_day,
  DCH_dy,
  DCH_mon,
  DCH_month,
  DCH_p_m,
  DCH_pm,
  DCH_rm,
  DCH_tz,
  type FmtKeyword,
  type FmtSuffix,
  MODE_ISOWEEK,
  MODE_NONE,
  NUM_INDEX,
  NUM_KEYWORDS,
  SUFFIX_FM,
  SUFFIX_TH,
  SUFFIX_TM,
  SUFFIX_th,
  WORDS_ADBC,
  WORDS_ADBC_LONG,
  WORDS_AMPM,
  WORDS_AMPM_LONG,
  WORDS_DAYS_ABBREV,
  WORDS_DAYS_FULL,
  WORDS_MONTHS_ABBREV,
  WORDS_MONTHS_FULL,
  WORDS_ORDINAL_LOWER,
  WORDS_ORDINAL_UPPER,
  WORDS_RM_MONTHS_LOWER,
  WORDS_RM_MONTHS_UPPER,
} from "./formatting_gen.ts";
import { civilFromMicros, daysFromCivil, NEG_INFINITY, POS_INFINITY } from "./timestamp.ts";
import { DATE_NEG_INFINITY, DATE_POS_INFINITY } from "./date.ts";
import {
  instantToLocalMicros,
  localToInstantMicros,
  offsetAtRef,
  type ZoneRef,
} from "./timezone.ts";
import type { Interval } from "./interval.ts";
import { renderFloatPg } from "./value.ts";

// --- byte strings ------------------------------------------------------------------------------

const UTF8_ENC = new TextEncoder();
const UTF8_DEC = new TextDecoder();

// toBytes is s's UTF-8 encoding as a byte string (one char per byte).
export function toBytes(s: string): string {
  const b = UTF8_ENC.encode(s);
  let out = "";
  for (let i = 0; i < b.length; i += 8192) {
    out += String.fromCharCode(...b.subarray(i, i + 8192));
  }
  return out;
}

// fromBytes decodes a byte string's UTF-8 back to a JS string.
export function fromBytes(s: string): string {
  const b = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) b[i] = s.charCodeAt(i);
  return UTF8_DEC.decode(b);
}

// --- C int helpers -----------------------------------------------------------------------------

const INT32_MIN = -2147483648;
const INT32_MAX = 2147483647;
const I64_MIN = -9223372036854775808n;
const I64_MAX = 9223372036854775807n;
// TWO_63 is 2^63 as a double: the i64 range bound for a float.
const TWO_63 = 2 ** 63;

// div32 is C int division (truncating) of int32 operands.
function div32(a: number, b: number): number {
  return (a / b) | 0;
}

// --- template nodes ----------------------------------------------------------------------------

// The format-node kinds (formatting.c NODE_TYPE_*).
export const FN_END = 0;
export const FN_ACTION = 1;
export const FN_CHAR = 2;
export const FN_SEPARATOR = 3;
export const FN_SPACE = 4;

// FmtNode is one parsed template element: a pattern (key) with its suffix bits, or one literal
// character (chars, a whole UTF-8 sequence as a byte string).
export interface FmtNode {
  typ: number;
  chars: string;
  suffix: number;
  key: FmtKeyword | null;
}

// NumPrepare is the number template's NUMDesc_prepare hook (formatting_num.ts).
export interface NumPrepare {
  prepare(id: number): void;
}

// isSpaceC is C's isspace in the C locale (over a byte code).
export function isSpaceC(b: number): boolean {
  return b === 0x20 || b === 0x09 || b === 0x0a || b === 0x0b || b === 0x0c || b === 0x0d;
}

export function isDigitC(b: number): boolean {
  return b >= 0x30 && b <= 0x39;
}

function isAlphaC(b: number): boolean {
  return (b >= 0x41 && b <= 0x5a) || (b >= 0x61 && b <= 0x7a);
}

// isSeparatorChar is formatting.c's is_separator_char: printable ASCII that is not a letter or digit.
function isSeparatorChar(b: number): boolean {
  return b > 0x20 && b < 0x7f && !isAlphaC(b) && !isDigitC(b);
}

// pgMblen is PostgreSQL's pg_utf_mblen of the character at s[i], clipped to the string.
export function pgMblen(s: string, i: number): number {
  const c = s.charCodeAt(i);
  let n = 1;
  if ((c & 0x80) === 0) n = 1;
  else if ((c & 0xe0) === 0xc0) n = 2;
  else if ((c & 0xf0) === 0xe0) n = 3;
  else if ((c & 0xf8) === 0xf0) n = 4;
  if (i + n > s.length) n = s.length - i;
  return n;
}

// fmtIndexSearch is formatting.c's index_seq_search: the first keyword, in table order from the
// first one sharing s[pos]'s byte, whose name is a prefix of s[pos:].
function fmtIndexSearch(
  s: string,
  pos: number,
  kws: readonly FmtKeyword[],
  index: readonly number[],
): FmtKeyword | null {
  const c = s.charCodeAt(pos);
  if (c <= 0x20 || c >= 0x7e) return null;
  let k = index[c]!;
  if (k < 0) return null;
  for (;;) {
    if (s.startsWith(kws[k]!.name, pos)) return kws[k]!;
    k++;
    if (k === kws.length || kws[k]!.name.charCodeAt(0) !== c) return null;
  }
}

// fmtSuffixSearch is formatting.c's suff_search over DCH_suff for one suffix type.
function fmtSuffixSearch(s: string, pos: number, prefix: boolean): FmtSuffix | null {
  for (const sf of DCH_SUFFIXES) {
    if (sf.prefix === prefix && s.startsWith(sf.name, pos)) return sf;
  }
  return null;
}

// parseFormat is formatting.c's parse_format: the template (a byte string) as a node list ending in
// an FN_END node. A number template (dch = false) also feeds num, which can reject it (42601).
export function parseFormat(str: string, dch: boolean, num: NumPrepare | null): FmtNode[] {
  const kws = dch ? DCH_KEYWORDS : NUM_KEYWORDS;
  const index = dch ? DCH_INDEX : NUM_INDEX;
  const nodes: FmtNode[] = [];
  let i = 0;
  while (i < str.length) {
    let suffix = 0;
    if (dch) {
      const sf = fmtSuffixSearch(str, i, true);
      if (sf !== null) {
        suffix |= sf.flag;
        i += sf.name.length;
      }
    }
    if (i >= str.length) break;
    const k = fmtIndexSearch(str, i, kws, index);
    if (k !== null) {
      const n: FmtNode = { typ: FN_ACTION, chars: "", suffix, key: k };
      i += k.name.length;
      if (!dch) num!.prepare(k.id);
      if (dch && i < str.length) {
        const sf = fmtSuffixSearch(str, i, false);
        if (sf !== null) {
          n.suffix |= sf.flag;
          i += sf.name.length;
        }
      }
      nodes.push(n);
      continue;
    }
    if (str.charCodeAt(i) === 0x22) {
      // A double-quoted literal; a backslash quotes the next character.
      i++;
      while (i < str.length) {
        if (str.charCodeAt(i) === 0x22) {
          i++;
          break;
        }
        if (str.charCodeAt(i) === 0x5c && i + 1 < str.length) i++;
        const cl = pgMblen(str, i);
        nodes.push({ typ: FN_CHAR, chars: str.slice(i, i + cl), suffix: 0, key: null });
        i += cl;
      }
      continue;
    }
    // Outside quotes a backslash is special only before a double quote.
    if (str.charCodeAt(i) === 0x5c && i + 1 < str.length && str.charCodeAt(i + 1) === 0x22) i++;
    const cl = pgMblen(str, i);
    let typ = FN_CHAR;
    if (dch && isSeparatorChar(str.charCodeAt(i))) typ = FN_SEPARATOR;
    else if (isSpaceC(str.charCodeAt(i))) typ = FN_SPACE;
    nodes.push({ typ, chars: str.slice(i, i + cl), suffix: 0, key: null });
    i += cl;
  }
  nodes.push({ typ: FN_END, chars: "", suffix: 0, key: null });
  return nodes;
}

// --- C formatting helpers ----------------------------------------------------------------------

// cPadInt is C's "%0*d": v zero-padded to width w (the sign counts toward the width; 0 = none).
export function cPadInt(v: number | bigint, w: number): string {
  const s = String(v);
  if (s.length >= w) return s;
  if (s.charCodeAt(0) === 0x2d) return "-" + "0".repeat(w - s.length) + s.slice(1);
  return "0".repeat(w - s.length) + s;
}

// cPadRight is C's "%-*s": s space-padded on the right to w bytes.
function cPadRight(s: string, w: number): string {
  return s.length >= w ? s : s + " ".repeat(w - s.length);
}

// cPadLeft is C's "%*s": s space-padded on the left to w bytes.
export function cPadLeft(s: string, w: number): string {
  return s.length >= w ? s : " ".repeat(w - s.length) + s;
}

// fmtOrdinal is formatting.c's get_th: the ordinal suffix of a rendered number (22P02 when its
// last byte is not a digit).
export function fmtOrdinal(num: string, upper: boolean): string {
  let last = num.charCodeAt(num.length - 1);
  if (!isDigitC(last)) {
    throw engineError("invalid_text_representation", `${JSON.stringify(num)} is not a number`);
  }
  if (num.length > 1 && num.charCodeAt(num.length - 2) === 0x31) last = 0;
  let idx = 3;
  if (last === 0x31) idx = 0;
  else if (last === 0x32) idx = 1;
  else if (last === 0x33) idx = 2;
  return upper ? WORDS_ORDINAL_UPPER[idx]! : WORDS_ORDINAL_LOWER[idx]!;
}

// asciiUpperStr / asciiLowerStr are PostgreSQL's asc_toupper / asc_tolower (ASCII letters only,
// so a byte string's high bytes are untouched).
function asciiUpperStr(s: string): string {
  let out = "";
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    out += c >= 0x61 && c <= 0x7a ? String.fromCharCode(c - 32) : s[i];
  }
  return out;
}

export function asciiLowerStr(s: string): string {
  let out = "";
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    out += c >= 0x41 && c <= 0x5a ? String.fromCharCode(c + 32) : s[i];
  }
  return out;
}

// --- PostgreSQL's calendar helpers (datetime.c / timestamp.c), C int arithmetic ----------------

// pgDate2j is datetime.c's date2j: the Julian day of a (proleptic Gregorian) date, in wrapping
// int32 arithmetic.
function pgDate2j(year: number, month: number, day: number): number {
  if (month > 2) {
    month = (month + 1) | 0;
    year = (year + 4800) | 0;
  } else {
    month = (month + 13) | 0;
    year = (year + 4799) | 0;
  }
  const century = div32(year, 100);
  let julian = (Math.imul(year, 365) - 32167) | 0;
  julian = (julian + ((div32(year, 4) - century + div32(century, 4)) | 0)) | 0;
  julian = (julian + ((div32(Math.imul(7834, month), 256) + day) | 0)) | 0;
  return julian;
}

// pgJ2date is datetime.c's j2date (unsigned int arithmetic).
function pgJ2date(jd: number): [number, number, number] {
  let julian = jd >>> 0;
  julian = (julian + 32044) >>> 0;
  let quad = Math.floor(julian / 146097);
  const extra = (Math.imul((julian - quad * 146097) >>> 0, 4) + 3) >>> 0;
  julian = (julian + 60 + Math.imul(quad, 3) + Math.floor(extra / 146097)) >>> 0;
  quad = Math.floor(julian / 1461);
  julian = (julian - quad * 1461) >>> 0;
  let y = Math.floor((Math.imul(julian, 4) >>> 0) / 1461) | 0;
  if (y !== 0) julian = ((julian + 305) % 365) + 123;
  else julian = ((julian + 306) % 366) + 123;
  y = (y + Math.imul(quad, 4)) | 0;
  const year = (y - 4800) | 0;
  quad = Math.floor((Math.imul(julian, 2141) >>> 0) / 65536);
  const day = ((julian - Math.floor((Math.imul(7834, quad) >>> 0) / 256)) >>> 0) | 0;
  const month = ((quad + 10) % 12) + 1;
  return [year, month, day];
}

// pgJ2day is datetime.c's j2day: 0..6 = Sunday..Saturday.
function pgJ2day(date: number): number {
  date = (date + 1) | 0;
  date %= 7;
  if (date < 0) date += 7;
  return date;
}

// pgIsoweek2j is timestamp.c's isoweek2j.
function pgIsoweek2j(year: number, week: number): number {
  const day4 = pgDate2j(year, 1, 4);
  const day0 = pgJ2day((day4 - 1) | 0);
  return (Math.imul((week - 1) | 0, 7) + ((day4 - day0) | 0)) | 0;
}

// pgDate2isoweek is timestamp.c's date2isoweek.
function pgDate2isoweek(year: number, mon: number, mday: number): number {
  const dayn = pgDate2j(year, mon, mday);
  let day4 = pgDate2j(year, 1, 4);
  let day0 = pgJ2day((day4 - 1) | 0);
  if (dayn < ((day4 - day0) | 0)) {
    day4 = pgDate2j((year - 1) | 0, 1, 4);
    day0 = pgJ2day((day4 - 1) | 0);
  }
  let result = (div32((dayn - ((day4 - day0) | 0)) | 0, 7) + 1) | 0;
  if (result >= 52) {
    day4 = pgDate2j((year + 1) | 0, 1, 4);
    day0 = pgJ2day((day4 - 1) | 0);
    if (dayn >= ((day4 - day0) | 0)) {
      result = (div32((dayn - ((day4 - day0) | 0)) | 0, 7) + 1) | 0;
    }
  }
  return result;
}

// pgDate2isoyear is timestamp.c's date2isoyear.
function pgDate2isoyear(year: number, mon: number, mday: number): number {
  const dayn = pgDate2j(year, mon, mday);
  let day4 = pgDate2j(year, 1, 4);
  let day0 = pgJ2day((day4 - 1) | 0);
  if (dayn < ((day4 - day0) | 0)) {
    day4 = pgDate2j((year - 1) | 0, 1, 4);
    day0 = pgJ2day((day4 - 1) | 0);
    year = (year - 1) | 0;
  }
  const result = (div32((dayn - ((day4 - day0) | 0)) | 0, 7) + 1) | 0;
  if (result >= 52) {
    day4 = pgDate2j((year + 1) | 0, 1, 4);
    day0 = pgJ2day((day4 - 1) | 0);
    if (dayn >= ((day4 - day0) | 0)) year = (year + 1) | 0;
  }
  return year;
}

// pgDate2isoyearday is timestamp.c's date2isoyearday.
function pgDate2isoyearday(year: number, mon: number, mday: number): number {
  return (pgDate2j(year, mon, mday) - pgIsoweek2j(pgDate2isoyear(year, mon, mday), 1) + 1) | 0;
}

// pgIsLeap is PostgreSQL's isleap.
function pgIsLeap(y: number): boolean {
  return y % 4 === 0 && (y % 100 !== 0 || y % 400 === 0);
}

const PG_DAY_TAB: readonly (readonly number[])[] = [
  [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
  [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
];

// --- to_char for datetimes and intervals (DCH_to_char) -----------------------------------------

// FmtTm is formatting.c's fmt_tm: a broken-down datetime or interval. hour may exceed the int
// range for an interval (it stays exact in a double); the rest are C ints.
interface FmtTm {
  sec: number;
  min: number;
  hour: number;
  mday: number;
  mon: number;
  year: number;
  wday: number;
  yday: number;
  gmtoff: number;
}

function invalidForInterval(): EngineError {
  return engineError(
    "invalid_datetime_format",
    "invalid format specification for an interval value",
  );
}

// adjustYear is formatting.c's ADJUST_YEAR: a BC year shown positive (an interval's as is).
function adjustYear(year: number, isInterval: boolean): number {
  if (isInterval) return year;
  return year <= 0 ? -((year - 1) | 0) | 0 : year;
}

// caseOf maps a name pattern's id to its case: 1 upper, 2 lower, 0 the name as written (initcap).
function caseOf(id: number, upper: number, lower: number): number {
  if (id === upper) return 1;
  if (id === lower) return 2;
  return 0;
}

function offsetSign(gmtoff: number): string {
  return gmtoff >= 0 ? "+" : "-";
}

// dchToChar is formatting.c's DCH_to_char; the result is a byte string.
function dchToChar(
  nodes: FmtNode[],
  isInterval: boolean,
  tm: FmtTm,
  fsec: number,
  tzn: string | null,
): string {
  const parts: string[] = [];
  const fm = (n: FmtNode): boolean => (n.suffix & SUFFIX_FM) !== 0;
  const tmS = (n: FmtNode): boolean => (n.suffix & SUFFIX_TM) !== 0;
  // width is a "%0*d" width: 0 in fill mode, else w (w+1 for a negative value).
  const width = (n: FmtNode, w: number, neg: boolean): number => {
    if (fm(n)) return 0;
    return neg ? w + 1 : w;
  };
  // num writes a number with its optional ordinal suffix (TH / th).
  const num = (n: FmtNode, s: string): void => {
    if ((n.suffix & (SUFFIX_TH | SUFFIX_th)) !== 0) {
      s += fmtOrdinal(s, (n.suffix & SUFFIX_TH) !== 0);
    }
    parts.push(s);
  };
  // word writes a padded (or, with TM / FM, bare) month or day name in the requested case.
  const word = (n: FmtNode, name: string, caseMode: number, pad: boolean): void => {
    if (caseMode === 1) name = asciiUpperStr(name);
    else if (caseMode === 2) name = asciiLowerStr(name);
    if (pad && !tmS(n) && !fm(n)) name = cPadRight(name, 9);
    parts.push(name);
  };
  const absOff = Math.abs(tm.gmtoff);
  const pm = tm.hour % 24 >= 12;
  for (const n of nodes) {
    if (n.typ === FN_END) break;
    if (n.typ !== FN_ACTION) {
      parts.push(n.chars);
      continue;
    }
    const id = n.key!.id;
    switch (id) {
      case DCH_A_M:
      case DCH_P_M:
        parts.push(pm ? "P.M." : "A.M.");
        break;
      case DCH_AM:
      case DCH_PM:
        parts.push(pm ? "PM" : "AM");
        break;
      case DCH_a_m:
      case DCH_p_m:
        parts.push(pm ? "p.m." : "a.m.");
        break;
      case DCH_am:
      case DCH_pm:
        parts.push(pm ? "pm" : "am");
        break;
      case DCH_HH:
      case DCH_HH12: {
        let v = tm.hour % 12;
        if (v === 0) v = 12;
        num(n, cPadInt(v, width(n, 2, tm.hour < 0)));
        break;
      }
      case DCH_HH24:
        num(n, cPadInt(tm.hour, width(n, 2, tm.hour < 0)));
        break;
      case DCH_MI:
        num(n, cPadInt(tm.min, width(n, 2, tm.min < 0)));
        break;
      case DCH_SS:
        num(n, cPadInt(tm.sec, width(n, 2, tm.sec < 0)));
        break;
      case DCH_FF1:
        num(n, cPadInt(Math.trunc(fsec / 100000), 1));
        break;
      case DCH_FF2:
        num(n, cPadInt(Math.trunc(fsec / 10000), 2));
        break;
      case DCH_FF3:
      case DCH_MS:
        num(n, cPadInt(Math.trunc(fsec / 1000), 3));
        break;
      case DCH_FF4:
        num(n, cPadInt(Math.trunc(fsec / 100), 4));
        break;
      case DCH_FF5:
        num(n, cPadInt(Math.trunc(fsec / 10), 5));
        break;
      case DCH_FF6:
      case DCH_US:
        num(n, cPadInt(fsec, 6));
        break;
      case DCH_SSSS:
        num(n, String(tm.hour * 3600 + tm.min * 60 + tm.sec));
        break;
      case DCH_tz:
      case DCH_TZ:
        if (isInterval) throw invalidForInterval();
        if (tzn !== null) {
          let p = tzn;
          if (id === DCH_tz) p = asciiLowerStr(p);
          if (p.length > n.key!.name.length * 12) {
            throw engineError("datetime_field_overflow", "time zone format value too long");
          }
          parts.push(p);
        }
        break;
      case DCH_TZH:
        if (isInterval) throw invalidForInterval();
        parts.push(offsetSign(tm.gmtoff) + cPadInt(Math.trunc(absOff / 3600), 2));
        break;
      case DCH_TZM:
        if (isInterval) throw invalidForInterval();
        parts.push(cPadInt(Math.trunc((absOff % 3600) / 60), 2));
        break;
      case DCH_OF: {
        if (isInterval) throw invalidForInterval();
        const w = fm(n) ? 0 : 2;
        parts.push(offsetSign(tm.gmtoff) + cPadInt(Math.trunc(absOff / 3600), w));
        if (absOff % 3600 !== 0) parts.push(":" + cPadInt(Math.trunc((absOff % 3600) / 60), 2));
        break;
      }
      case DCH_A_D:
      case DCH_B_C:
      case DCH_AD:
      case DCH_BC:
      case DCH_a_d:
      case DCH_b_c:
      case DCH_ad:
      case DCH_bc: {
        if (isInterval) throw invalidForInterval();
        const bc = tm.year <= 0;
        let s: string;
        if (id === DCH_A_D || id === DCH_B_C) s = bc ? "B.C." : "A.D.";
        else if (id === DCH_AD || id === DCH_BC) s = bc ? "BC" : "AD";
        else if (id === DCH_a_d || id === DCH_b_c) s = bc ? "b.c." : "a.d.";
        else s = bc ? "bc" : "ad";
        parts.push(s);
        break;
      }
      case DCH_MONTH:
      case DCH_Month:
      case DCH_month:
        if (isInterval) throw invalidForInterval();
        if (tm.mon === 0) break;
        word(n, WORDS_MONTHS_FULL[tm.mon - 1]!, caseOf(id, DCH_MONTH, DCH_month), true);
        break;
      case DCH_MON:
      case DCH_Mon:
      case DCH_mon:
        if (isInterval) throw invalidForInterval();
        if (tm.mon === 0) break;
        word(n, WORDS_MONTHS_ABBREV[tm.mon - 1]!, caseOf(id, DCH_MON, DCH_mon), false);
        break;
      case DCH_MM:
        num(n, cPadInt(tm.mon, width(n, 2, tm.mon < 0)));
        break;
      case DCH_DAY:
      case DCH_Day:
      case DCH_day:
        if (isInterval) throw invalidForInterval();
        word(n, WORDS_DAYS_FULL[tm.wday]!, caseOf(id, DCH_DAY, DCH_day), true);
        break;
      case DCH_DY:
      case DCH_Dy:
      case DCH_dy:
        if (isInterval) throw invalidForInterval();
        word(n, WORDS_DAYS_ABBREV[tm.wday]!, caseOf(id, DCH_DY, DCH_dy), false);
        break;
      case DCH_DDD:
      case DCH_IDDD: {
        const v = id === DCH_IDDD ? pgDate2isoyearday(tm.year, tm.mon, tm.mday) : tm.yday;
        num(n, cPadInt(v, width(n, 3, false)));
        break;
      }
      case DCH_DD:
        num(n, cPadInt(tm.mday, width(n, 2, false)));
        break;
      case DCH_D:
        if (isInterval) throw invalidForInterval();
        num(n, String(tm.wday + 1));
        break;
      case DCH_ID: {
        if (isInterval) throw invalidForInterval();
        num(n, String(tm.wday === 0 ? 7 : tm.wday));
        break;
      }
      case DCH_WW:
        num(n, cPadInt((div32((tm.yday - 1) | 0, 7) + 1) | 0, width(n, 2, false)));
        break;
      case DCH_IW:
        num(n, cPadInt(pgDate2isoweek(tm.year, tm.mon, tm.mday), width(n, 2, false)));
        break;
      case DCH_Q:
        if (tm.mon === 0) break;
        num(n, String(div32(tm.mon - 1, 3) + 1));
        break;
      case DCH_CC: {
        let c: number;
        if (isInterval) c = div32(tm.year, 100);
        else if (tm.year > 0) c = div32(tm.year - 1, 100) + 1;
        else c = div32(tm.year, 100) - 1;
        if (c <= 99 && c >= -99) num(n, cPadInt(c, width(n, 2, c < 0)));
        else num(n, String(c));
        break;
      }
      case DCH_Y_YYY: {
        const y = adjustYear(tm.year, isInterval);
        const k = div32(y, 1000);
        num(n, String(k) + "," + cPadInt(y - k * 1000, 3));
        break;
      }
      case DCH_YYYY:
      case DCH_IYYY:
      case DCH_YYY:
      case DCH_IYY:
      case DCH_YY:
      case DCH_IY:
      case DCH_Y:
      case DCH_I: {
        const adj = adjustYear(tm.year, isInterval);
        let y = adj;
        if (id === DCH_IYYY || id === DCH_IYY || id === DCH_IY || id === DCH_I) {
          y = adjustYear(pgDate2isoyear(tm.year, tm.mon, tm.mday), isInterval);
        }
        let w: number;
        if (id === DCH_YYYY || id === DCH_IYYY) {
          w = width(n, 4, adj < 0);
        } else if (id === DCH_YYY || id === DCH_IYY) {
          y %= 1000;
          w = width(n, 3, adj < 0);
        } else if (id === DCH_YY || id === DCH_IY) {
          y %= 100;
          w = width(n, 2, adj < 0);
        } else {
          y %= 10;
          w = 1;
        }
        num(n, cPadInt(y, w));
        break;
      }
      case DCH_RM:
      case DCH_rm: {
        if (tm.mon === 0 && tm.year === 0) break;
        const months = id === DCH_rm ? WORDS_RM_MONTHS_LOWER : WORDS_RM_MONTHS_UPPER;
        let mon: number;
        if (tm.mon === 0) mon = tm.year >= 0 ? 0 : 11;
        else if (tm.mon < 0) mon = -1 * (tm.mon + 1);
        else mon = 12 - tm.mon;
        let s = months[mon]!;
        if (!fm(n)) s = cPadRight(s, 4);
        parts.push(s);
        break;
      }
      case DCH_W:
        num(n, String(div32(tm.mday - 1, 7) + 1));
        break;
      case DCH_J:
        num(n, String(pgDate2j(tm.year, tm.mon, tm.mday)));
        break;
    }
  }
  return parts.join("");
}

// tmFromWall breaks down a wall-clock microsecond reading (PostgreSQL's timestamp2tm plus the wday
// and yday to_char computes). fsec is the non-negative microsecond of the second.
function tmFromWall(micros: bigint): [FmtTm, number] {
  const [y, mo, d, h, mi, s, us] = civilFromMicros(micros);
  const tm: FmtTm = {
    sec: Number(s),
    min: Number(mi),
    hour: Number(h),
    mday: Number(d),
    mon: Number(mo),
    year: Number(BigInt.asIntN(32, y)),
    wday: 0,
    yday: 0,
    gmtoff: 0,
  };
  const thisdate = pgDate2j(tm.year, tm.mon, tm.mday);
  // PostgreSQL's (thisdate + 1) % 7; a floor modulo beyond its range, where a Julian day is negative.
  tm.wday = (((thisdate + 1) % 7) + 7) % 7;
  tm.yday = (thisdate - pgDate2j(tm.year, 1, 1) + 1) | 0;
  return [tm, Number(us)];
}

// datetimeToChar is formatting.c's datetime_to_char_body: an empty template is NULL.
function datetimeToChar(
  tm: FmtTm,
  fsec: number,
  tzn: string | null,
  tmpl: string,
  isInterval: boolean,
): string | null {
  if (tmpl === "") return null;
  const nodes = parseFormat(toBytes(tmpl), true, null);
  return fromBytes(dchToChar(nodes, isInterval, tm, fsec, tzn));
}

// toCharTimestamp is to_char(timestamp, text): ±infinity is NULL.
export function toCharTimestamp(micros: bigint, tmpl: string): string | null {
  if (tmpl === "" || micros === POS_INFINITY || micros === NEG_INFINITY) return null;
  const [tm, fsec] = tmFromWall(micros);
  return datetimeToChar(tm, fsec, null, tmpl, false);
}

// toCharTimestamptz is to_char(timestamptz, text): the instant decomposed in zone, with its offset
// (OF, TZH, TZM) and abbreviation (TZ). A fixed-offset zone other than UTC has no abbreviation, as
// PostgreSQL's ±HH:MM zone spelling has none. ±infinity is NULL. One timezone unit.
export function toCharTimestamptz(
  instant: bigint,
  zone: ZoneRef,
  tmpl: string,
  m: Meter,
): string | null {
  if (tmpl === "" || instant === POS_INFINITY || instant === NEG_INFINITY) return null;
  m.charge(COSTS.timezone);
  m.guard();
  const local = instantToLocalMicros(zone, instant);
  const secs = instant >= 0n ? instant / 1_000_000n : -((-instant + 999_999n) / 1_000_000n);
  const off = offsetAtRef(zone, secs);
  const [tm, fsec] = tmFromWall(local);
  tm.gmtoff = off.utoff;
  let tzn = toBytes(off.abbrev);
  if (zone.fixed) tzn = zone.off === 0 ? "UTC" : "";
  return datetimeToChar(tm, fsec, tzn, tmpl, false);
}

// toCharInterval is to_char(interval, text) — PostgreSQL's interval2itm decomposition, with yday
// the approximate span in days.
export function toCharInterval(iv: Interval, tmpl: string): string | null {
  if (tmpl === "") return null;
  let t = iv.micros;
  const hour = t / 3_600_000_000n;
  t -= hour * 3_600_000_000n;
  const min = t / 60_000_000n;
  t -= min * 60_000_000n;
  const sec = t / 1_000_000n;
  const usec = t - sec * 1_000_000n;
  const tm: FmtTm = {
    sec: Number(sec),
    min: Number(min),
    hour: Number(hour),
    mday: iv.days,
    mon: iv.months % 12,
    year: div32(iv.months, 12),
    wday: 0,
    yday: 0,
    gmtoff: 0,
  };
  tm.yday = (Math.imul((Math.imul(tm.year, 12) + tm.mon) | 0, 30) + tm.mday) | 0;
  return datetimeToChar(tm, Number(usec), null, tmpl, true);
}

// --- to_timestamp / to_date (DCH_from_char + do_to_timestamp) ----------------------------------

// TmFromChar is formatting.c's TmFromChar: the fields a template read (0 = absent).
interface TmFromChar {
  mode: number;
  hh: number;
  pm: number;
  mi: number;
  ss: number;
  ssss: number;
  d: number;
  dd: number;
  ddd: number;
  mm: number;
  ms: number;
  year: number;
  bc: number;
  ww: number;
  w: number;
  cc: number;
  j: number;
  us: number;
  yysz: number;
  clock: number;
  tzsign: number;
  tzh: number;
  tzm: number;
  ff: number;
}

type TmField = Exclude<keyof TmFromChar, "mode">;

function newTmFromChar(): TmFromChar {
  return {
    mode: MODE_NONE,
    hh: 0,
    pm: 0,
    mi: 0,
    ss: 0,
    ssss: 0,
    d: 0,
    dd: 0,
    ddd: 0,
    mm: 0,
    ms: 0,
    year: 0,
    bc: 0,
    ww: 0,
    w: 0,
    cc: 0,
    j: 0,
    us: 0,
    yysz: 0,
    clock: 0,
    tzsign: 0,
    tzh: 0,
    tzm: 0,
    ff: 0,
  };
}

function setMode(t: TmFromChar, mode: number): void {
  if (mode !== MODE_NONE) {
    if (t.mode === MODE_NONE) t.mode = mode;
    else if (t.mode !== mode) {
      throw engineError(
        "invalid_datetime_format",
        "invalid combination of date conventions",
      ).hinted("Do not mix Gregorian and ISO week date conventions in a formatting template.");
    }
  }
}

// fromCharSetInt is formatting.c's from_char_set_int: a field may be set twice only to one value.
function fromCharSetInt(out: TmFromChar, dest: TmField, value: number, n: FmtNode): void {
  if (out[dest] !== 0 && out[dest] !== value) {
    throw engineError(
      "invalid_datetime_format",
      `conflicting values for "${n.key!.name}" field in formatting string`,
    ).withDetail("This value contradicts a previous setting for the same field type.");
  }
  out[dest] = value;
}

// cStrtol is C's strtol(s + start, &end, 10): skip whitespace, an optional sign, then digits; no
// digits leaves end at start. erange reports a saturated overflow.
function cStrtol(s: string, start: number): [bigint, number, boolean] {
  let i = start;
  while (i < s.length && isSpaceC(s.charCodeAt(i))) i++;
  let neg = false;
  if (i < s.length && (s.charCodeAt(i) === 0x2b || s.charCodeAt(i) === 0x2d)) {
    neg = s.charCodeAt(i) === 0x2d;
    i++;
  }
  const ds = i;
  let acc = 0n;
  const limit = neg ? I64_MAX + 1n : I64_MAX;
  let over = false;
  while (i < s.length && isDigitC(s.charCodeAt(i))) {
    const d = BigInt(s.charCodeAt(i) - 0x30);
    if (!over && acc > (limit - d) / 10n) over = true;
    if (!over) acc = acc * 10n + d;
    i++;
  }
  if (i === ds) return [0n, start, false];
  if (over) return [neg ? I64_MIN : I64_MAX, i, true];
  return [neg ? -acc : acc, i, false];
}

function invalidValue(cp: string, n: FmtNode, detail: string): EngineError {
  return engineError(
    "invalid_datetime_format",
    `invalid value "${fromBytes(cp)}" for "${n.key!.name}"`,
  ).withDetail(detail);
}

// FM_HINT is formatting.c's hint on a fixed-width field the source string cannot fill.
const FM_HINT = 'If your source string is not fixed-width, try using the "FM" modifier.';

// DchReader is DCH_from_char's state: the input (a byte string), the position, and the node list.
class DchReader {
  pos = 0;
  readonly input: string;
  readonly nodes: FmtNode[];
  readonly out: TmFromChar;
  constructor(input: string, nodes: FmtNode[], out: TmFromChar) {
    this.input = input;
    this.nodes = nodes;
    this.out = out;
  }

  // isNextSeparator is formatting.c's is_next_separator: whether the field at node i is followed by
  // a non-digit, so a fixed-width read may take as many digits as are present.
  isNextSeparator(i: number): boolean {
    const n = this.nodes[i]!;
    if (n.typ === FN_END) return false;
    if (n.typ === FN_ACTION && (n.suffix & (SUFFIX_TH | SUFFIX_th)) !== 0) return true;
    const next = this.nodes[i + 1]!;
    if (next.typ === FN_END) return true;
    if (next.typ === FN_ACTION) return !next.key!.isDigit;
    if (next.chars.length === 1 && isDigitC(next.chars.charCodeAt(0))) return false;
    return true;
  }

  // parseIntLen is formatting.c's from_char_parse_int_len: read an integer field of width ln (any
  // width in fill mode or before a separator). dest may be null (the value is checked, then
  // dropped). Returns the bytes consumed.
  parseIntLen(dest: TmField | null, ln: number, i: number): number {
    const n = this.nodes[i]!;
    const input = this.input;
    const init = this.pos;
    while (this.pos < input.length && isSpaceC(input.charCodeAt(this.pos))) this.pos++;
    const end = Math.min(this.pos + ln, input.length);
    const cp = input.slice(this.pos, end);
    const used = input.length - this.pos;
    let result: bigint;
    let erange: boolean;
    if ((n.suffix & SUFFIX_FM) !== 0 || this.isNextSeparator(i)) {
      let e: number;
      [result, e, erange] = cStrtol(input, init);
      this.pos = e;
    } else {
      if (used < ln) {
        throw engineError(
          "invalid_datetime_format",
          `source string too short for "${n.key!.name}" formatting field`,
        )
          .withDetail(`Field requires ${ln} characters, but only ${used} remain.`)
          .hinted(FM_HINT);
      }
      let last: number;
      [result, last, erange] = cStrtol(cp, 0);
      if (last > 0 && last < ln) {
        throw invalidValue(
          cp,
          n,
          `Field requires ${ln} characters, but only ${last} could be parsed.`,
        ).hinted(FM_HINT);
      }
      this.pos += last;
    }
    if (this.pos === init) throw invalidValue(cp, n, "Value must be an integer.");
    if (erange || result < BigInt(INT32_MIN) || result > BigInt(INT32_MAX)) {
      throw engineError(
        "datetime_field_overflow",
        `value for "${n.key!.name}" in source string is out of range`,
      ).withDetail(`Value must be in the range ${INT32_MIN} to ${INT32_MAX}.`);
    }
    if (dest !== null) fromCharSetInt(this.out, dest, Number(result), n);
    return this.pos - init;
  }

  parseInt(dest: TmField | null, i: number): number {
    return this.parseIntLen(dest, this.nodes[i]!.key!.name.length, i);
  }

  // seqSearch is formatting.c's from_char_seq_search. The TM (translated) form searches the same
  // English words — jed's only locale is C — with ASCII case folding.
  seqSearch(words: readonly string[], i: number): number {
    const rest = this.input.slice(this.pos);
    const [idx, ln] = seqSearchASCII(rest, words);
    if (ln <= 0) {
      let cp = rest;
      for (let k = 0; k < cp.length; k++) {
        if (isSpaceC(cp.charCodeAt(k))) {
          cp = cp.slice(0, k);
          break;
        }
      }
      throw invalidValue(
        cp,
        this.nodes[i]!,
        "The given value did not match any of the allowed values for this field.",
      );
    }
    this.pos += ln;
    return idx;
  }

  // skipTH is formatting.c's SKIP_THth: an ordinal suffix on a field skips two input characters.
  skipTH(suffix: number): void {
    if ((suffix & (SUFFIX_TH | SUFFIX_th)) !== 0) {
      for (let k = 0; k < 2; k++) {
        if (this.pos < this.input.length) this.pos += pgMblen(this.input, this.pos);
      }
    }
  }
}

// seqSearchASCII is formatting.c's seq_search_ascii: the first word that, ASCII case-insensitively,
// is a prefix of s; its index and length, or -1.
function seqSearchASCII(s: string, words: readonly string[]): [number, number] {
  if (s === "") return [-1, 0];
  for (let wi = 0; wi < words.length; wi++) {
    const w = words[wi]!;
    if (w.length <= s.length && asciiLowerStr(s.slice(0, w.length)) === asciiLowerStr(w)) {
      return [wi, w.length];
    }
  }
  return [-1, 0];
}

// scanYYYY is sscanf(s, "%d,%03d%n"): the matched count, both numbers, and the bytes consumed.
function scanYYYY(s: string): [number, number, number, number] {
  const [v, e] = cStrtol(s, 0);
  if (e === 0) return [0, 0, 0, 0];
  const millennia = Number(BigInt.asIntN(32, v));
  if (e >= s.length || s.charCodeAt(e) !== 0x2c) return [1, millennia, 0, 0];
  let i = e + 1;
  while (i < s.length && isSpaceC(s.charCodeAt(i))) i++;
  const end = Math.min(i + 3, s.length);
  const [v2, e2] = cStrtol(s.slice(0, end), i);
  if (e2 === i) return [1, millennia, 0, 0];
  return [2, millennia, Number(BigInt.asIntN(32, v2)), e2];
}

// dchFromChar is formatting.c's DCH_from_char (non-standard mode).
function dchFromChar(nodes: FmtNode[], input: string, out: TmFromChar): void {
  const r = new DchReader(input, nodes, out);
  let fxMode = false;
  let extraSkip = 0;
  for (let i = 0; nodes[i]!.typ !== FN_END && r.pos < input.length; i++) {
    const n = nodes[i]!;
    if (
      !fxMode &&
      (n.typ !== FN_ACTION || n.key!.id !== DCH_FX) &&
      (n.typ === FN_ACTION || i === 0)
    ) {
      while (r.pos < input.length && isSpaceC(input.charCodeAt(r.pos))) {
        r.pos++;
        extraSkip++;
      }
    }
    if (n.typ === FN_SPACE || n.typ === FN_SEPARATOR) {
      if (!fxMode) {
        extraSkip--;
        if (
          r.pos < input.length &&
          (isSpaceC(input.charCodeAt(r.pos)) || isSeparatorChar(input.charCodeAt(r.pos)))
        ) {
          r.pos++;
          extraSkip++;
        }
      } else if (r.pos < input.length) {
        r.pos += pgMblen(input, r.pos);
      }
      continue;
    } else if (n.typ !== FN_ACTION) {
      if (!fxMode && extraSkip > 0) extraSkip--;
      else if (r.pos < input.length) r.pos += pgMblen(input, r.pos);
      continue;
    }
    const key = n.key!;
    setMode(out, key.dateMode);
    const id = key.id;
    switch (id) {
      case DCH_FX:
        fxMode = true;
        break;
      case DCH_A_M:
      case DCH_P_M:
      case DCH_a_m:
      case DCH_p_m:
      case DCH_AM:
      case DCH_PM:
      case DCH_am:
      case DCH_pm: {
        const long = id === DCH_A_M || id === DCH_P_M || id === DCH_a_m || id === DCH_p_m;
        const v = r.seqSearch(long ? WORDS_AMPM_LONG : WORDS_AMPM, i);
        fromCharSetInt(out, "pm", v % 2, n);
        out.clock = 12;
        break;
      }
      case DCH_HH:
      case DCH_HH12:
        r.parseIntLen("hh", 2, i);
        out.clock = 12;
        r.skipTH(n.suffix);
        break;
      case DCH_HH24:
        r.parseIntLen("hh", 2, i);
        r.skipTH(n.suffix);
        break;
      case DCH_MI:
        r.parseInt("mi", i);
        r.skipTH(n.suffix);
        break;
      case DCH_SS:
        r.parseInt("ss", i);
        r.skipTH(n.suffix);
        break;
      case DCH_MS: {
        const ln = r.parseIntLen("ms", 3, i);
        if (ln === 1) out.ms = Math.imul(out.ms, 100);
        else if (ln === 2) out.ms = Math.imul(out.ms, 10);
        r.skipTH(n.suffix);
        break;
      }
      case DCH_FF1:
      case DCH_FF2:
      case DCH_FF3:
      case DCH_FF4:
      case DCH_FF5:
      case DCH_FF6:
      case DCH_US: {
        let w = 6;
        if (id !== DCH_US) {
          out.ff = id - DCH_FF1 + 1;
          w = out.ff;
        }
        const ln = r.parseIntLen("us", w, i);
        const mul = [1, 100000, 10000, 1000, 100, 10][ln];
        if (mul !== undefined && mul !== 1) out.us = Math.imul(out.us, mul);
        r.skipTH(n.suffix);
        break;
      }
      case DCH_SSSS:
        r.parseInt("ssss", i);
        r.skipTH(n.suffix);
        break;
      case DCH_tz:
      case DCH_TZ:
      case DCH_OF:
      case DCH_TZH: {
        if (
          (id === DCH_tz || id === DCH_TZ) &&
          r.pos < input.length &&
          isAlphaC(input.charCodeAt(r.pos))
        ) {
          // A zone abbreviation: jed has no abbreviation table (timezones.md §15).
          throw engineError(
            "feature_not_supported",
            "time zone abbreviations are not supported in to_timestamp input",
          );
        }
        // OF / TZ read TZH[:TZM]; a sign may have been skipped as a separator (TZH).
        const c = r.pos < input.length ? input.charCodeAt(r.pos) : -1;
        if (c === 0x2b || c === 0x2d || c === 0x20) {
          out.tzsign = c === 0x2d ? -1 : 1;
          r.pos++;
        } else if (extraSkip > 0 && r.pos > 0 && input.charCodeAt(r.pos - 1) === 0x2d) {
          out.tzsign = -1;
        } else {
          out.tzsign = 1;
        }
        r.parseIntLen("tzh", 2, i);
        if (id !== DCH_TZH && r.pos < input.length && input.charCodeAt(r.pos) === 0x3a) {
          r.pos++;
          r.parseIntLen("tzm", 2, i);
        }
        break;
      }
      case DCH_TZM:
        if (out.tzsign === 0) out.tzsign = 1;
        r.parseIntLen("tzm", 2, i);
        break;
      case DCH_A_D:
      case DCH_B_C:
      case DCH_a_d:
      case DCH_b_c:
      case DCH_AD:
      case DCH_BC:
      case DCH_ad:
      case DCH_bc: {
        const long = id === DCH_A_D || id === DCH_B_C || id === DCH_a_d || id === DCH_b_c;
        const v = r.seqSearch(long ? WORDS_ADBC_LONG : WORDS_ADBC, i);
        fromCharSetInt(out, "bc", v % 2, n);
        break;
      }
      case DCH_MONTH:
      case DCH_Month:
      case DCH_month: {
        const v = r.seqSearch(WORDS_MONTHS_FULL, i);
        fromCharSetInt(out, "mm", v + 1, n);
        break;
      }
      case DCH_MON:
      case DCH_Mon:
      case DCH_mon: {
        const v = r.seqSearch(WORDS_MONTHS_ABBREV, i);
        fromCharSetInt(out, "mm", v + 1, n);
        break;
      }
      case DCH_MM:
        r.parseInt("mm", i);
        r.skipTH(n.suffix);
        break;
      case DCH_DAY:
      case DCH_Day:
      case DCH_day:
      case DCH_DY:
      case DCH_Dy:
      case DCH_dy: {
        const abbrev = id === DCH_DY || id === DCH_Dy || id === DCH_dy;
        const v = r.seqSearch(abbrev ? WORDS_DAYS_ABBREV : WORDS_DAYS_FULL, i);
        fromCharSetInt(out, "d", v, n);
        out.d = (out.d + 1) | 0;
        break;
      }
      case DCH_DDD:
        r.parseInt("ddd", i);
        r.skipTH(n.suffix);
        break;
      case DCH_IDDD:
        r.parseIntLen("ddd", 3, i);
        r.skipTH(n.suffix);
        break;
      case DCH_DD:
        r.parseInt("dd", i);
        r.skipTH(n.suffix);
        break;
      case DCH_D:
        r.parseInt("d", i);
        r.skipTH(n.suffix);
        break;
      case DCH_ID:
        r.parseIntLen("d", 1, i);
        // Shift to the Gregorian numbering, Sunday = 1.
        out.d = (out.d + 1) | 0;
        if (out.d > 7) out.d = 1;
        r.skipTH(n.suffix);
        break;
      case DCH_WW:
      case DCH_IW:
        r.parseInt("ww", i);
        r.skipTH(n.suffix);
        break;
      case DCH_Q:
        // Q is read and checked but ignored (PostgreSQL).
        r.parseInt(null, i);
        r.skipTH(n.suffix);
        break;
      case DCH_CC:
        r.parseInt("cc", i);
        r.skipTH(n.suffix);
        break;
      case DCH_Y_YYY: {
        const rest = input.slice(r.pos);
        const [matched, millennia, years, nch] = scanYYYY(rest);
        if (matched < 2) {
          throw engineError(
            "invalid_datetime_format",
            `invalid value "${fromBytes(rest)}" for "Y,YYY"`,
          );
        }
        const m64 = millennia * 1000;
        const y64 = years + m64;
        if (m64 !== (m64 | 0) || y64 !== (y64 | 0)) {
          throw engineError(
            "datetime_field_overflow",
            'value for "Y,YYY" in source string is out of range',
          );
        }
        fromCharSetInt(out, "year", y64, n);
        out.yysz = 4;
        r.pos += nch;
        r.skipTH(n.suffix);
        break;
      }
      case DCH_YYYY:
      case DCH_IYYY:
        r.parseInt("year", i);
        out.yysz = 4;
        r.skipTH(n.suffix);
        break;
      case DCH_YYY:
      case DCH_IYY:
      case DCH_YY:
      case DCH_IY:
      case DCH_Y:
      case DCH_I: {
        const ln = r.parseInt("year", i);
        if (ln < 4) out.year = adjustPartialYear(out.year);
        out.yysz = key.name.length;
        r.skipTH(n.suffix);
        break;
      }
      case DCH_RM:
      case DCH_rm: {
        const v = r.seqSearch(WORDS_RM_MONTHS_LOWER, i);
        fromCharSetInt(out, "mm", 12 - v, n);
        break;
      }
      case DCH_W:
        r.parseInt("w", i);
        r.skipTH(n.suffix);
        break;
      case DCH_J:
        r.parseInt("j", i);
        r.skipTH(n.suffix);
        break;
    }
    // Ignore the spaces after a field.
    if (!fxMode) {
      extraSkip = 0;
      while (r.pos < input.length && isSpaceC(input.charCodeAt(r.pos))) {
        r.pos++;
        extraSkip++;
      }
    }
  }
}

// adjustPartialYear is formatting.c's adjust_partial_year_to_2020.
function adjustPartialYear(year: number): number {
  if (year < 70) return year + 2000;
  if (year < 100) return year + 1900;
  if (year < 520) return year + 2000;
  if (year < 1000) return year + 1000;
  return year;
}

// PgTm is the broken-down result of do_to_timestamp (PostgreSQL's pg_tm subset).
interface PgTm {
  year: number;
  mon: number;
  mday: number;
  hour: number;
  min: number;
  sec: number;
}

function fieldOverflow(input: string): EngineError {
  return engineError(
    "datetime_field_overflow",
    `date/time field value out of range: "${fromBytes(input)}"`,
  );
}

// mulS32 / addS32 are PostgreSQL's pg_mul_s32_overflow / pg_add_s32_overflow: the wrapped result
// and whether it overflowed. (Every caller's other operand is small, so the exact product fits a
// double.)
function mulS32(a: number, b: number): [number, boolean] {
  const r = a * b;
  return [r | 0, r !== (r | 0)];
}

function addS32(a: number, b: number): [number, boolean] {
  const r = a + b;
  return [r | 0, r !== (r | 0)];
}

interface TimestampParts {
  tm: PgTm;
  fsec: number;
  fprec: number;
  hasTz: boolean;
  tzEast: number;
}

// toTimestampParts is formatting.c's do_to_timestamp over byte strings: the date/time fields, the
// microseconds, the fractional precision FFn asked for (0 = none), and the explicit zone offset
// (east of UTC) if the template read one.
function toTimestampParts(input: string, tmpl: string): TimestampParts {
  const tmfc = newTmFromChar();
  const tm: PgTm = { year: 0, mon: 1, mday: 1, hour: 0, min: 0, sec: 0 };
  if (tmpl !== "") {
    const nodes = parseFormat(tmpl, true, null);
    dchFromChar(nodes, input, tmfc);
  }
  const M_YEAR = 1;
  const M_MONTH = 2;
  const M_DAY = 4;
  let fmask = 0;
  let fsec = 0;
  let hasTz = false;
  let tzEast = 0;
  if (tmfc.ssss !== 0) {
    let x = tmfc.ssss;
    tm.hour = div32(x, 3600);
    x %= 3600;
    tm.min = div32(x, 60);
    x %= 60;
    tm.sec = x;
  }
  if (tmfc.ss !== 0) tm.sec = tmfc.ss;
  if (tmfc.mi !== 0) tm.min = tmfc.mi;
  if (tmfc.hh !== 0) tm.hour = tmfc.hh;
  if (tmfc.clock === 12) {
    if (tm.hour < 1 || tm.hour > 12) {
      throw engineError(
        "invalid_datetime_format",
        `hour "${tm.hour}" is invalid for the 12-hour clock`,
      ).hinted("Use the 24-hour clock, or give an hour between 1 and 12.");
    }
    if (tmfc.pm !== 0 && tm.hour < 12) tm.hour += 12;
    else if (tmfc.pm === 0 && tm.hour === 12) tm.hour = 0;
  }
  if (tmfc.year !== 0) {
    // CC with YY (or Y) puts the two low-order digits in that century; a 4-digit year wins.
    if (tmfc.cc !== 0 && tmfc.yysz <= 2) {
      if (tmfc.bc !== 0) tmfc.cc = -tmfc.cc | 0;
      tm.year = tmfc.year % 100;
      if (tm.year !== 0) {
        if (tmfc.cc >= 0) {
          const [tmp, o1] = mulS32((tmfc.cc - 1) | 0, 100);
          const [y, o2] = addS32(tm.year, tmp);
          if (o1 || o2) throw fieldOverflow(input);
          tm.year = y;
        } else {
          const [tmp, o1] = mulS32((tmfc.cc + 1) | 0, 100);
          const [tmp2, o2] = addS32(tmp, -tm.year | 0);
          const [y, o3] = addS32(tmp2, 1);
          if (o1 || o2 || o3) throw fieldOverflow(input);
          tm.year = y;
        }
      } else {
        // The century year of a date ending in "00".
        const adj = tmfc.cc < 0 ? 1 : 0;
        tm.year = (Math.imul(tmfc.cc, 100) + adj) | 0;
      }
    } else {
      tm.year = tmfc.year;
      if (tmfc.bc !== 0) tm.year = -tm.year | 0;
      if (tm.year < 0) tm.year = (tm.year + 1) | 0;
    }
    fmask |= M_YEAR;
  } else if (tmfc.cc !== 0) {
    // The first year of the century.
    if (tmfc.bc !== 0) tmfc.cc = -tmfc.cc | 0;
    const [y, o1] = tmfc.cc >= 0 ? mulS32((tmfc.cc - 1) | 0, 100) : mulS32(tmfc.cc, 100);
    const [yr, o2] = addS32(y, 1);
    tm.year = yr;
    if (o1 || o2) throw fieldOverflow(input);
    fmask |= M_YEAR;
  }
  if (tmfc.j !== 0) {
    [tm.year, tm.mon, tm.mday] = pgJ2date(tmfc.j);
    fmask |= M_YEAR | M_MONTH | M_DAY;
  }
  if (tmfc.ww !== 0) {
    if (tmfc.mode === MODE_ISOWEEK) {
      // No day of week leaves the date at the start (Monday) of the ISO week.
      if (tmfc.d !== 0) {
        let jday = pgIsoweek2j(tm.year, tmfc.ww);
        if (tmfc.d > 1) jday = (jday + tmfc.d - 2) | 0;
        else jday = (jday + 6) | 0;
        [tm.year, tm.mon, tm.mday] = pgJ2date(jday);
      } else {
        [tm.year, tm.mon, tm.mday] = pgJ2date(pgIsoweek2j(tm.year, tmfc.ww));
      }
      fmask |= M_YEAR | M_MONTH | M_DAY;
    } else {
      const [a, o1] = addS32(tmfc.ww, -1);
      const [b, o2] = mulS32(a, 7);
      const [c, o3] = addS32(b, 1);
      if (o1 || o2 || o3) throw fieldOverflow(input);
      tmfc.ddd = c;
    }
  }
  if (tmfc.w !== 0) {
    const [a, o1] = addS32(tmfc.w, -1);
    const [b, o2] = mulS32(a, 7);
    const [c, o3] = addS32(b, 1);
    if (o1 || o2 || o3) throw fieldOverflow(input);
    tmfc.dd = c;
  }
  if (tmfc.dd !== 0) {
    tm.mday = tmfc.dd;
    fmask |= M_DAY;
  }
  if (tmfc.mm !== 0) {
    tm.mon = tmfc.mm;
    fmask |= M_MONTH;
  }
  if (tmfc.ddd !== 0 && (tm.mon <= 1 || tm.mday <= 1)) {
    // The month and day come from the day of the year, Gregorian or ISO.
    if (tm.year === 0 && tmfc.bc === 0) {
      throw engineError(
        "invalid_datetime_format",
        "cannot calculate day of year without year information",
      );
    }
    if (tmfc.mode === MODE_ISOWEEK) {
      const j0 = (pgIsoweek2j(tm.year, 1) - 1) | 0;
      [tm.year, tm.mon, tm.mday] = pgJ2date((j0 + tmfc.ddd) | 0);
      fmask |= M_YEAR | M_MONTH | M_DAY;
    } else {
      const ysum = [
        [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365],
        [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366],
      ];
      const y = ysum[pgIsLeap(tm.year) ? 1 : 0]!;
      let i = 1;
      for (; i <= 12; i++) {
        if (tmfc.ddd <= y[i]!) break;
      }
      if (tm.mon <= 1) tm.mon = i;
      if (tm.mday <= 1) tm.mday = (tmfc.ddd - y[i - 1]!) | 0;
      fmask |= M_MONTH | M_DAY;
    }
  }
  if (tmfc.ms !== 0) {
    const [tmp, o1] = mulS32(tmfc.ms, 1000);
    const [f, o2] = addS32(fsec, tmp);
    if (o1 || o2) throw fieldOverflow(input);
    fsec = f;
  }
  if (tmfc.us !== 0) fsec = (fsec + tmfc.us) | 0;
  const fprec = tmfc.ff;
  if (fmask !== 0) {
    // PostgreSQL's ValidateDate (the year was already adjusted for AD/BC).
    let bad =
      ((fmask & M_MONTH) !== 0 && (tm.mon < 1 || tm.mon > 12)) ||
      ((fmask & M_DAY) !== 0 && (tm.mday < 1 || tm.mday > 31));
    if (!bad && fmask === (M_YEAR | M_MONTH | M_DAY)) {
      bad = tm.mday > PG_DAY_TAB[pgIsLeap(tm.year) ? 1 : 0]![tm.mon - 1]!;
    }
    if (bad) throw fieldOverflow(input);
  }
  if (
    tm.hour < 0 ||
    tm.hour >= 24 ||
    tm.min < 0 ||
    tm.min >= 60 ||
    tm.sec < 0 ||
    tm.sec >= 60 ||
    fsec < 0 ||
    fsec >= 1_000_000
  ) {
    throw fieldOverflow(input);
  }
  if (tmfc.tzsign !== 0) {
    if (tmfc.tzh < 0 || tmfc.tzh > 15 || tmfc.tzm < 0 || tmfc.tzm >= 60) {
      throw engineError(
        "invalid_time_zone_displacement_value",
        `time zone displacement out of range: "${fromBytes(input)}"`,
      );
    }
    hasTz = true;
    tzEast = (tmfc.tzh * 60 + tmfc.tzm) * 60;
    if (tmfc.tzsign < 0) tzEast = -tzEast;
  }
  return { tm, fsec, fprec, hasTz, tzEast };
}

function inI64(v: bigint): boolean {
  return v >= I64_MIN && v <= I64_MAX;
}

// wallFromTm is the wall-clock microsecond reading of the broken-down fields, or null when it
// leaves the finite timestamp range.
function wallFromTm(tm: PgTm, fsec: number): bigint | null {
  const days = daysFromCivil(BigInt(tm.year), BigInt(tm.mon), BigInt(tm.mday));
  const p = days * 86_400n;
  if (!inI64(p)) return null;
  const secs = p + BigInt(tm.hour * 3600 + tm.min * 60 + tm.sec);
  if (!inI64(secs)) return null;
  const p2 = secs * 1_000_000n;
  if (!inI64(p2)) return null;
  const t = p2 + BigInt(fsec);
  if (!inI64(t) || t === POS_INFINITY || t === NEG_INFINITY) return null;
  return t;
}

// PG_EPOCH_SHIFT_MICROS is PostgreSQL's 2000-01-01 epoch, in jed's microseconds since 1970-01-01.
const PG_EPOCH_SHIFT_MICROS = 946_684_800n * 1_000_000n;

// roundToPrecision is PostgreSQL's AdjustTimestampForTypmod for 0 < prec < 6: round half away from
// zero to prec fractional digits, the sign taken relative to PostgreSQL's 2000-01-01 epoch.
function roundToPrecision(t: bigint, prec: number): bigint | null {
  if (prec <= 0 || prec >= 6) return t;
  let scale = 1n;
  for (let k = prec; k < 6; k++) scale *= 10n;
  const offset = scale / 2n;
  let pg = t - PG_EPOCH_SHIFT_MICROS;
  if (!inI64(pg)) return null;
  if (pg >= 0n) pg = ((pg + offset) / scale) * scale;
  else pg = -(((-pg + offset) / scale) * scale);
  const r = pg + PG_EPOCH_SHIFT_MICROS;
  if (!inI64(r) || r === POS_INFINITY || r === NEG_INFINITY) return null;
  return r;
}

function timestampOutOfRange(): EngineError {
  return engineError("datetime_field_overflow", "timestamp out of range");
}

// toTimestampText is to_timestamp(text, text) → timestamptz: an explicit TZH / TZM / OF offset fixes
// the instant; otherwise the wall clock is read in zone (one timezone unit).
export function toTimestampText(input: string, tmpl: string, zone: ZoneRef, m: Meter): bigint {
  const p = toTimestampParts(toBytes(input), toBytes(tmpl));
  const wall = wallFromTm(p.tm, p.fsec);
  if (wall === null) throw timestampOutOfRange();
  let inst: bigint;
  if (p.hasTz) {
    inst = wall - BigInt(p.tzEast) * 1_000_000n;
    if (!inI64(inst) || inst === NEG_INFINITY || inst === POS_INFINITY) {
      throw timestampOutOfRange();
    }
  } else {
    m.charge(COSTS.timezone);
    m.guard();
    inst = localToInstantMicros(zone, wall);
  }
  if (p.fprec !== 0) {
    const r = roundToPrecision(inst, p.fprec);
    if (r === null) throw timestampOutOfRange();
    inst = r;
  }
  return inst;
}

// toDateText is to_date(text, text) → date (the day count).
export function toDateText(input: string, tmpl: string): bigint {
  const { tm } = toTimestampParts(toBytes(input), toBytes(tmpl));
  const days = daysFromCivil(BigInt(tm.year), BigInt(tm.mon), BigInt(tm.mday));
  if (days <= DATE_NEG_INFINITY || days >= DATE_POS_INFINITY) {
    throw engineError("datetime_field_overflow", `date out of range: "${input}"`);
  }
  return days;
}

// roundToEven is Go's math.RoundToEven (C rint under the default rounding mode).
export function roundToEven(x: number): number {
  if (!Number.isFinite(x)) return x;
  const f = Math.floor(x);
  const diff = x - f;
  if (diff < 0.5) return f;
  if (diff > 0.5) return f + 1;
  return f % 2 === 0 ? f : f + 1;
}

// toTimestampFloat is to_timestamp(double precision): Unix-epoch seconds, PostgreSQL's
// float8_timestamptz — computed in PostgreSQL's 2000 epoch with its float operations, so the
// rounding matches to the microsecond.
export function toTimestampFloat(secs: number): bigint {
  if (Number.isNaN(secs)) throw engineError("datetime_field_overflow", "timestamp cannot be NaN");
  if (secs === Infinity) return POS_INFINITY;
  if (secs === -Infinity) return NEG_INFINITY;
  const tooFar = (): EngineError =>
    engineError(
      "datetime_field_overflow",
      `timestamp out of range: "${renderFloatPg(secs, false)}"`,
    );
  // jed's range is the finite int64 microseconds, checked on the rounded count.
  const us = roundToEven((secs - 946_684_800) * 1_000_000);
  if (us < -TWO_63 || us >= TWO_63) throw tooFar();
  const r = BigInt(us) + PG_EPOCH_SHIFT_MICROS;
  if (!inI64(r) || r === POS_INFINITY || r === NEG_INFINITY) throw tooFar();
  return r;
}
