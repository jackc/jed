// Format v54 (spec/fileformat/format.md "Leaf node", spec/design/leaf-write-cost.md §8–§9): a
// base-plus-offsets region with exceptions. A fixed column eligible for base plus offsets may take
// region kind 6:
//
//   null bitmap | base (w) | offset width bw (u8) | E (u8) | E exception rows (u16, ascending)
//   | E exception bodies (w) | N offsets (bw)
//
// A layout is a width bw narrower than kind 1's and a base among the present values; its exceptions
// are the present values outside [base, base + 2^(8·bw) − 1], at most 16, each stored whole with
// offset zero. The region takes the layout of least size (E + 1)·(w + 2) + N·bw, the narrower width
// and then the lower base on a tie, when it saves more than kind 1. The exceptions are some of the 16
// smallest and 16 largest present values, so a column's size facts are its 17 smallest and 17
// largest (PforAcc). A reader reads the region in place (format.ts pfBody): row i is its exception's
// body when i is an exception row (a bucket mask rejects most rows before the search), else the base
// plus offset i.
//
// Pure integer rules with no dependency on the value codec (like leaf_compact.ts).

import { forWidth, forWidthN } from "./leaf_compact.ts";

// REGION_PFOR is the region kind byte of a base-plus-offsets region with exceptions.
export const REGION_PFOR = 6;

// PFOR_MAX_K is the most values a kind 6 region stores whole.
export const PFOR_MAX_K = 16;

const K1 = PFOR_MAX_K + 1;

// pforCap is the exception cap this process writes with: PFOR_MAX_K, as format v54 requires. Tests
// lower it to measure other caps, or set 0 to compare a workload with the regions kind 1 alone
// gives. The facts always keep 17 extremes, which serve any lower cap.
let pforCap = PFOR_MAX_K;

// setPforCap sets the exception cap (tests only); facts measured under the old cap must not be reused.
export function setPforCap(k: number): void {
  pforCap = Math.max(0, Math.min(k, PFOR_MAX_K));
}

// pforStats counts the kind 6 regions written and their exceptions (tests).
export const pforStats = { regions: 0, exceptions: 0 };

// The offset widths a layout may take, indexed as PforAcc's cache, and each width's largest offset
// (as a number for a column of width 4 or less, as a bigint for a wider one).
const PFOR_WIDTHS = [0, 1, 2, 4];
const PFOR_RANGE_N = [0, 0xff, 0xffff, 0xffff_ffff];
const PFOR_RANGE_B = [0n, 0xffn, 0xffffn, 0xffff_ffffn];

// A column value as the facts hold it: a number for a column of width 4 or less, a bigint for a wider
// one (one instance holds one kind). Only order and differences matter, so any order-preserving
// shift of the stored slots (the signed value, or the unsigned slot) measures the same.
type PV = number | bigint;

// PforPlan is a kind 6 layout: offsets of width bw from base, e exceptions, and the region's size
// past its kind byte and null bitmap.
export type PforPlan = { bw: number; base: PV; e: number; size: number };

// PforAcc is a column's kind 6 size facts: its k+1 smallest and k+1 largest present values.
export class PforAcc {
  // big says the values are bigints (an 8-byte column).
  readonly big: boolean;
  // lo is the 17 smallest present values, hi the 17 largest, each ascending.
  lo: PV[];
  hi: PV[];
  // np is the present values.
  np: number;
  // The least exception count for each offset width 0, 1, 2, 4 and the lowest base giving it, a
  // function of lo and hi alone (not of the row count), so a saving query is four products.
  private cached: boolean;
  private readonly ce: number[];
  private readonly cb: PV[];
  private readonly cok: boolean[];
  // full says lo and hi each hold 17 values and enough values lie between them that a value in
  // [skipLo, skipHi] = [lo[16], hi[0]] changes nothing. skipLoN / skipHiN are those bounds as numbers
  // (rounded, for a bigint): a value whose rounding lies strictly between them lies between the
  // bounds, which lets a caller reject an 8-byte value before building its bigint.
  full: boolean;
  skipLo: PV;
  skipHi: PV;
  skipLoN: number;
  skipHiN: number;

  constructor(big: boolean) {
    this.big = big;
    this.lo = [];
    this.hi = [];
    this.np = 0;
    this.cached = false;
    this.ce = [0, 0, 0, 0];
    this.cb = big ? [0n, 0n, 0n, 0n] : [0, 0, 0, 0];
    this.cok = [false, false, false, false];
    this.full = false;
    this.skipLo = big ? 0n : 0;
    this.skipHi = big ? 0n : 0;
    this.skipLoN = 0;
    this.skipHiN = 0;
  }

  clone(): PforAcc {
    const out = new PforAcc(this.big);
    out.lo = this.lo.slice();
    out.hi = this.hi.slice();
    out.np = this.np;
    out.cached = this.cached;
    for (let x = 0; x < 4; x++) {
      out.ce[x] = this.ce[x]!;
      out.cb[x] = this.cb[x]!;
      out.cok[x] = this.cok[x]!;
    }
    out.full = this.full;
    out.skipLo = this.skipLo;
    out.skipHi = this.skipHi;
    out.skipLoN = this.skipLoN;
    out.skipHiN = this.skipHiN;
    return out;
  }

  add(v: PV): void {
    this.np++;
    if (this.full && v >= this.skipLo && v <= this.skipHi) return; // neither among the extremes
    this.addSlow(v);
  }

  private addSlow(v: PV): void {
    if (this.np <= 2 * K1 + 1) {
      this.cached = false; // whether more values lie beyond hi can still change
    } else {
      this.full = true;
    }
    const lo = this.lo;
    if (lo.length < K1 || v < lo[K1 - 1]!) {
      let i = lo.length;
      if (i < K1) lo.push(v);
      else i = K1 - 1; // the largest kept value drops out
      while (i > 0 && lo[i - 1]! > v) {
        lo[i] = lo[i - 1]!;
        i--;
      }
      lo[i] = v;
      this.cached = false;
    }
    const hi = this.hi;
    if (hi.length < K1) {
      hi.push(v);
      let i = hi.length - 1;
      while (i > 0 && hi[i - 1]! > v) {
        hi[i] = hi[i - 1]!;
        i--;
      }
      hi[i] = v;
      this.cached = false;
    } else if (v > hi[0]!) {
      let i = 0; // the smallest kept value drops out
      while (i + 1 < K1 && hi[i + 1]! < v) {
        hi[i] = hi[i + 1]!;
        i++;
      }
      hi[i] = v;
      this.cached = false;
    }
    if (this.full) {
      this.skipLo = lo[K1 - 1]!;
      this.skipHi = hi[0]!;
      this.skipLoN = Number(this.skipLo);
      this.skipHiN = Number(this.skipHi);
    }
  }

  // remove takes present value v out, reporting whether the facts may change, so the column must be
  // measured again (the facts are then left as they are): while few values are present, or when v is
  // one of the kept extremes. Otherwise v lies strictly between them and only the count changes.
  remove(v: PV): boolean {
    if (this.np <= 2 * K1) return true;
    if (v <= this.lo[this.lo.length - 1]! || v >= this.hi[0]!) return true;
    this.np--;
    return false;
  }

  // bestWidth is the cache index of the smallest kind 6 layout of a w-byte column of n rows (a width
  // narrower than kind 1's, the narrower width first on a tie), or -1 when none is admissible; its
  // size is then (ce + 1)·(w + 2) + n·bw.
  private bestWidth(n: number, w: number): number {
    if (this.np === 0) return -1;
    if (!this.cached) this.measure();
    const k1bw = this.big
      ? forWidth(w, (this.hi[this.hi.length - 1] as bigint) - (this.lo[0] as bigint))
      : forWidthN(w, (this.hi[this.hi.length - 1] as number) - (this.lo[0] as number));
    let best = -1;
    let size = 0;
    for (let x = 0; x < 4; x++) {
      const bw = PFOR_WIDTHS[x]!;
      if (bw >= k1bw) break;
      if (!this.cok[x]) continue;
      const s = (this.ce[x]! + 1) * (w + 2) + n * bw;
      if (best < 0 || s < size) {
        best = x;
        size = s;
      }
    }
    return best;
  }

  // plan is the smallest kind 6 layout of a w-byte column of n rows, or null when none is admissible.
  plan(n: number, w: number): PforPlan | null {
    const x = this.bestWidth(n, w);
    if (x < 0) return null;
    const bw = PFOR_WIDTHS[x]!;
    const e = this.ce[x]!;
    return { bw, base: this.cb[x]!, e, size: (e + 1) * (w + 2) + n * bw };
  }

  // saving is the bytes a kind 6 region saves over the plain one (0 when it does not).
  saving(n: number, w: number): number {
    const x = this.bestWidth(n, w);
    if (x < 0) return 0;
    const s = n * w - ((this.ce[x]! + 1) * (w + 2) + n * PFOR_WIDTHS[x]!);
    return s > 0 ? s : 0;
  }

  // measure fills the per-width cache: for each width, the least exception count over bases (at most
  // 16) and the lowest base giving it. A base is one of the 17 smallest values (more below it would
  // be more than 16 exceptions); the values above its window are counted from hi, whose boundary
  // moves only up as the base rises.
  private measure(): void {
    this.cached = true;
    const lo = this.lo;
    const hi = this.hi;
    const nh = hi.length;
    for (let x = 0; x < 4; x++) {
      this.cok[x] = false;
      let idx = 0; // hi[idx:] are the kept values above the window
      for (let j = 0; j < lo.length; j++) {
        if (j > 0 && lo[j] === lo[j - 1]) continue;
        const base = lo[j]!;
        const top: PV = this.big
          ? (base as bigint) + PFOR_RANGE_B[x]!
          : (base as number) + PFOR_RANGE_N[x]!;
        while (idx < nh && hi[idx]! <= top) idx++;
        if (idx === 0 && this.np > nh) continue; // more than 16 above
        const e = j + nh - idx;
        if (e > pforCap) continue;
        if (!this.cok[x] || e < this.ce[x]!) {
          this.ce[x] = e;
          this.cb[x] = base;
          this.cok[x] = true;
        }
      }
    }
  }
}
