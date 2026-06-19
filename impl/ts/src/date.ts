// The date calendar type — parsing and rendering (spec/design/date.md). A date is an i32
// count of days since the Unix epoch (1970-01-01), proleptic Gregorian. It is the day-granular
// sibling of timestamp and REUSES timestamp's calendar core verbatim (daysFromCivil/civilFromDays,
// same epoch — spec/design/timestamp.md §2), so the two types cannot drift.
//
// Unlike timestamp, a date keeps ONLY the date portion: a time/offset in the input is parsed and
// validated, then DISCARDED — and 24:00:00 does NOT roll into the day (PG behavior). No instant is
// ever computed, so a date spans a wider range than the i64-µs timestamp. The day count is held
// as `bigint` (the TS core's uniform-integer discipline), converted to i32 at the codec boundary.

import { EngineError } from "./errors.ts";
import {
  civilFromDays,
  type Cur,
  daysFromCivil,
  daysInMonth,
  expectChar,
  fieldOverflow,
  invalidDatetime,
  readFrac,
  readUint,
  trimASCIIWS,
} from "./timestamp.ts";

// DATE_NEG_INFINITY is the -infinity sentinel (the smallest i32; sorts before every finite date).
export const DATE_NEG_INFINITY = -2147483648n;
// DATE_POS_INFINITY is the +infinity sentinel (the largest i32; sorts after every finite date).
export const DATE_POS_INFINITY = 2147483647n;

// Finite day counts occupy [MinInt32+1, MaxInt32-1]; the extremes are reserved for ±infinity.
const DATE_MIN_FINITE = -2147483647n;
const DATE_MAX_FINITE = 2147483646n;

// parseDate parses a date literal to its i32 day count (a bigint in [DATE_MIN_FINITE,
// DATE_MAX_FINITE], or a ±infinity sentinel) since 1970-01-01. The grammar is the full timestamp
// literal grammar (spec/design/timestamp.md §3), but only the date portion is kept: a trailing
// time and/or offset is validated then discarded, and 24:00:00 does not advance the day. Malformed
// syntax traps 22007; an out-of-range field or a day count beyond the finite i32 range traps 22008.
export function parseDate(input: string): bigint {
  const s = trimASCIIWS(input);
  const low = s.toLowerCase();

  if (low === "infinity" || low === "+infinity") return DATE_POS_INFINITY;
  if (low === "-infinity") return DATE_NEG_INFINITY;

  let bc = false;
  let body = s;
  if (low.endsWith(" bc")) {
    bc = true;
    body = trimASCIIWS(s.slice(0, s.length - 3));
  } else if (low.endsWith(" ad")) {
    body = trimASCIIWS(s.slice(0, s.length - 3));
  }

  const cur: Cur = { i: 0 };
  const year = readUint(body, cur);
  expectChar(body, cur, "-");
  const month = readUint(body, cur);
  expectChar(body, cur, "-");
  const day = readUint(body, cur);

  const bad = (): EngineError => invalidDatetime("invalid input syntax for type date");

  // optional time — validated for syntax/range, then DISCARDED (the day is taken from the date
  // fields directly; 24:00:00 does not advance it).
  let hour = 0n;
  let minute = 0n;
  let second = 0n;
  let micro = 0n;
  if (cur.i < body.length && (body[cur.i] === " " || body[cur.i] === "T" || body[cur.i] === "t")) {
    cur.i++;
    hour = readUint(body, cur);
    expectChar(body, cur, ":");
    minute = readUint(body, cur);
    if (cur.i < body.length && body[cur.i] === ":") {
      cur.i++;
      second = readUint(body, cur);
      if (cur.i < body.length && body[cur.i] === ".") {
        cur.i++;
        micro = readFrac(body, cur);
      }
    }
  }

  // optional offset — validated, then DISCARDED (never applied, so it cannot shift the day).
  if (cur.i < body.length) {
    const ch = body[cur.i];
    if (ch === "Z" || ch === "z") {
      cur.i++;
    } else if (ch === "+" || ch === "-") {
      cur.i++;
      const oh = readUint(body, cur);
      let om = 0n;
      let os = 0n;
      if (cur.i < body.length && body[cur.i] === ":") {
        cur.i++;
        om = readUint(body, cur);
        if (cur.i < body.length && body[cur.i] === ":") {
          cur.i++;
          os = readUint(body, cur);
        }
      }
      if (oh > 15n || om > 59n || os > 59n) throw fieldOverflow("time zone offset out of range");
    } else {
      throw bad();
    }
  }
  if (cur.i !== body.length) throw bad();

  // Field validation. The year magnitude cap (a date spans ≈ ±5.88M years, far wider than
  // timestamp's ±294k) is only an overflow guard; the real bound is the i32 day-range check.
  if (year < 1n || year > 9_999_999n) throw fieldOverflow("year out of range");
  if (month < 1n || month > 12n) throw fieldOverflow("month out of range");
  const astro = bc ? 1n - year : year;
  if (day < 1n || day > daysInMonth(astro, month)) throw fieldOverflow("day out of range for month");
  // hour 0..23, plus exactly 24:00:00 (a valid end-of-day; unlike timestamp it does NOT advance
  // the date — the day comes from the date fields directly).
  const allow24 = hour === 24n && minute === 0n && second === 0n && micro === 0n;
  if (hour > 23n && !allow24) throw fieldOverflow("hour out of range");
  if (minute > 59n) throw fieldOverflow("minute out of range");
  if (second > 59n) throw fieldOverflow("second out of range"); // no leap seconds (:60)

  const days = daysFromCivil(astro, month, day);
  if (days < DATE_MIN_FINITE || days > DATE_MAX_FINITE) throw fieldOverflow("date out of range");
  return days;
}

// renderDate renders a date value (i32 days since 1970-01-01, as a bigint) to its canonical
// YYYY-MM-DD text (a BC suffix for an astronomical year <= 0; ±infinity render as the bare words).
export function renderDate(days: bigint): string {
  if (days === DATE_NEG_INFINITY) return "-infinity";
  if (days === DATE_POS_INFINITY) return "infinity";
  const [y, mo, d] = civilFromDays(days);
  const displayed = y <= 0n ? 1n - y : y;
  const era = y <= 0n ? " BC" : "";
  const pad = (n: bigint, w: number): string => n.toString().padStart(w, "0");
  return `${pad(displayed, 4)}-${pad(mo, 2)}-${pad(d, 2)}${era}`;
}
