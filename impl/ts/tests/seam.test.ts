// The entropy+clock seam (spec/design/entropy.md §2/§3, environment.md §4) — the provided splitmix64
// source over bigint, the controllable clocks, and the uuid generators. Mirrors the Rust/Go seam
// tests; the byte-exact cross-core agreement is also pinned by the conformance corpus
// (expr/uuid_generate.test).

import assert from "node:assert/strict";
import { test } from "node:test";
import { Environment } from "../src/environment.ts";
import {
  AdvancingClock,
  advancingClock,
  ManualClock,
  manualClock,
  SeededRandom,
  seededRandomSource,
  StmtRng,
} from "../src/seam.ts";
import { parseUuid } from "../src/value.ts";
import { uuidExtractTimestampMicros, uuidExtractVersion } from "../src/uuid.ts";

// An environment on the provided deterministic random source (seed) + a manual clock — the test path.
function seeded(seed: bigint, clock: bigint = 0n): Environment {
  return Environment.builder()
    .randomSource(seededRandomSource(seed))
    .clockSource(manualClock(clock))
    .build();
}

function eq(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && a.every((x, i) => x === b[i]);
}

// seed 1 → 910a2dec89025cc1, beeb8da1658eec67, f893a2eefb32555e (big-endian; entropy.md §2).
const SEED1_STREAM = Uint8Array.from([
  0x91, 0x0a, 0x2d, 0xec, 0x89, 0x02, 0x5c, 0xc1, 0xbe, 0xeb, 0x8d, 0xa1, 0x65, 0x8e, 0xec, 0x67,
  0xf8, 0x93, 0xa2, 0xee, 0xfb, 0x32, 0x55, 0x5e,
]);

test("the provided seeded source fills bytes from the pinned splitmix64 stream", () => {
  const src = seededRandomSource(1n);
  const buf = new Uint8Array(24);
  src.fill(buf);
  assert.ok(eq(buf, SEED1_STREAM));
});

test("reseed restarts the stream exactly as a fresh source", () => {
  const src = new SeededRandom(7n);
  src.fill(new Uint8Array(40)); // advance it
  src.reseed(1n);
  const buf = new Uint8Array(24);
  src.fill(buf);
  assert.ok(eq(buf, SEED1_STREAM));
});

test("manual clock reads what it was set to until set or advanced", () => {
  const clk = new ManualClock(500n);
  assert.equal(clk.nowMicros(), 500n);
  assert.equal(clk.nowMicros(), 500n);
  clk.set(1_000n);
  assert.equal(clk.nowMicros(), 1_000n);
  clk.advance(25n);
  assert.equal(clk.nowMicros(), 1_025n);
});

test("uuidv4 is deterministic and well-formed", () => {
  const r = new StmtRng();
  const b = r.uuidV4(seeded(1n));
  // seed 1 → splitmix64 910a2dec89025cc1, beeb8da1658eec67 → version 4 + RFC variant.
  const want = parseUuid("910a2dec-8902-4cc1-beeb-8da1658eec67");
  assert.ok(!("error" in want));
  assert.ok(eq(b, (want as { bytes: Uint8Array }).bytes));
  assert.equal(uuidExtractVersion(b), 4n);
});

test("uuidv7 embeds the clock and is monotonic within a statement", () => {
  const clock = 1_721_056_591_872_000n;
  const r = new StmtRng();
  const s = seeded(42n, clock);
  const a = r.uuidV7(s, clock);
  const b = r.uuidV7(s, clock);
  assert.equal(uuidExtractVersion(a), 7n);
  assert.equal(uuidExtractTimestampMicros(a), 1_721_056_591_872_000n);
  // Same statement clock → ordered by the per-statement counter (byte-lexicographic a < b).
  assert.ok(Buffer.from(a).compare(Buffer.from(b)) < 0, "uuidv7 must be monotonic");
});

test("the default environment uses OS entropy and the wall clock", () => {
  // The PRODUCTION path: an empty environment → crypto.getRandomValues per draw + Date.
  const env = new Environment();
  const r = new StmtRng();
  const v4 = r.uuidV4(env);
  assert.equal(uuidExtractVersion(v4), 4n);
  const v7 = r.uuidV7(env, r.statementClockMicros(env));
  // A plausible wall-clock instant (after 2020-01-01).
  assert.ok(uuidExtractTimestampMicros(v7)! > 1_577_836_800_000_000n);
});

test("uuidv7 rejects an out-of-range (pre-epoch) clock", () => {
  const r = new StmtRng();
  assert.throws(() => r.uuidV7(seeded(1n), -1_000_000n));
});

test("advancing clock steps per read; now() caches while clock_timestamp() advances", () => {
  // The advancing clock yields start, start+step, … one increment per read (entropy.md §6).
  const clk = advancingClock(1000n, 1n);
  assert.equal(clk.nowMicros(), 1000n);
  assert.equal(clk.nowMicros(), 1001n);
  assert.equal(clk.nowMicros(), 1002n);
  // reset restarts the sequence exactly as a fresh clock.
  clk.reset(50n, 10n);
  assert.equal(clk.nowMicros(), 50n);
  assert.equal(clk.nowMicros(), 60n);
  // now() (statementClockMicros) reads ONCE and caches: it pulls 1000 then stays 1000 even as
  // clock_timestamp() (clockNowMicros) keeps advancing the SAME source — the stable-vs-volatile
  // distinction, made deterministic.
  const env = Environment.builder().clockSource(new AdvancingClock(1000n, 1n)).build();
  const r = new StmtRng();
  assert.equal(r.statementClockMicros(env), 1000n); // first read → 1000, cached
  assert.equal(r.clockNowMicros(env), 1001n); // per-call read advances the source
  assert.equal(r.clockNowMicros(env), 1002n);
  assert.equal(r.statementClockMicros(env), 1000n); // still the cached statement clock
});
