// The entropy + clock seam (spec/design/entropy.md) — two host-injectable functions that feed the
// volatile UUID generators (uuidv4/uuidv7), each defaulting to the platform primitive:
//
//   - the RANDOM SOURCE — fills N bytes; default = the OS CSPRNG (node:crypto), drawn PER VALUE (so
//     production UUIDs are unpredictable, not derived from a single seeded PRNG).
//   - the CLOCK SOURCE  — returns micros since the Unix epoch; default = the wall clock (Date.now).
//
// A host injects its own functions for reproducibility (e.g. a controllable clock, or the provided
// seededRandomSource below). The conformance harness injects exactly those via the # seed: /
// # clock: directives, which is what makes the generators byte-identical across cores. The engine
// itself contains NO production PRNG — splitmix64 lives here only as the provided DETERMINISTIC
// source a caller may opt into; it is never the default.
//
// The PRNG runs over `bigint` masked to 64 bits — JS numbers are f64, so splitmix64's 64-bit
// arithmetic (and the i64 clock/ms) must use bigint, the same discipline as the cost counter.

import { engineError } from "./errors.ts";
import { buildUuidV4, buildUuidV7 } from "./uuid.ts";

// A host random source: fills `buf` with buf.length random bytes. A deterministic source (e.g.
// seededRandomSource) advances its own captured state per call.
export type RandomFill = (buf: Uint8Array) => void;
// A host clock source: returns micros since the Unix epoch (a host may inject an advancing clock).
export type ClockFunc = () => bigint;

// Seam is the host seam carried on the Engine handle (spec/design/api.md §10): the injected random
// + clock functions, each undefined ⇒ the platform default. Only the volatile uuid generators touch
// it; every other expression ignores it.
export class Seam {
  randomFill?: RandomFill;
  clock?: ClockFunc;

  // fill writes buf.length random bytes: the injected source, else the OS CSPRNG via the Web Crypto
  // global (crypto.getRandomValues). The global is present in both browsers/workers AND Node ≥19, so
  // the engine core imports no `node:*` here and runs unchanged in a browser bundle (the OPFS host) —
  // identical entropy semantics, no behavior change (the default source is not conformance-checked; the
  // # seed: directive injects seededRandomSource). getRandomValues fills ≤ 65536 bytes per call; uuid
  // fills are 8/16 bytes, but chunk for generality.
  fill(buf: Uint8Array): void {
    if (this.randomFill) {
      this.randomFill(buf);
      return;
    }
    const QUOTA = 65536;
    for (let off = 0; off < buf.length; off += QUOTA) {
      crypto.getRandomValues(buf.subarray(off, Math.min(off + QUOTA, buf.length)));
    }
  }

  // nowMicros returns the current time in micros since the Unix epoch: the injected clock, else the
  // wall clock.
  nowMicros(): bigint {
    return this.clock ? this.clock() : BigInt(Date.now()) * 1000n;
  }
}

const MASK64 = 0xffff_ffff_ffff_ffffn;
// splitmix64 constants (entropy.md §2; identical to the bench PRNG, re-authored as engine data).
const GAMMA = 0x9e3779b97f4a7c15n;
const MIX1 = 0xbf58476d1ce4e5b9n;
const MIX2 = 0x94d049bb133111ebn;
const TWO_POW_48 = 1n << 48n;

// seededRandomSource is the provided DETERMINISTIC random source: a splitmix64 stream seeded with
// `seed`, serialized big-endian in 8-byte chunks (a final partial chunk takes the high bytes of one
// more draw — never hit by the 16-/8-byte uuid fills). This is what a host injects for
// reproducibility and what the conformance harness injects for the # seed: directive; it is
// byte-pinned in spec/encoding/prng.toml and asserted cross-core (entropy.md §2). Not the default.
export function seededRandomSource(seed: bigint): RandomFill {
  let state = BigInt.asUintN(64, seed);
  return (buf: Uint8Array) => {
    let i = 0;
    while (i < buf.length) {
      state = (state + GAMMA) & MASK64;
      let x = state;
      x = ((x ^ (x >> 30n)) * MIX1) & MASK64;
      x = ((x ^ (x >> 27n)) * MIX2) & MASK64;
      const v = x ^ (x >> 31n);
      const n = Math.min(8, buf.length - i);
      for (let k = 0; k < n; k++) buf[i + k] = Number((v >> BigInt(8 * (7 - k))) & 0xffn);
      i += n;
    }
  };
}

// fixedClock is the provided FIXED clock source: always returns `micros`. The # clock: directive
// injects this (entropy.md §6); a host wanting a frozen instant uses it too.
export function fixedClock(micros: bigint): ClockFunc {
  return () => micros;
}

// advancingClock is the provided ADVANCING clock source: returns start, then start+step,
// start+2·step, … — one increment per read (captured state). The # clock_advance: directive injects
// this (entropy.md §6) to make clock_timestamp()'s per-call reads deterministic and distinguishable
// from the statement-stable now() cross-core; the draw order follows expression-evaluation order.
export function advancingClock(start: bigint, step: bigint): ClockFunc {
  let cur = start;
  return () => {
    const v = cur;
    cur += step;
    return v;
  };
}

// StmtRng is the per-statement mutable seam state: the uuidv7 monotonic counter and the
// once-resolved statement clock (entropy.md §5 — read once, reused, so a statement's time cannot
// vary row-to-row). The PRNG state itself lives in the injected RandomFill (handle-scoped).
export class StmtRng {
  private counter = 0;
  private clock = 0n;
  private clockResolved = false;

  // The statement clock in micros since the Unix epoch, resolved once (entropy.md §5): the seam's
  // clock source. Reused for every uuidv7 / now() in the statement (STABLE).
  statementClockMicros(seam: Seam): bigint {
    if (!this.clockResolved) {
      this.clock = seam.nowMicros();
      this.clockResolved = true;
    }
    return this.clock;
  }

  // A fresh read of the clock seam in micros since the Unix epoch — used by clock_timestamp()
  // (entropy.md §5), which reads on EVERY call (VOLATILE) and so does NOT touch the once-resolved
  // statement clock above. It caches nothing.
  clockNowMicros(seam: Seam): bigint {
    return seam.nowMicros();
  }

  // uuidv4 — 16 bytes from the seam's random source, version/variant overwritten (entropy.md §3).
  uuidV4(seam: Seam): Uint8Array {
    const b = new Uint8Array(16);
    seam.fill(b);
    return buildUuidV4(b);
  }

  // uuidv7 — the 48-bit ms of shiftedMicros (the statement clock, possibly interval-shifted by the
  // caller), a per-statement monotonic counter in rand_a, and 62 random bits (8 bytes from the
  // seam) in rand_b (entropy.md §3). An out-of-48-bit ms traps 22008.
  uuidV7(seam: Seam, shiftedMicros: bigint): Uint8Array {
    const unixMs = floorDiv(shiftedMicros, 1000n);
    if (unixMs < 0n || unixMs >= TWO_POW_48) {
      throw engineError("datetime_field_overflow", "uuidv7 timestamp out of range");
    }
    const counter = this.counter & 0x0fff;
    this.counter++;
    const randB = new Uint8Array(8);
    seam.fill(randB);
    return buildUuidV7(unixMs, counter, randB);
  }
}

// floorDiv divides toward negative infinity (so a pre-epoch micros maps to a negative ms that fails
// the range check, matching Rust's div_euclid / Go's floorDiv).
function floorDiv(a: bigint, b: bigint): bigint {
  let q = a / b;
  if (a % b !== 0n && a < 0n !== b < 0n) q -= 1n;
  return q;
}
