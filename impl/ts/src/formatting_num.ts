// The number half of the data-type formatting functions — to_char over integer, decimal, and
// float values, and to_number (spec/design/formatting.md §6–§7): PostgreSQL 18's NUMDesc_prepare,
// NUM_processor, and the per-type to_char preparers, transcribed byte for byte. The C printf
// conversions PostgreSQL's float paths use ("%.*f", "%+.*e") are reproduced exactly from the
// float's exact decimal expansion (round half to even, glibc's rule), with PostgreSQL's own
// snprintf spellings of NaN and Infinity. Like formatting.ts, the processors run over UTF-8 byte
// strings / byte arrays, never UTF-16 code units.

import type { Meter } from "./cost.ts";
import { Decimal } from "./decimal.ts";
import { engineError, type EngineError } from "./errors.ts";
import {
  NUM_0,
  NUM_9,
  NUM_B,
  NUM_COMMA,
  NUM_D,
  NUM_DEC,
  NUM_E,
  NUM_FM,
  NUM_G,
  NUM_L,
  NUM_MI,
  NUM_PL,
  NUM_PR,
  NUM_RN,
  NUM_S,
  NUM_SG,
  NUM_TH,
  NUM_V,
  NUM_rn,
  NUM_th,
  WORDS_RN_HUNDREDS,
  WORDS_RN_TENS,
  WORDS_RN_UNITS,
} from "./formatting_gen.ts";
import {
  asciiLowerStr,
  cPadInt,
  cPadLeft,
  FN_ACTION,
  FN_END,
  type FmtNode,
  fmtOrdinal,
  fromBytes,
  isDigitC,
  isSpaceC,
  parseFormat,
  roundToEven,
  toBytes,
} from "./formatting.ts";

const INT32_MIN = -2147483648;
const INT32_MAX = 2147483647;
const I64_MIN = -9223372036854775808n;
const I64_MAX = 9223372036854775807n;
// TWO_63 is 2^63 as a double: the i64 range bound for a float.
const TWO_63 = 2 ** 63;

// The byte codes the processors compare against.
const C_PLUS = 0x2b;
const C_MINUS = 0x2d;
const C_SPACE = 0x20;
const C_POINT = 0x2e;
const C_COMMA = 0x2c;
const C_ZERO = 0x30;
const C_HASH = 0x23;
const C_LT = 0x3c;
const C_GT = 0x3e;

// The number template flags (formatting.c NUM_F_*).
const NUM_F_DECIMAL = 1 << 1;
const NUM_F_LDECIMAL = 1 << 2;
const NUM_F_ZERO = 1 << 3;
const NUM_F_BLANK = 1 << 4;
const NUM_F_FILLMODE = 1 << 5;
const NUM_F_LSIGN = 1 << 6;
const NUM_F_BRACKET = 1 << 7;
const NUM_F_MINUS = 1 << 8;
const NUM_F_PLUS = 1 << 9;
const NUM_F_ROMAN = 1 << 10;
const NUM_F_MULTI = 1 << 11;
const NUM_F_PLUS_POST = 1 << 12;
const NUM_F_MINUS_POST = 1 << 13;
const NUM_F_EEEE = 1 << 14;

const LSIGN_PRE = -1;
const LSIGN_POST = 1;
const LSIGN_NONE = 0;

// The C-locale symbols (PostgreSQL's NUM_prepare_locale under lc_numeric = C).
const NUM_LOCALE_DECIMAL = ".";
const NUM_LOCALE_THOUSAND = ",";
const NUM_LOCALE_CURRENCY = " ";
const NUM_LOCALE_NEGATIVE = "-";
const NUM_LOCALE_POSITIVE = "+";

function numSyntax(msg: string): EngineError {
  return engineError("syntax_error", msg);
}

// NumDesc is formatting.c's NUMDesc: what a number template asks for.
class NumDesc {
  pre = 0;
  post = 0;
  lsign = LSIGN_NONE;
  flag = 0;
  preLsignNum = 0;
  multi = 0;
  zeroStart = 0;
  zeroEnd = 0;

  has(f: number): boolean {
    return (this.flag & f) !== 0;
  }

  // prepare is formatting.c's NUMDesc_prepare for one pattern.
  prepare(id: number): void {
    if (this.has(NUM_F_EEEE) && id !== NUM_E) {
      throw numSyntax('"EEEE" must be the last pattern used');
    }
    switch (id) {
      case NUM_9:
        if (this.has(NUM_F_BRACKET)) throw numSyntax('"9" must be ahead of "PR"');
        if (this.has(NUM_F_MULTI)) {
          this.multi++;
          break;
        }
        if (this.has(NUM_F_DECIMAL)) this.post++;
        else this.pre++;
        break;
      case NUM_0:
        if (this.has(NUM_F_BRACKET)) throw numSyntax('"0" must be ahead of "PR"');
        if (!this.has(NUM_F_ZERO) && !this.has(NUM_F_DECIMAL)) {
          this.flag |= NUM_F_ZERO;
          this.zeroStart = this.pre + 1;
        }
        if (!this.has(NUM_F_DECIMAL)) this.pre++;
        else this.post++;
        this.zeroEnd = this.pre + this.post;
        break;
      case NUM_B:
        if (this.pre === 0 && this.post === 0 && !this.has(NUM_F_ZERO)) this.flag |= NUM_F_BLANK;
        break;
      case NUM_D:
      case NUM_DEC:
        if (id === NUM_D) this.flag |= NUM_F_LDECIMAL;
        if (this.has(NUM_F_DECIMAL)) throw numSyntax("multiple decimal points");
        if (this.has(NUM_F_MULTI)) throw numSyntax('cannot use "V" and decimal point together');
        this.flag |= NUM_F_DECIMAL;
        break;
      case NUM_FM:
        this.flag |= NUM_F_FILLMODE;
        break;
      case NUM_S:
        if (this.has(NUM_F_LSIGN)) throw numSyntax('cannot use "S" twice');
        if (this.has(NUM_F_PLUS) || this.has(NUM_F_MINUS) || this.has(NUM_F_BRACKET)) {
          throw numSyntax('cannot use "S" and "PL"/"MI"/"SG"/"PR" together');
        }
        if (!this.has(NUM_F_DECIMAL)) {
          this.lsign = LSIGN_PRE;
          this.preLsignNum = this.pre;
          this.flag |= NUM_F_LSIGN;
        } else if (this.lsign === LSIGN_NONE) {
          this.lsign = LSIGN_POST;
          this.flag |= NUM_F_LSIGN;
        }
        break;
      case NUM_MI:
        if (this.has(NUM_F_LSIGN)) throw numSyntax('cannot use "S" and "MI" together');
        this.flag |= NUM_F_MINUS;
        if (this.has(NUM_F_DECIMAL)) this.flag |= NUM_F_MINUS_POST;
        break;
      case NUM_PL:
        if (this.has(NUM_F_LSIGN)) throw numSyntax('cannot use "S" and "PL" together');
        this.flag |= NUM_F_PLUS;
        if (this.has(NUM_F_DECIMAL)) this.flag |= NUM_F_PLUS_POST;
        break;
      case NUM_SG:
        if (this.has(NUM_F_LSIGN)) throw numSyntax('cannot use "S" and "SG" together');
        this.flag |= NUM_F_MINUS | NUM_F_PLUS;
        break;
      case NUM_PR:
        if (this.has(NUM_F_LSIGN) || this.has(NUM_F_PLUS) || this.has(NUM_F_MINUS)) {
          throw numSyntax('cannot use "PR" and "S"/"PL"/"MI"/"SG" together');
        }
        this.flag |= NUM_F_BRACKET;
        break;
      case NUM_RN:
      case NUM_rn:
        if (this.has(NUM_F_ROMAN)) throw numSyntax('cannot use "RN" twice');
        this.flag |= NUM_F_ROMAN;
        break;
      case NUM_V:
        if (this.has(NUM_F_DECIMAL)) throw numSyntax('cannot use "V" and decimal point together');
        this.flag |= NUM_F_MULTI;
        break;
      case NUM_E:
        if (this.has(NUM_F_EEEE)) throw numSyntax('cannot use "EEEE" twice');
        if (
          this.has(NUM_F_BLANK) ||
          this.has(NUM_F_FILLMODE) ||
          this.has(NUM_F_LSIGN) ||
          this.has(NUM_F_BRACKET) ||
          this.has(NUM_F_MINUS) ||
          this.has(NUM_F_PLUS) ||
          this.has(NUM_F_ROMAN) ||
          this.has(NUM_F_MULTI)
        ) {
          throw numSyntax('"EEEE" is incompatible with other formats');
        }
        this.flag |= NUM_F_EEEE;
        break;
    }
    if (this.has(NUM_F_ROMAN) && (this.flag & ~(NUM_F_ROMAN | NUM_F_FILLMODE)) !== 0) {
      throw numSyntax('"RN" is incompatible with other formats');
    }
  }
}

// pushStr appends a byte string's bytes.
function pushStr(out: number[], s: string): void {
  for (let i = 0; i < s.length; i++) out.push(s.charCodeAt(i));
}

// bytesToStr is a byte array as a byte string.
function bytesToStr(b: readonly number[]): string {
  let s = "";
  for (let i = 0; i < b.length; i += 8192) {
    s += String.fromCharCode(...b.slice(i, i + 8192));
  }
  return s;
}

function strToBytes(s: string): number[] {
  const b: number[] = new Array(s.length);
  for (let i = 0; i < s.length; i++) b[i] = s.charCodeAt(i);
  return b;
}

// cString is b up to its first NUL byte, as a byte string.
function cString(b: readonly number[]): string {
  const k = b.indexOf(0);
  return bytesToStr(k >= 0 ? b.slice(0, k) : b);
}

function signSymbol(sign: number): string {
  return sign === C_MINUS ? NUM_LOCALE_NEGATIVE : NUM_LOCALE_POSITIVE;
}

// NumProc is formatting.c's NUMProc. For to_char, out is the output and number the digit string
// being laid out; for to_number, inout is the input and number the digits read (number[0] is the
// sign slot).
class NumProc {
  sign = 0;
  signWrote = false;
  numCount = 0;
  numIn = false;
  numCurr = 0;
  outPreSpaces = 0;
  readDec = false;
  readPost = 0;
  readPre = 0;
  numberP = 0;
  inoutP = 0;
  out: number[] = [];
  lastRelevant = -1; // -1: none

  readonly num: NumDesc;
  number: number[];
  readonly inout: number[];
  constructor(num: NumDesc, number: number[], inout: number[]) {
    this.num = num;
    this.number = number;
    this.inout = inout;
  }

  // at is the number byte at i, 0 past the end (the C string's terminator).
  at(i: number): number {
    return i < this.number.length ? this.number[i]! : 0;
  }

  // in is the input byte at i, 0 past the end.
  inb(i: number): number {
    return i < this.inout.length ? this.inout[i]! : 0;
  }

  overload(): boolean {
    return this.inoutP >= this.inout.length;
  }

  amount(s: number): boolean {
    return this.inoutP <= this.inout.length - s;
  }

  inputHasPrefix(p: string): boolean {
    if (!this.amount(p.length)) return false;
    for (let k = 0; k < p.length; k++) {
      if (this.inout[this.inoutP + k] !== p.charCodeAt(k)) return false;
    }
    return true;
  }

  // isPredecSpace is formatting.c's IS_PREDEC_SPACE: a lone leading zero before the decimal point.
  isPredecSpace(): boolean {
    return (
      !this.num.has(NUM_F_ZERO) &&
      this.numberP === 0 &&
      this.at(0) === C_ZERO &&
      this.num.post !== 0
    );
  }

  lastRelevantIsPoint(): boolean {
    return this.lastRelevant >= 0 && this.at(this.lastRelevant) === C_POINT;
  }

  // numpartToChar is formatting.c's NUM_numpart_to_char: write a sign, digit, or decimal point.
  numpartToChar(id: number): void {
    const num = this.num;
    if (num.has(NUM_F_ROMAN)) return;
    this.numIn = false;
    const fill = num.has(NUM_F_FILLMODE);
    if (
      !this.signWrote &&
      (this.numCurr >= this.outPreSpaces ||
        (num.has(NUM_F_ZERO) && num.zeroStart === this.numCurr)) &&
      (!this.isPredecSpace() || this.lastRelevantIsPoint())
    ) {
      if (num.has(NUM_F_LSIGN)) {
        if (num.lsign === LSIGN_PRE) {
          pushStr(this.out, signSymbol(this.sign));
          this.signWrote = true;
        }
      } else if (num.has(NUM_F_BRACKET)) {
        this.out.push(this.sign === C_PLUS ? C_SPACE : C_LT);
        this.signWrote = true;
      } else if (this.sign === C_PLUS) {
        if (!fill) this.out.push(C_SPACE);
        this.signWrote = true;
      } else if (this.sign === C_MINUS) {
        this.out.push(C_MINUS);
        this.signWrote = true;
      }
    }
    if (id === NUM_9 || id === NUM_0 || id === NUM_D || id === NUM_DEC) {
      if (
        this.numCurr < this.outPreSpaces &&
        (num.zeroStart > this.numCurr || !num.has(NUM_F_ZERO))
      ) {
        if (!fill) this.out.push(C_SPACE);
      } else if (
        num.has(NUM_F_ZERO) &&
        this.numCurr < this.outPreSpaces &&
        num.zeroStart <= this.numCurr
      ) {
        this.out.push(C_ZERO);
        this.numIn = true;
      } else {
        if (this.at(this.numberP) === C_POINT) {
          if (!this.lastRelevantIsPoint() || fill) pushStr(this.out, NUM_LOCALE_DECIMAL);
        } else if (this.lastRelevant >= 0 && this.numberP > this.lastRelevant && id !== NUM_0) {
          // past the last relevant digit: nothing
        } else if (this.isPredecSpace()) {
          if (!fill) this.out.push(C_SPACE);
          else if (this.lastRelevantIsPoint()) this.out.push(C_ZERO);
        } else {
          this.out.push(this.at(this.numberP));
          this.numIn = true;
        }
        if (this.at(this.numberP) !== 0) this.numberP++;
      }
      let end = this.numCount;
      if (this.outPreSpaces !== 0) end++;
      if (num.has(NUM_F_DECIMAL)) end++;
      if (this.lastRelevant >= 0 && this.lastRelevant === this.numberP) end = this.numCurr;
      if (this.numCurr + 1 === end) {
        if (this.signWrote && num.has(NUM_F_BRACKET)) {
          this.out.push(this.sign === C_PLUS ? C_SPACE : C_GT);
        } else if (num.has(NUM_F_LSIGN) && num.lsign === LSIGN_POST) {
          pushStr(this.out, signSymbol(this.sign));
        }
      }
    }
    this.numCurr++;
  }

  // numpartFromChar is formatting.c's NUM_numpart_from_char: read a sign, digit, or decimal point.
  numpartFromChar(id: number): void {
    const num = this.num;
    if (this.overload()) return;
    if (this.inb(this.inoutP) === C_SPACE) this.inoutP++;
    if (this.overload()) return;
    // A sign before the number.
    if (
      this.number[0] === C_SPACE &&
      (id === NUM_0 || id === NUM_9) &&
      this.readPre + this.readPost === 0
    ) {
      if (num.has(NUM_F_LSIGN) && num.lsign === LSIGN_PRE) {
        if (this.inputHasPrefix(NUM_LOCALE_NEGATIVE)) {
          this.inoutP += NUM_LOCALE_NEGATIVE.length;
          this.number[0] = C_MINUS;
        } else if (this.inputHasPrefix(NUM_LOCALE_POSITIVE)) {
          this.inoutP += NUM_LOCALE_POSITIVE.length;
          this.number[0] = C_PLUS;
        }
      } else {
        const c = this.inb(this.inoutP);
        if (c === C_MINUS || (num.has(NUM_F_BRACKET) && c === C_LT)) {
          this.number[0] = C_MINUS;
          this.inoutP++;
        } else if (c === C_PLUS) {
          this.number[0] = C_PLUS;
          this.inoutP++;
        }
      }
    }
    if (this.overload()) return;
    let isread = false;
    if (isDigitC(this.inb(this.inoutP))) {
      if (this.readDec && this.readPost === num.post) return;
      this.number.push(this.inb(this.inoutP));
      this.numberP++;
      if (this.readDec) this.readPost++;
      else this.readPre++;
      isread = true;
    } else if (num.has(NUM_F_DECIMAL) && !this.readDec) {
      if (this.inputHasPrefix(NUM_LOCALE_DECIMAL)) {
        this.inoutP += NUM_LOCALE_DECIMAL.length - 1;
        this.number.push(C_POINT);
        this.numberP++;
        this.readDec = true;
        isread = true;
      }
    }
    if (this.overload()) return;
    // A sign after the last digit.
    if (this.number[0] === C_SPACE && this.readPre + this.readPost > 0) {
      if (
        num.has(NUM_F_LSIGN) &&
        isread &&
        this.inoutP + 1 < this.inout.length &&
        !isDigitC(this.inb(this.inoutP + 1))
      ) {
        const tmp = this.inoutP;
        this.inoutP++;
        if (this.inputHasPrefix(NUM_LOCALE_NEGATIVE)) {
          this.inoutP += NUM_LOCALE_NEGATIVE.length - 1;
          this.number[0] = C_MINUS;
        } else if (this.inputHasPrefix(NUM_LOCALE_POSITIVE)) {
          this.inoutP += NUM_LOCALE_POSITIVE.length - 1;
          this.number[0] = C_PLUS;
        }
        if (this.number[0] === C_SPACE) this.inoutP = tmp;
      } else if (
        !isread &&
        !num.has(NUM_F_LSIGN) &&
        (num.has(NUM_F_PLUS) || num.has(NUM_F_MINUS))
      ) {
        const c = this.inb(this.inoutP);
        if (c === C_MINUS || c === C_PLUS) this.number[0] = c;
      }
    }
  }

  // eatNonDataChars is formatting.c's NUM_eat_non_data_chars: skip up to n input characters that
  // are not numeric data.
  eatNonDataChars(n: number): void {
    for (; n > 0; n--) {
      if (this.overload()) break;
      if ("0123456789.,+-".indexOf(String.fromCharCode(this.inb(this.inoutP))) >= 0) break;
      this.inoutP += pgMblenArr(this.inout, this.inoutP);
    }
  }

  // romanToInt is formatting.c's roman_to_int: a standard-form Roman numeral (1..3999), or -1.
  romanToInt(): number {
    while (!this.overload() && isSpaceC(this.inb(this.inoutP))) this.inoutP++;
    const chars: number[] = [];
    const vals: number[] = [];
    while (chars.length < 15 && !this.overload()) {
      let c = this.inb(this.inoutP);
      if (c >= 0x61 && c <= 0x7a) c -= 32;
      const v = romanVal(c);
      if (v === 0) break;
      chars.push(c);
      vals.push(v);
      this.inoutP++;
    }
    if (chars.length === 0) return -1;
    let result = 0;
    let repeat = 1;
    let vCount = 0;
    let lCount = 0;
    let dCount = 0;
    let subtraction = false;
    let lastSub = 0;
    for (let i = 0; i < chars.length; i++) {
      const cur = chars[i]!;
      const cv = vals[i]!;
      if (subtraction && cv >= lastSub) return -1;
      if ((vCount !== 0 && cv >= 5) || (lCount !== 0 && cv >= 50) || (dCount !== 0 && cv >= 500)) {
        return -1;
      }
      if (cur === 0x56) vCount++;
      else if (cur === 0x4c) lCount++;
      else if (cur === 0x44) dCount++;
      if (i < chars.length - 1) {
        const next = chars[i + 1]!;
        const nv = vals[i + 1]!;
        if (cv < nv) {
          if (!validSubComb(cur, next) || repeat > 1) return -1;
          if (
            (vCount !== 0 && nv >= 5) ||
            (lCount !== 0 && nv >= 50) ||
            (dCount !== 0 && nv >= 500)
          ) {
            return -1;
          }
          if (next === 0x56) vCount++;
          else if (next === 0x4c) lCount++;
          else if (next === 0x44) dCount++;
          i++;
          repeat = 1;
          subtraction = true;
          lastSub = cv;
          result += nv - cv;
        } else {
          if (cur === next) {
            repeat++;
            if (repeat > 3) return -1;
          } else {
            repeat = 1;
          }
          result += cv;
        }
      } else {
        result += cv;
      }
    }
    return result;
  }
}

function pgMblenArr(b: readonly number[], i: number): number {
  const c = b[i]!;
  let n = 1;
  if ((c & 0x80) === 0) n = 1;
  else if ((c & 0xe0) === 0xc0) n = 2;
  else if ((c & 0xf0) === 0xe0) n = 3;
  else if ((c & 0xf8) === 0xf0) n = 4;
  if (i + n > b.length) n = b.length - i;
  return n;
}

// romanVal is formatting.c's ROMAN_VAL.
function romanVal(c: number): number {
  switch (c) {
    case 0x49: // I
      return 1;
    case 0x56: // V
      return 5;
    case 0x58: // X
      return 10;
    case 0x4c: // L
      return 50;
    case 0x43: // C
      return 100;
    case 0x44: // D
      return 500;
    case 0x4d: // M
      return 1000;
  }
  return 0;
}

function validSubComb(cur: number, next: number): boolean {
  return (
    (cur === 0x49 && (next === 0x56 || next === 0x58)) ||
    (cur === 0x58 && (next === 0x4c || next === 0x43)) ||
    (cur === 0x43 && (next === 0x44 || next === 0x4d))
  );
}

// intToRoman is formatting.c's int_to_roman: upper-case, unpadded; 15 '#' out of 1..3999.
function intToRoman(number: number): string {
  if (number > 3999 || number < 1) return "#".repeat(15);
  let b = "";
  const s = String(number);
  for (let i = 0; i < s.length; i++) {
    const d = s.charCodeAt(i) - 0x30 - 1;
    if (d < 0) continue;
    switch (s.length - i) {
      case 4:
        b += "M".repeat(d + 1);
        break;
      case 3:
        b += WORDS_RN_HUNDREDS[d]!;
        break;
      case 2:
        b += WORDS_RN_TENS[d]!;
        break;
      case 1:
        b += WORDS_RN_UNITS[d]!;
        break;
    }
  }
  return b;
}

// numProcessor is formatting.c's NUM_processor. For to_char it returns the formatted bytes; for
// to_number the digits read, with num.post set to the fractional digits read.
function numProcessor(
  nodes: FmtNode[],
  num: NumDesc,
  inout: number[],
  number: number[],
  outPreSpaces: number,
  sign: number,
  isToChar: boolean,
): number[] {
  const np = new NumProc(num, number, inout);
  if (num.zeroStart !== 0) num.zeroStart--;
  if (num.has(NUM_F_EEEE)) {
    if (!isToChar) throw engineError("feature_not_supported", '"EEEE" not supported for input');
    return number.slice();
  }
  const fill = num.has(NUM_F_FILLMODE);
  if (isToChar) {
    np.sign = sign;
    if (num.has(NUM_F_PLUS) || num.has(NUM_F_MINUS)) {
      np.signWrote = !(num.has(NUM_F_PLUS) && !num.has(NUM_F_MINUS));
    } else {
      if (np.sign !== C_MINUS && fill) num.flag &= ~NUM_F_BRACKET;
      np.signWrote = np.sign === C_PLUS && fill && !num.has(NUM_F_LSIGN);
      if (num.lsign === LSIGN_PRE && num.pre === num.preLsignNum) num.lsign = LSIGN_POST;
    }
  }
  np.numCount = num.post + num.pre - 1;
  if (isToChar) {
    np.outPreSpaces = outPreSpaces;
    if (fill && num.has(NUM_F_DECIMAL)) {
      np.lastRelevant = lastRelevantDecnum(np.number);
      // Keep the digits a '0' pattern asks for, but not past the end of the number.
      if (np.lastRelevant >= 0 && num.zeroEnd > np.outPreSpaces) {
        let lastZero = np.number.length - 1;
        const z = num.zeroEnd - np.outPreSpaces;
        if (z < lastZero) lastZero = z;
        if (np.lastRelevant < lastZero) np.lastRelevant = lastZero;
      }
    }
    if (!np.signWrote && np.outPreSpaces === 0) np.numCount++;
  } else {
    np.number = [C_SPACE];
    np.numberP = 1;
  }
  for (const n of nodes) {
    if (n.typ === FN_END) break;
    if (!isToChar && np.overload()) break;
    if (n.typ !== FN_ACTION) {
      if (isToChar) pushStr(np.out, n.chars);
      else np.inoutP += pgMblenArr(np.inout, np.inoutP);
      continue;
    }
    const id = n.key!.id;
    switch (id) {
      case NUM_9:
      case NUM_0:
      case NUM_DEC:
      case NUM_D:
        if (isToChar) {
          np.numpartToChar(id);
          continue;
        }
        np.numpartFromChar(id);
        break;
      case NUM_COMMA:
        if (isToChar) {
          if (np.numIn) np.out.push(C_COMMA);
          else if (!fill) np.out.push(C_SPACE);
          continue;
        }
        if (!np.numIn && fill) continue;
        if (np.inb(np.inoutP) !== C_COMMA) continue;
        break;
      case NUM_G:
        if (isToChar) {
          if (np.numIn) pushStr(np.out, NUM_LOCALE_THOUSAND);
          else if (!fill) pushStr(np.out, " ".repeat(NUM_LOCALE_THOUSAND.length));
          continue;
        }
        if (!np.numIn && fill) continue;
        if (!np.inputHasPrefix(NUM_LOCALE_THOUSAND)) continue;
        np.inoutP += NUM_LOCALE_THOUSAND.length - 1;
        break;
      case NUM_L:
        if (isToChar) {
          pushStr(np.out, NUM_LOCALE_CURRENCY);
          continue;
        }
        np.eatNonDataChars(NUM_LOCALE_CURRENCY.length);
        continue;
      case NUM_RN:
      case NUM_rn: {
        if (isToChar) {
          let s = cString(np.number.slice(np.numberP));
          if (id === NUM_rn) s = asciiLowerStr(s);
          if (!fill) s = cPadLeft(s, 15);
          pushStr(np.out, s);
          continue;
        }
        const r = np.romanToInt();
        if (r < 0) throw engineError("invalid_text_representation", "invalid Roman numeral");
        const s = String(r);
        pushStr(np.number, s);
        np.numberP += s.length;
        num.pre = s.length;
        num.post = 0;
        continue;
      }
      case NUM_th:
      case NUM_TH:
        if (
          num.has(NUM_F_ROMAN) ||
          np.at(0) === C_HASH ||
          np.sign === C_MINUS ||
          num.has(NUM_F_DECIMAL)
        ) {
          continue;
        }
        if (isToChar) {
          pushStr(np.out, fmtOrdinal(cString(np.number), id === NUM_TH));
          continue;
        }
        np.eatNonDataChars(2);
        continue;
      case NUM_MI:
      case NUM_PL: {
        const want = id === NUM_PL ? C_PLUS : C_MINUS;
        if (isToChar) {
          if (np.sign === want) np.out.push(want);
          else if (!fill) np.out.push(C_SPACE);
          continue;
        }
        if (np.inb(np.inoutP) !== want) {
          np.eatNonDataChars(1);
          continue;
        }
        np.number[0] = want;
        break;
      }
      case NUM_SG: {
        if (isToChar) {
          np.out.push(np.sign);
          continue;
        }
        const c = np.inb(np.inoutP);
        if (c !== C_MINUS && c !== C_PLUS) {
          np.eatNonDataChars(1);
          continue;
        }
        np.number[0] = c;
        break;
      }
      default:
        continue;
    }
    // to_number: the action consumed one input character.
    np.inoutP++;
  }
  if (isToChar) {
    // PostgreSQL's output is a C string: anything after a NUL byte is dropped.
    const k = np.out.indexOf(0);
    return k >= 0 ? np.out.slice(0, k) : np.out;
  }
  if (np.numberP > 0 && np.number[np.numberP - 1] === C_POINT) {
    np.number = np.number.slice(0, np.numberP - 1);
  }
  num.post = np.readPost;
  return np.number;
}

// lastRelevantDecnum is formatting.c's get_last_relevant_decnum: the last nonzero digit after the
// decimal point, the point itself if there is none, or -1 without a point.
function lastRelevantDecnum(num: readonly number[]): number {
  const p = num.indexOf(C_POINT);
  if (p < 0) return -1;
  let result = p;
  for (let i = p + 1; i < num.length && num[i] !== 0; i++) {
    if (num[i] !== C_ZERO) result = i;
  }
  return result;
}

// --- the C printf conversions of a double ------------------------------------------------------

// roundDigitsHalfEven rounds the digit string c to its first keep digits, half to even on the
// exact remainder; carry reports a carry out of the top digit ("999" → "000", carry).
function roundDigitsHalfEven(c: string, keep: number): [string, boolean] {
  if (keep >= c.length) return [c + "0".repeat(keep - c.length), false];
  const head = strToBytes(c.slice(0, keep));
  const rest = c.slice(keep);
  let up = false;
  if (rest.charCodeAt(0) > 0x35) {
    up = true;
  } else if (rest.charCodeAt(0) === 0x35) {
    if (/[^0]/.test(rest.slice(1))) up = true;
    else up = keep > 0 && (head[keep - 1]! - 0x30) % 2 === 1;
  }
  if (!up) return [bytesToStr(head), false];
  for (let i = keep - 1; i >= 0; i--) {
    if (head[i] === 0x39) {
      head[i] = 0x30;
      continue;
    }
    head[i]!++;
    return [bytesToStr(head), false];
  }
  return [bytesToStr(head), true];
}

// canonicalParts is Go's Decimal.canonical: (neg, coefficient digits with trailing fractional
// zeros stripped, scale); zero is (false, "0", 0).
function canonicalParts(d: Decimal): [boolean, string, number] {
  const s = d.canonicalString(); // `${sign}${digits}e${scale}`
  const e = s.indexOf("e");
  return [s.charCodeAt(0) === C_MINUS, s.slice(1, e), Number(s.slice(e + 1))];
}

// exactDigits is |v|'s exact decimal expansion as (coefficient digits without leading zeros,
// scale); zero is ("", 0).
function exactDigits(v: number): [string, number] {
  const [, digits, scale] = canonicalParts(Decimal.exactFromFloat64(Math.abs(v)));
  if (digits === "0") return ["", 0];
  return [digits, scale];
}

function isNegFloat(v: number): boolean {
  return v < 0 || Object.is(v, -0);
}

// cFormatF is PostgreSQL's snprintf "%.*f": NaN and Infinity in PostgreSQL's spelling, the
// precision capped at 350 with the rest zero-filled.
function cFormatF(v: number, prec: number): string {
  if (Number.isNaN(v)) return "NaN";
  const sign = isNegFloat(v) ? "-" : "";
  if (!Number.isFinite(v)) return sign + "Infinity";
  const p = Math.min(prec, 350);
  const [c, s] = exactDigits(v);
  // Align to p fractional digits: the integer value round(|v| * 10^p).
  let scaled: string;
  if (c === "") {
    scaled = "";
  } else if (s <= p) {
    scaled = c + "0".repeat(p - s);
  } else {
    const keep = c.length - (s - p);
    if (keep < 0) {
      // Every kept digit is zero and the value is below half a unit: it rounds to zero.
      scaled = "";
    } else {
      let carry: boolean;
      [scaled, carry] = roundDigitsHalfEven(c, keep);
      if (carry) scaled = "1" + scaled;
    }
  }
  scaled = scaled.replace(/^0+/, "");
  if (scaled.length < p + 1) scaled = "0".repeat(p + 1 - scaled.length) + scaled;
  let out = scaled.slice(0, scaled.length - p);
  if (p > 0) out += "." + scaled.slice(scaled.length - p);
  return sign + out + "0".repeat(prec - p);
}

// cFormatE is PostgreSQL's snprintf "%+.*e" (forceSign) or "%.*e".
function cFormatE(v: number, prec: number, forceSign: boolean): string {
  if (Number.isNaN(v)) return "NaN";
  let sign = "";
  if (isNegFloat(v)) sign = "-";
  else if (forceSign) sign = "+";
  if (!Number.isFinite(v)) return sign + "Infinity";
  const p = Math.min(prec, 350);
  const [c, s] = exactDigits(v);
  let exp = 0;
  let digits: string;
  if (c === "") {
    digits = "0".repeat(p + 1);
  } else {
    exp = c.length - 1 - s;
    let carry: boolean;
    [digits, carry] = roundDigitsHalfEven(c, p + 1);
    if (carry) {
      digits = "1" + digits.slice(0, p);
      exp++;
    }
  }
  let out = digits.slice(0, 1);
  if (p > 0) out += "." + digits.slice(1);
  out += "0".repeat(prec - p);
  let es = "+";
  if (exp < 0) {
    es = "-";
    exp = -exp;
  }
  return sign + out + "e" + es + cPadInt(exp, 2);
}

// numericOutSci is numeric.c's numeric_out_sci: d as a significand rounded (half away from zero)
// to scale digits, then "e" and a signed exponent of at least two digits. A significand that
// rounds up to 10 stays 10 (PostgreSQL does not renormalize).
function numericOutSci(d: Decimal, scale: number): string {
  if (scale < 0) scale = 0;
  let [neg, digits, dscale] = canonicalParts(d);
  let exp = 0;
  let sig: string;
  if (digits === "0") {
    sig = "0";
    if (scale > 0) sig += "." + "0".repeat(scale);
    neg = false;
  } else {
    exp = digits.length - 1 - dscale;
    const keep = scale + 1;
    let head: string;
    if (keep >= digits.length) {
      head = digits + "0".repeat(keep - digits.length);
    } else {
      const hb = strToBytes(digits.slice(0, keep));
      if (digits.charCodeAt(keep) >= 0x35) {
        let carried = true;
        for (let i = keep - 1; i >= 0 && carried; i--) {
          if (hb[i] === 0x39) hb[i] = 0x30;
          else {
            hb[i]!++;
            carried = false;
          }
        }
        if (carried) hb.unshift(0x31);
      }
      head = bytesToStr(hb);
    }
    const ip = head.length - scale;
    sig = head.slice(0, ip);
    if (scale > 0) sig += "." + head.slice(ip);
  }
  let es = "+";
  if (exp < 0) {
    es = "-";
    exp = -exp;
  }
  const out = sig + "e" + es + cPadInt(exp, 2);
  return neg ? "-" + out : out;
}

// pow10 is glibc's pow(10, n) for a non-negative integer n (formatting.md §6.3): the nearest double
// to 10^n, which the decimal spelling "1e<n>" gives, except at the two exact halfway cases below
// 10^309 (n = 23 and n = 210), where glibc takes the upper neighbor instead of the even one
// (checked against PostgreSQL's 10::float8 ^ n for every n from 0 to 308).
function pow10(n: number): number {
  const f = Number("1e" + n);
  if (n === 23 || n === 210) {
    const dv = new DataView(new ArrayBuffer(8));
    dv.setFloat64(0, f);
    dv.setBigUint64(0, dv.getBigUint64(0) + 1n);
    return dv.getFloat64(0);
  }
  return f;
}

// --- to_char over numbers ----------------------------------------------------------------------

// numToCharFinish is NUM_TOCHAR_finish: run the processor over the laid-out number.
function numToCharFinish(
  nodes: FmtNode[],
  num: NumDesc,
  numstr: string,
  outPreSpaces: number,
  sign: number,
): string {
  const out = numProcessor(nodes, num, [], strToBytes(numstr), outPreSpaces, sign, true);
  return fromBytes(bytesToStr(out));
}

// numLayout splits a rendered number into its sign and digits and sizes the leading padding; a
// number wider than the template's integer digits becomes '#' fill.
function numLayout(orgnum: string, num: NumDesc): [string, number, number] {
  let sign = C_PLUS;
  let numstr = orgnum;
  if (orgnum.startsWith("-")) {
    sign = C_MINUS;
    numstr = orgnum.slice(1);
  }
  let preLen = numstr.indexOf(".");
  if (preLen < 0) preLen = numstr.length;
  let outPreSpaces = 0;
  if (preLen < num.pre) outPreSpaces = num.pre - preLen;
  else if (preLen > num.pre) numstr = hashFill(num.pre, num.post);
  return [numstr, sign, outPreSpaces];
}

// hashFill is the overflow fill: pre + post + 1 '#', with '.' at position pre.
function hashFill(pre: number, post: number): string {
  return "#".repeat(pre) + "." + "#".repeat(post);
}

// toCharNumberPrepare is NUM_TOCHAR_prepare: null for an empty template (the empty string).
function toCharNumberPrepare(tmpl: string): [FmtNode[], NumDesc] | null {
  if (tmpl === "") return null;
  const num = new NumDesc();
  const nodes = parseFormat(toBytes(tmpl), false, num);
  return [nodes, num];
}

// toCharInt is to_char(i16 / i32 / i64, text): int4_to_char for the 32-bit (and smaller) types,
// int8_to_char for i64 (whose EEEE goes through numeric, not float8).
export function toCharInt(v: bigint, is8: boolean, tmpl: string): string {
  const prep = toCharNumberPrepare(tmpl);
  if (prep === null) return "";
  const [nodes, num] = prep;
  if (num.has(NUM_F_ROMAN)) {
    const iv = v > BigInt(INT32_MAX) || v < BigInt(INT32_MIN) ? INT32_MAX : Number(v);
    return numToCharFinish(nodes, num, intToRoman(iv), 0, 0);
  }
  if (num.has(NUM_F_EEEE)) {
    let s: string;
    if (is8) {
      s = numericOutSci(Decimal.fromBigInt(v), num.post);
      if (!s.startsWith("-")) s = " " + s;
    } else {
      s = cFormatE(Number(v), num.post, true);
      if (s.startsWith("+")) s = " " + s.slice(1);
    }
    return numToCharFinish(nodes, num, s, 0, 0);
  }
  if (num.has(NUM_F_MULTI)) {
    v = intMulMulti(v, pow10(num.multi), is8);
    num.pre += num.multi;
  }
  let orgnum = String(v);
  let sign = C_PLUS;
  if (v < 0n) {
    sign = C_MINUS;
    orgnum = orgnum.slice(1);
  }
  const preLen = orgnum.length;
  let numstr = orgnum;
  if (num.post !== 0) numstr = orgnum + "." + "0".repeat(num.post);
  let outPreSpaces = 0;
  if (preLen < num.pre) outPreSpaces = num.pre - preLen;
  else if (preLen > num.pre) numstr = hashFill(num.pre, num.post);
  return numToCharFinish(nodes, num, numstr, outPreSpaces, sign);
}

// intMulMulti is the V multiplier on an integer: PostgreSQL's dtoi4 / dtoi8 of pow(10, n) and
// int4mul / int8mul, each 22003 out of range.
function intMulMulti(v: bigint, multi: number, is8: boolean): bigint {
  const m = roundToEven(multi);
  if (!is8) {
    const over = engineError("numeric_value_out_of_range", "integer out of range");
    if (Number.isNaN(m) || m < -2147483648.0 || m >= 2147483648.0) throw over;
    const r = v * BigInt(m);
    if (r > BigInt(INT32_MAX) || r < BigInt(INT32_MIN)) throw over;
    return r;
  }
  const over = engineError("numeric_value_out_of_range", "bigint out of range");
  if (Number.isNaN(m) || m < -TWO_63 || m >= TWO_63) throw over;
  const r = v * BigInt(m);
  if (r < I64_MIN || r > I64_MAX) throw over;
  return r;
}

// toCharDecimal is to_char(decimal, text) — numeric_to_char.
export function toCharDecimal(d: Decimal, tmpl: string): string {
  const prep = toCharNumberPrepare(tmpl);
  if (prep === null) return "";
  const [nodes, num] = prep;
  if (num.has(NUM_F_ROMAN)) {
    const r = d.toBigIntRound();
    const iv = r === null || r > BigInt(INT32_MAX) || r < BigInt(INT32_MIN) ? INT32_MAX : Number(r);
    return numToCharFinish(nodes, num, intToRoman(iv), 0, 0);
  }
  if (num.has(NUM_F_EEEE)) {
    let s = numericOutSci(d, num.post);
    if (!s.startsWith("-")) s = " " + s;
    return numToCharFinish(nodes, num, s, 0, 0);
  }
  let val = d;
  if (num.has(NUM_F_MULTI)) {
    val = d.mul(Decimal.fromDigitsScale(false, "1" + "0".repeat(num.multi), 0));
    num.pre += num.multi;
  }
  const x = val.roundPlaces(num.post);
  const [numstr, sign, outPreSpaces] = numLayout(x.render(), num);
  return numToCharFinish(nodes, num, numstr, outPreSpaces, sign);
}

// toCharFloat is to_char(f32 / f64, text) — float4_to_char / float8_to_char. The digits beyond
// FLT_DIG (6) / DBL_DIG (15) significant ones are dropped, as PostgreSQL does.
export function toCharFloat(f: number, isF32: boolean, tmpl: string): string {
  const prep = toCharNumberPrepare(tmpl);
  if (prep === null) return "";
  const [nodes, num] = prep;
  if (num.has(NUM_F_ROMAN)) {
    let iv = INT32_MAX;
    const r = roundToEven(f);
    if (!Number.isNaN(r) && r >= -2147483648.0 && r < 2147483648.0) iv = r;
    return numToCharFinish(nodes, num, intToRoman(iv), 0, 0);
  }
  if (num.has(NUM_F_EEEE)) {
    let s: string;
    if (!Number.isFinite(f)) {
      const b = strToBytes("#".repeat(num.pre + num.post + 6));
      b[0] = C_SPACE;
      b[num.pre + 1] = C_POINT;
      s = bytesToStr(b);
    } else {
      s = cFormatE(f, num.post, true);
      if (s.startsWith("+")) s = " " + s.slice(1);
    }
    return numToCharFinish(nodes, num, s, 0, 0);
  }
  let val = f;
  if (num.has(NUM_F_MULTI)) {
    const multi = pow10(num.multi);
    val = isF32 ? Math.fround(Math.fround(f) * Math.fround(multi)) : f * multi;
    num.pre += num.multi;
  }
  const dig = isF32 ? 6 : 15;
  const preLen = cFormatF(Math.abs(val), 0).length;
  if (preLen >= dig) num.post = 0;
  else if (preLen + num.post > dig) num.post = dig - preLen;
  const [numstr, sign, outPreSpaces] = numLayout(cFormatF(val, num.post), num);
  return numToCharFinish(nodes, num, numstr, outPreSpaces, sign);
}

// --- to_number ---------------------------------------------------------------------------------

// toNumberText is to_number(text, text) → decimal (numeric_to_number): the digits the template
// reads, as numeric(precision, scale) of the template's digit counts, then scaled by any V. An
// empty template is NULL.
export function toNumberText(input: string, tmpl: string, m: Meter): Decimal | null {
  if (tmpl === "") return null;
  const num = new NumDesc();
  const nodes = parseFormat(toBytes(tmpl), false, num);
  const numstr = numProcessor(nodes, num, strToBytes(toBytes(input)), [], 0, 0, false);
  const scale = num.post;
  const precision = num.pre + num.multi + scale;
  let d = parseNumericIn(bytesToStr(numstr));
  d = applyNumericTypmod(d, precision, scale);
  if (num.has(NUM_F_MULTI)) {
    const p = Decimal.decPower(Decimal.fromBigInt(10n), Decimal.fromBigInt(BigInt(-num.multi)), m);
    d = d.mul(p);
  }
  return d;
}

// parseNumericIn reads the processor's digits as numeric_in does: [sign] digits [. digits],
// surrounded by optional whitespace.
function parseNumericIn(s: string): Decimal {
  const bad = (): EngineError =>
    engineError(
      "invalid_text_representation",
      `invalid input syntax for type numeric: "${fromBytes(s)}"`,
    );
  let i = 0;
  while (i < s.length && isSpaceC(s.charCodeAt(i))) i++;
  let neg = false;
  if (i < s.length && (s.charCodeAt(i) === C_PLUS || s.charCodeAt(i) === C_MINUS)) {
    neg = s.charCodeAt(i) === C_MINUS;
    i++;
  }
  let intPart = "";
  let frac = "";
  while (i < s.length && isDigitC(s.charCodeAt(i))) intPart += s[i++];
  if (i < s.length && s.charCodeAt(i) === C_POINT) {
    i++;
    while (i < s.length && isDigitC(s.charCodeAt(i))) frac += s[i++];
  }
  while (i < s.length && isSpaceC(s.charCodeAt(i))) i++;
  if (i !== s.length || intPart.length + frac.length === 0) throw bad();
  return Decimal.fromDigitsScale(neg, intPart + frac, frac.length).checkCap();
}

// applyNumericTypmod is numeric.c's apply_typmod: round to scale, then 22003 when the integer
// digits exceed precision − scale.
function applyNumericTypmod(d: Decimal, precision: number, scale: number): Decimal {
  const r = d.roundToScale(scale);
  if (r.isZero()) return r;
  const [, digits, rs] = canonicalParts(r);
  if (digits.length - rs > precision - scale) {
    throw engineError("numeric_value_out_of_range", "numeric field overflow");
  }
  return r;
}
