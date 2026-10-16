// The entropy + clock seam (spec/design/entropy.md, spec/design/environment.md §4) — the two
// host-supplied sources that feed the volatile UUID generators (uuidv4/uuidv7) and the clock
// functions. An Environment fixes WHICH source is installed; each defaults to the platform primitive:
//
//   - the RANDOM SOURCE — fills N bytes; default = the OS CSPRNG (osRandomSource), drawn PER VALUE (so
//     production UUIDs are unpredictable, not derived from a single seeded PRNG).
//   - the CLOCK SOURCE  — returns micros since the Unix epoch; default = the wall clock (wallClock).
//
// A source is an object the host may keep and control: the provided SeededRandom (reseed), ManualClock
// (set / advance), and AdvancingClock (reset) are what a host installs for reproducibility, and what
// the conformance harness drives for the # seed: / # clock: / # clock_advance: directives — which is
// what makes the generators byte-identical across cores. The engine itself contains NO production
// PRNG — splitmix64 lives here only as the provided DETERMINISTIC source a caller may opt into; it is
// never the default.
//
// The PRNG runs over `bigint` masked to 64 bits — JS numbers are f64, so splitmix64's 64-bit
// arithmetic (and the i64 clock/ms) must use bigint, the same discipline as the cost counter.

import type { Environment } from "./environment.ts";
import { engineError } from "./errors.ts";
import { buildUuidV4, buildUuidV7 } from "./uuid.ts";

// RandomSource is a host random source: fill writes buf.length random bytes into buf. A deterministic
// source (e.g. SeededRandom) advances its own state per call. One source may serve every database and
// session using its environment, so its draws interleave in call order (environment.md §4).
export interface RandomSource {
  fill(buf: Uint8Array): void;
}

// ClockSource is a host clock source: nowMicros returns micros since the Unix epoch.
export interface ClockSource {
  nowMicros(): bigint;
}

// The OS CSPRNG via the Web Crypto global (crypto.getRandomValues). The global is present in both
// browsers/workers AND Node ≥19, so the engine core imports no `node:*` here and runs unchanged in a
// browser bundle (the OPFS host). getRandomValues fills ≤ 65536 bytes per call; uuid fills are 8/16
// bytes, but chunk for generality. Stateless, so one shared instance serves every environment.
const OS_RANDOM: RandomSource = {
  fill(buf: Uint8Array): void {
    const QUOTA = 65536;
    for (let off = 0; off < buf.length; off += QUOTA) {
      crypto.getRandomValues(
        buf.subarray(off, Math.min(off + QUOTA, buf.length)) as Uint8Array<ArrayBuffer>,
      );
    }
  },
};

// The wall clock (Date.now, in micros). Stateless, so one shared instance serves every environment.
const WALL_CLOCK: ClockSource = {
  nowMicros(): bigint {
    return BigInt(Date.now()) * 1000n;
  },
};

// osRandomSource is the default random source: fresh OS entropy per value (no control).
export function osRandomSource(): RandomSource {
  return OS_RANDOM;
}

// wallClock is the default clock source: the wall clock (no control).
export function wallClock(): ClockSource {
  return WALL_CLOCK;
}

const MASK64 = 0xffff_ffff_ffff_ffffn;
// splitmix64 constants (entropy.md §2; identical to the bench PRNG, re-authored as engine data).
const GAMMA = 0x9e3779b97f4a7c15n;
const MIX1 = 0xbf58476d1ce4e5b9n;
const MIX2 = 0x94d049bb133111ebn;
const TWO_POW_48 = 1n << 48n;

// SeededRandom is the provided DETERMINISTIC random source: a splitmix64 stream seeded with `seed`,
// serialized big-endian in 8-byte chunks (a final partial chunk takes the high bytes of one more
// draw — never hit by the 16-/8-byte uuid fills). This is what a host installs for reproducibility
// and what the conformance harness installs for the # seed: directive; it is byte-pinned in
// spec/encoding/prng.toml and asserted cross-core (entropy.md §2). Not the default.
export class SeededRandom implements RandomSource {
  private state: bigint;

  constructor(seed: bigint) {
    this.state = BigInt.asUintN(64, seed);
  }

  // reseed restarts the stream exactly as a fresh SeededRandom(seed) would (environment.md §4).
  reseed(seed: bigint): void {
    this.state = BigInt.asUintN(64, seed);
  }

  fill(buf: Uint8Array): void {
    let i = 0;
    while (i < buf.length) {
      this.state = (this.state + GAMMA) & MASK64;
      let x = this.state;
      x = ((x ^ (x >> 30n)) * MIX1) & MASK64;
      x = ((x ^ (x >> 27n)) * MIX2) & MASK64;
      const v = x ^ (x >> 31n);
      const n = Math.min(8, buf.length - i);
      for (let k = 0; k < n; k++) buf[i + k] = Number((v >> BigInt(8 * (7 - k))) & 0xffn);
      i += n;
    }
  }
}

// seededRandomSource returns a fresh SeededRandom over `seed`.
export function seededRandomSource(seed: bigint): SeededRandom {
  return new SeededRandom(seed);
}

// ManualClock is the provided MANUAL clock source: returns the instant it was last set to, until the
// host sets or advances it (environment.md §4). The # clock: directive sets one; a host wanting a
// frozen instant uses it too.
export class ManualClock implements ClockSource {
  private micros: bigint;

  constructor(micros: bigint) {
    this.micros = micros;
  }

  // set makes every later read return `micros`.
  set(micros: bigint): void {
    this.micros = micros;
  }

  // advance moves the clock forward by `step` micros (negative moves it back).
  advance(step: bigint): void {
    this.micros += step;
  }

  nowMicros(): bigint {
    return this.micros;
  }
}

// manualClock returns a fresh ManualClock set to `micros`.
export function manualClock(micros: bigint): ManualClock {
  return new ManualClock(micros);
}

// AdvancingClock is the provided ADVANCING clock source: returns start, then start+step,
// start+2·step, … — one increment per read. The # clock_advance: directive installs this
// (entropy.md §6) to make clock_timestamp()'s per-call reads deterministic and distinguishable from
// the statement-stable now() cross-core; the draw order follows expression-evaluation order.
export class AdvancingClock implements ClockSource {
  private cur: bigint;
  private step: bigint;

  constructor(start: bigint, step: bigint) {
    this.cur = start;
    this.step = step;
  }

  // reset restarts the sequence exactly as a fresh AdvancingClock(start, step) would.
  reset(start: bigint, step: bigint): void {
    this.cur = start;
    this.step = step;
  }

  nowMicros(): bigint {
    const v = this.cur;
    this.cur += this.step;
    return v;
  }
}

// advancingClock returns a fresh AdvancingClock.
export function advancingClock(start: bigint, step: bigint): AdvancingClock {
  return new AdvancingClock(start, step);
}

// StmtRng is the per-statement mutable seam state: the uuidv7 monotonic counter and the
// once-resolved statement clock (entropy.md §5 — read once, reused, so a statement's time cannot
// vary row-to-row). The PRNG state itself lives in the environment's random source.
export class StmtRng {
  private counter = 0;
  private clock = 0n;
  private clockResolved = false;

  // The statement clock in micros since the Unix epoch, resolved once (entropy.md §5): the
  // environment's clock source. Reused for every uuidv7 / now() in the statement (STABLE).
  statementClockMicros(env: Environment): bigint {
    if (!this.clockResolved) {
      this.clock = env.clockSource().nowMicros();
      this.clockResolved = true;
    }
    return this.clock;
  }

  // A fresh read of the environment's clock in micros since the Unix epoch — used by
  // clock_timestamp() (entropy.md §5), which reads on EVERY call (VOLATILE) and so does NOT touch the
  // once-resolved statement clock above. It caches nothing.
  clockNowMicros(env: Environment): bigint {
    return env.clockSource().nowMicros();
  }

  // uuidv4 — 16 bytes from the environment's random source, version/variant overwritten
  // (entropy.md §3).
  uuidV4(env: Environment): Uint8Array {
    const b = new Uint8Array(16);
    env.randomSource().fill(b);
    return buildUuidV4(b);
  }

  // uuidv7 — the 48-bit ms of shiftedMicros (the statement clock, possibly interval-shifted by the
  // caller), a per-statement monotonic counter in rand_a, and 62 random bits (8 bytes from the
  // environment's random source) in rand_b (entropy.md §3). An out-of-48-bit ms traps 22008.
  uuidV7(env: Environment, shiftedMicros: bigint): Uint8Array {
    const unixMs = floorDiv(shiftedMicros, 1000n);
    if (unixMs < 0n || unixMs >= TWO_POW_48) {
      throw engineError("datetime_field_overflow", "uuidv7 timestamp out of range");
    }
    const counter = this.counter & 0x0fff;
    this.counter++;
    const randB = new Uint8Array(8);
    env.randomSource().fill(randB);
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
