# Entropy + clock seam — design

> The host-injected **random and clock seams** — two functions the host supplies (each defaulting
> to the platform primitive) — and the **spec'd byte-exact PRNG** the engine provides as an
> injectable *deterministic* source. Together they let nondeterministic functions (`uuidv4`,
> `uuidv7`, `now()`/`current_timestamp`, `clock_timestamp()`, and later `random()`) live inside jed's
> cross-core determinism contract. This RATIFIES [determinism.md](determinism.md) §5 (class **B**)
> for the UUID generators and the clock functions. Read determinism.md §1/§5 first — this doc is the
> concrete realization of the seam it proposes.

## 1. The problem and the move

`uuidv4()` is random and `uuidv7()` is wall-clock + random — both are nondeterministic, which
collides with the project's spine: with no reference implementation, the only thing that says two
cores agree is byte-identical output on shared tests ([CLAUDE.md](../../CLAUDE.md) §2). The move
(determinism.md §5) is **not** to make the engine nondeterministic, but to push the two
nondeterministic inputs to **host-injected functions** behind seams — exactly like the storage and
cost seams (CLAUDE.md §9):

- **Random source** — a host function that fills N bytes. The **default is the OS CSPRNG, drawn
  per value** (Go `crypto/rand`, TS `node:crypto`, Rust `getrandom`), so production UUIDs are
  **unpredictable** — they are *not* derived from a single seeded PRNG that an observer could
  reconstruct from one output. A host may inject its own function for reproducibility.
- **Clock source** — a host function returning micros since the Unix epoch. The default is the wall
  clock (Rust `SystemTime`, Go `time.Now`, TS `Date`). A host may inject a fixed/controllable clock.

The engine contains **no production PRNG**. For reproducibility the engine *provides* a spec'd
deterministic source (`seeded_random_source`, §2) the host can inject; the conformance harness
injects exactly that plus a fixed clock. The result: given the same `(query, db, injected random
source, injected clock)`, every core emits **byte-identical** UUIDs (G1+G2 preserved), so the
corpus tests the generators with **exact** assertions under injected seam functions — no harness
exception, no property-only check. The only irreducible production nondeterminism is the raw
CSPRNG bytes and the raw clock read; everything downstream is in-contract.

**Stability scope (determinism.md §5).** The clock is read **once per statement** and reused for
every row (PG's `now()` semantics). The random source is asked for fresh bytes per generated value,
in row-evaluation order, so distinct rows get distinct values.

**Other clock-seam consumers — `now()` / `current_timestamp` / `clock_timestamp()`.** The clock
seam also feeds the current-time functions ([functions.md](functions.md) §12), all returning
`timestamptz` (the seam's micros are exactly timestamptz's internal representation). `now()` (and its
bare-keyword sugar `current_timestamp`) is **STABLE**: it reads the once-resolved **statement clock**
above and reuses it for every row. `clock_timestamp()` is **VOLATILE**: it reads the seam on **every**
call (a fresh read that does *not* touch the statement-clock cache), so it may advance within a
statement — the draws follow expression-evaluation order, exactly like the random source's. They take
no entropy. The **clock-relative date literals** (`'today'`/`'now'`/`'tomorrow'`/`'yesterday'` —
[date.md §6](date.md)) are a further STABLE consumer: the same once-resolved statement clock,
decomposed into a day in the session zone — as are `CURRENT_DATE` and the one-argument `age(x)`,
which measures from that day's midnight ([interval.md §5.2](interval.md)).

## 2. The provided deterministic source — splitmix64, byte-exact shared data

For reproducibility (tests, and any host that wants it) the engine provides a **deterministic**
random source built on **splitmix64** (the same algorithm the bench harness pins in
[benchmarks.md](benchmarks.md), re-authored as engine shared data — the bench module is not a core,
CLAUDE.md §14). It is **not** the production default; it is what `seeded_random_source(seed)` returns
and what the `# seed:` directive injects. State is one `u64`; one step, all arithmetic wrapping to
64 bits:

```
next():
  state += 0x9E3779B97F4A7C15        # mod 2^64
  x = state
  x = (x ^ (x >> 30)) * 0xBF58476D1CE4E5B9   # mod 2^64
  x = (x ^ (x >> 27)) * 0x94D049BB133111EB   # mod 2^64
  return x ^ (x >> 31)
```

The state is initialized **directly from the seed** (`state = seed`); the first `next()` adds the
gamma. TypeScript implements this over `BigInt` masked to 64 bits (JS numbers are f64 — the same
discipline as the cost counter and the bench PRNG). Pinned engine vectors live in
[../encoding/prng.toml](../encoding/prng.toml), verified by Ruby (the independent third voice,
CLAUDE.md §8) and asserted in each core's unit tests.

**The fill contract.** A random source fills a byte buffer of the exact length the engine requests
(16 bytes for v4, 8 for v7's `rand_b` — §3). The provided seeded source fills in **8-byte
big-endian chunks**: each chunk is one `next()` serialized big-endian (most significant first — the
engine's on-disk convention everywhere); a final partial chunk (< 8 bytes, never hit by the uuid
fills) takes the high bytes of one more draw. **The fill length, the draw count, and the
draw→byte mapping are part of the contract** — pinned in prng.toml and asserted in the corpus under
an injected seed; a one-byte or one-draw disagreement is a silent G2 break.

## 3. UUID byte layout (RFC 9562)

16 bytes, big-endian field order; byte 0 is the most-significant. The version nibble is the high
nibble of byte 6; the variant bits are the top two bits of byte 8. The random bytes come from the
seam's random source via the fill contract (§2); under the provided seeded source they are the
splitmix64 stream serialized big-endian.

### uuidv4 — 122 random bits (16 bytes filled)
`gen_random_uuid()` is PostgreSQL's older name for the same function: it resolves to the same
kernel, so it draws the same bytes from the source and, under one seed, returns the same value.

1. Fill `b[0..16]` from the random source. (Under the seeded source: draw 1 → bytes 0..7, draw 2 → bytes 8..15.)
2. `b[6] = (b[6] & 0x0F) | 0x40`  — version 4.
3. `b[8] = (b[8] & 0x3F) | 0x80`  — RFC 4122 variant.

### uuidv7 — 48-bit ms timestamp + a monotonic counter + 62 random bits (8 bytes filled)
RFC 9562 §5.7 layout with the **Method 1 (fixed-length dedicated counter)** monotonicity (§6.2):
`rand_a` (12 bits) is a per-statement monotonic counter, `rand_b` (62 bits) is random.

1. `unix_ms = floor(stmt_clock_micros_after_shift / 1000)`. Range-check `0 ≤ unix_ms < 2^48`, else
   **`22008` datetime_field_overflow** (a negative/pre-epoch or far-future instant). The shift (the
   optional `interval` arg) is applied first — §4.
2. `b[0..6]` = the 48-bit big-endian `unix_ms` (`b[0]=(ms>>40)&0xFF … b[5]=ms&0xFF`).
3. `counter = rng.counter; rng.counter += 1` — the per-statement counter (starts at 0, no draw).
   `rand_a = counter & 0x0FFF` (12 bits; >4096 uuidv7 in one statement-millisecond wraps — not
   monotonic past that, a documented bound).
4. `b[6] = 0x70 | ((rand_a >> 8) & 0x0F)`  — version 7 + the high nibble of `rand_a`.
   `b[7] = rand_a & 0xFF`                  — the low byte of `rand_a`.
5. Fill `b[8..16]` from the random source (8 bytes); then
   `b[8] = (b[8] & 0x3F) | 0x80`           — RFC 4122 variant (overwrites the top 2 bits).

**Monotonicity.** With a statement-stable clock, every uuidv7 in a statement shares `b[0..6]`; the
counter in `b[6..8]` strictly increases per call, so the values sort in generation order (uuidv7's
reason to exist). The random `rand_b` keeps them distinct across statements/seeds even at equal
counters. This is deterministic under an injected random source + clock and cross-core identical.

### The extractors (the pure half, already landed — [functions.md](functions.md) §12)
`uuid_extract_version` / `uuid_extract_timestamp` are the inverse reads, immutable, no seam.

## 4. The `uuidv7(shift interval)` overload

PostgreSQL offsets the embedded timestamp by an `interval` before generating (its
`timestamptz_pl_interval`, in the session zone). jed reuses its existing **calendar-aware**
`timestamptz + interval` arithmetic (`interval::ts_shift_zoned` with the session zone,
[interval.md](interval.md) §5) — months land on calendar months and days on calendar days of the
session zone's wall clock — so the shift is PG-faithful and there is no second interval model:
`shifted = ts_shift_zoned(stmt_clock_micros, interval, session_zone)`, then
`unix_ms = floor(shifted / 1000)` (§3), charging one `timezone` unit per calendar step like the
operator. Overflow in the add traps the same `22008` the shift arithmetic already raises. A NULL
interval propagates (the catalog `null = "propagates"` — the call returns NULL).

## 5. The seam in the cores

The **random and clock sources** are installed in the handle's **environment**
([environment.md](environment.md) §4) — the default environment carries the OS random source and
the wall clock. The evaluator reaches them through the session's environment from the per-statement
`EvalEnv`. A small per-statement `StmtRng` carries the uuidv7 monotonic **counter** and the
once-resolved **clock** (read once — §1 stability); it is created where the cost `Meter` is. Rust
holds it as `Cell<StmtRng>` borrowed by `EvalEnv` (interior mutability — the seam state advances and
`EvalEnv` is `&`-shared; the draw order is fixed by eval order regardless); Go a `*StmtRng`; TS a
`StmtRng` object. **No syscall on the hot path**: a query with no generator never calls the
random/clock source.

A source is an **object** the environment shares by reference. A *deterministic* source keeps its
PRNG state inside itself (**environment-scoped**, advancing across every statement of every database
and session that uses the environment), not in `StmtRng`; Rust and Go sources are thread-safe because
sessions may run concurrently. The production OS source is stateless from the engine's view (each fill
is fresh OS entropy).

**Per-statement, not per-block (a documented bound).** The `StmtRng` is created per query-block
execution (where `EvalEnv` is built), so the monotonic counter and cached clock reset per block. A
generator inside a **correlated subquery** or a **set-operation arm** therefore resets its counter
per re-execution: the output stays deterministic and cross-core identical, but the counter ordering
restarts. The long-term refinement is the counter-keyed RNG of determinism.md §7 (`prng(seed,
row_key)`), deferred with parallelism; until then a generator's natural home is a single query block
(projection / `INSERT … SELECT` / a `generate_series` feed), where it is both deterministic and
ordered.

## 6. Host API — sources in the environment

The random and clock sources are part of the handle's **environment** ([environment.md](environment.md)),
not stored in the file and not per-statement arguments. The host chooses them on the environment
builder; the built environment is immutable, so the *installed* source never changes while a database
is open. A source is a controllable object, so a host (or a test) keeps a reference and steers its
state instead of swapping it:

| Setting | Rust | Go | TS |
|---|---|---|---|
| install random source | `builder.random_source(Arc<dyn RandomSource>)` | `b.SetRandomSource(RandomSource)` | `builder.randomSource(s)` |
| install clock source | `builder.clock_source(Arc<dyn ClockSource>)` | `b.SetClockSource(ClockSource)` | `builder.clockSource(c)` |
| source interface | `trait RandomSource { fn fill(&self, &mut [u8]) -> Result<()> }`, `trait ClockSource { fn now_micros(&self) -> i64 }` | `RandomSource { Fill([]byte) error }`, `ClockSource { NowMicros() int64 }` | `RandomSource { fill(Uint8Array) }`, `ClockSource { nowMicros(): bigint }` |

The engine provides these sources:

| Provided source | Rust | Go | TS | Control |
|---|---|---|---|---|
| OS random (default) | `os_random_source()` | `OSRandomSource()` | `osRandomSource()` | none |
| wall clock (default) | `wall_clock()` | `WallClock()` | `wallClock()` | none |
| seeded PRNG | `seeded_random_source(u64)` → `SeededRandom` | `NewSeededRandom(uint64)` | `seededRandomSource(bigint)` → `SeededRandom` | `reseed(seed)` restarts the stream |
| manual clock | `manual_clock(i64)` → `ManualClock` | `NewManualClock(int64)` | `manualClock(bigint)` → `ManualClock` | `set(micros)`, `advance(step)` |
| advancing clock | `advancing_clock(start, step)` → `AdvancingClock` | `NewAdvancingClock(start, step)` | `advancingClock(start, step)` → `AdvancingClock` | `reset(start, step)` |

The **manual clock** replaces the former `fixed_clock`: it returns the instant it was last set to
(or advanced to) on every read. The **advancing clock** returns `start`, then `start+step`,
`start+2·step`, … — one increment per read. It makes `clock_timestamp()`'s per-call reads
deterministic and distinguishable from the statement-stable `now()` (which reads the source only
once): under it, two `now()` in a statement are equal while two `clock_timestamp()` differ. The
conformance harness drives it via the **`# clock_advance: start,step`** directive
([conformance.md](conformance.md) §4), the advancing counterpart to `# clock:`.

By default the random source reads **OS entropy per value** and the clock reads the **wall clock**:
Go `crypto/rand` + `time`, TS `node:crypto` + `Date`, Rust `getrandom` (the approved CLAUDE.md §14
dependency — Rust `std` has no OS RNG; Go/TS use their stdlib) + `SystemTime`. The conformance
harness builds its database's environment with its own switchable sources, which default to OS
entropy and the wall clock and, for a record carrying `# seed:` / `# clock:` / `# clock_advance:`,
draw from a reseeded `SeededRandom` / a set `ManualClock` / a reset `AdvancingClock` for that record
and switch back after — exactly the `# max_cost:` pattern ([conformance.md](conformance.md) §4).

## 7. Cost

A generator call charges the uniform **one `operator_eval`** (like every scalar function),
**independent of the random values and the draw count** — so cost stays deterministic and identical
regardless of source/clock (asserted in the corpus by pinning the same `# cost:` under different
`# seed:` values). The `uuidv7(interval)` argument charges its own `operator_eval`(s). `now()` /
`current_timestamp` / `clock_timestamp()` likewise each charge one `operator_eval`, independent of
the clock value.

## 8. Determinism ledger

The generators are class-**B** entries in
[../conformance/determinism_exceptions.toml](../conformance/determinism_exceptions.toml): with the
provided seeded source + fixed clock injected (the test path) they keep **G1+G2** (drop nothing —
exact cross-core); production drops G1/G2 only on the raw per-value OS-entropy fills and the raw
clock read. The blast radius is the uuid result column (and any row multiset it gates —
determinism.md §4); the test mechanism is injected-source+clock-exact plus the prng.toml vectors.
The uuidv7 monotonic-counter behavior is itself deterministic — it is **not** a determinism
exception (and is not oracle-comparable to PG's own counter, since neither core byte-matches PG's
entropy; the generators are not oracle-imported).

The clock functions are the two further class-**B** entries `now-clock` (STABLE) and
`clock-timestamp-clock` (VOLATILE, a distinct entry — per-call, not statement-stable): with a fixed
or advancing clock injected they keep **G1+G2**; production drops G1/G2 only on the raw wall-clock
read(s). They are likewise **not** oracle-imported (PG's wall clock differs).
