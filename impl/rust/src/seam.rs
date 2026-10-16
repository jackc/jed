// The entropy + clock sources (spec/design/entropy.md, spec/design/environment.md §4) — the two
// outside inputs that feed the volatile UUID generators (uuidv4/uuidv7) and the clock functions:
//
//   - a RANDOM SOURCE — fills N bytes; the default is the OS CSPRNG, drawn PER VALUE (so production
//     UUIDs are unpredictable, not derived from a single seeded PRNG).
//   - a CLOCK SOURCE  — returns micros since the Unix epoch; the default is the wall clock.
//
// A host installs its sources in an `Environment` (crate::environment); the environment fixes WHICH
// source a database uses, never the source's internal state. The provided controllable sources below
// (a reseedable splitmix64 stream, a manual clock, an advancing clock) are what a host installs for
// reproducibility and what the conformance harness drives for its `# seed:` / `# clock:` /
// `# clock_advance:` directives, which is what makes the generators byte-identical across cores. The
// engine itself contains NO production PRNG — splitmix64 lives here only as the provided
// DETERMINISTIC source a caller may opt into; it is never the default.
//
// Sources are SHARED objects: one environment may serve many databases and sessions at once, on
// many threads. So every source is `Send + Sync` and keeps its state behind a `Mutex` or an atomic.

use crate::environment::Environment;
use crate::error::{EngineError, Result, SqlState};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

/// A host random source: fills `buf` with `buf.len()` random bytes. Shared across every database and
/// session using its environment, so it takes `&self` and must be `Send + Sync`.
pub trait RandomSource: Send + Sync {
    fn fill(&self, buf: &mut [u8]) -> Result<()>;
}

/// A host clock source: returns micros since the Unix epoch. Shared like [`RandomSource`].
pub trait ClockSource: Send + Sync {
    fn now_micros(&self) -> i64;
}

/// The OS CSPRNG (the default random source): one draw of OS entropy per fill (the approved
/// `getrandom` dependency — CLAUDE.md §14; Rust `std` has no OS RNG).
struct OsRandom;

impl RandomSource for OsRandom {
    fn fill(&self, buf: &mut [u8]) -> Result<()> {
        getrandom::fill(buf)
            .map_err(|_| EngineError::new(SqlState::IoError, "OS entropy source unavailable"))
    }
}

/// The default random source: the OS CSPRNG, drawn per value (production — unpredictable output).
pub fn os_random_source() -> Arc<dyn RandomSource> {
    Arc::new(OsRandom)
}

/// The wall clock (the default clock source).
struct WallClock;

impl ClockSource for WallClock {
    fn now_micros(&self) -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_micros() as i64,
            Err(e) => -(e.duration().as_micros() as i64),
        }
    }
}

/// The default clock source: the wall clock.
pub fn wall_clock() -> Arc<dyn ClockSource> {
    Arc::new(WallClock)
}

// splitmix64 constants (entropy.md §2; identical to the bench PRNG, re-authored as engine data).
const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
const MIX1: u64 = 0xBF58_476D_1CE4_E5B9;
const MIX2: u64 = 0x94D0_49BB_1331_11EB;

/// The provided **deterministic** random source: a splitmix64 stream seeded with a `u64`, serialized
/// big-endian in 8-byte chunks (a final partial chunk takes the high bytes of one more draw — never
/// hit by the 16-/8-byte uuid fills). This is what a host installs for reproducibility and what the
/// conformance harness drives for the `# seed:` directive; it is byte-pinned in
/// spec/encoding/prng.toml and asserted cross-core (entropy.md §2). Not the production default.
pub struct SeededRandom {
    state: Mutex<u64>,
}

impl SeededRandom {
    /// A stream seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        SeededRandom {
            state: Mutex::new(seed),
        }
    }

    /// Restart the stream exactly as a fresh source seeded with `seed` would begin.
    pub fn reseed(&self, seed: u64) {
        *self.state.lock().expect("seeded random lock not poisoned") = seed;
    }
}

impl RandomSource for SeededRandom {
    fn fill(&self, buf: &mut [u8]) -> Result<()> {
        let mut state = self.state.lock().expect("seeded random lock not poisoned");
        let mut i = 0;
        while i < buf.len() {
            *state = state.wrapping_add(GAMMA);
            let mut x = *state;
            x = (x ^ (x >> 30)).wrapping_mul(MIX1);
            x = (x ^ (x >> 27)).wrapping_mul(MIX2);
            let bytes = (x ^ (x >> 31)).to_be_bytes();
            let n = (buf.len() - i).min(8);
            buf[i..i + n].copy_from_slice(&bytes[..n]);
            i += n;
        }
        Ok(())
    }
}

/// A shared [`SeededRandom`] seeded with `seed`, ready to install in an environment while the caller
/// keeps the `Arc` to reseed it.
pub fn seeded_random_source(seed: u64) -> Arc<SeededRandom> {
    Arc::new(SeededRandom::new(seed))
}

/// The provided **manual** clock source: returns the instant it was last set to, until the host sets
/// or advances it. The `# clock:` directive sets one (entropy.md §6); a host wanting a frozen instant
/// installs one too.
pub struct ManualClock {
    micros: AtomicI64,
}

impl ManualClock {
    /// A clock reading `micros`.
    pub fn new(micros: i64) -> Self {
        ManualClock {
            micros: AtomicI64::new(micros),
        }
    }

    /// Set the reading to `micros`.
    pub fn set(&self, micros: i64) {
        self.micros.store(micros, Ordering::SeqCst);
    }

    /// Move the reading forward by `step` micros (wrapping, like the advancing clock).
    pub fn advance(&self, step: i64) {
        self.micros.fetch_add(step, Ordering::SeqCst); // atomic add wraps on overflow
    }
}

impl ClockSource for ManualClock {
    fn now_micros(&self) -> i64 {
        self.micros.load(Ordering::SeqCst)
    }
}

/// A shared [`ManualClock`] reading `micros`, ready to install while the caller keeps the `Arc`.
pub fn manual_clock(micros: i64) -> Arc<ManualClock> {
    Arc::new(ManualClock::new(micros))
}

/// The provided **advancing** clock source: returns `start`, then `start+step`, `start+2·step`, …
/// — one increment per read. The `# clock_advance:` directive drives one (entropy.md §6) to make
/// `clock_timestamp()`'s per-call reads deterministic and distinguishable from the statement-stable
/// `now()` cross-core; the draw order follows expression-evaluation order.
pub struct AdvancingClock {
    /// `(next reading, step)`.
    state: Mutex<(i64, i64)>,
}

impl AdvancingClock {
    /// A clock whose next read returns `start`, each later read `step` more.
    pub fn new(start: i64, step: i64) -> Self {
        AdvancingClock {
            state: Mutex::new((start, step)),
        }
    }

    /// Restart the sequence: the next read returns `start`, each later read `step` more.
    pub fn reset(&self, start: i64, step: i64) {
        *self
            .state
            .lock()
            .expect("advancing clock lock not poisoned") = (start, step);
    }
}

impl ClockSource for AdvancingClock {
    fn now_micros(&self) -> i64 {
        let mut state = self
            .state
            .lock()
            .expect("advancing clock lock not poisoned");
        let v = state.0;
        state.0 = v.wrapping_add(state.1);
        v
    }
}

/// A shared [`AdvancingClock`], ready to install while the caller keeps the `Arc` to reset it.
pub fn advancing_clock(start: i64, step: i64) -> Arc<AdvancingClock> {
    Arc::new(AdvancingClock::new(start, step))
}

/// The per-statement mutable entropy/clock state: the uuidv7 monotonic counter and the once-resolved
/// statement clock (entropy.md §5 — read once, reused, so a statement's time cannot vary
/// row-to-row). The PRNG state itself lives in the environment's [`RandomSource`], not here. `Copy` so it can live in a `Cell<StmtRng>` on the `&`-shared `EvalEnv` (interior
/// mutability — the draw order is fixed by eval order regardless).
#[derive(Clone, Copy, Default)]
pub struct StmtRng {
    counter: u32,
    clock: i64,
    clock_resolved: bool,
}

impl StmtRng {
    pub fn new() -> Self {
        Self::default()
    }

    /// The statement clock in micros since the Unix epoch, resolved once (entropy.md §5): the
    /// environment's clock source. Reused for every uuidv7 / now() in the statement (STABLE).
    pub fn statement_clock_micros(&mut self, env: &Environment) -> i64 {
        if !self.clock_resolved {
            self.clock = env.now_micros();
            self.clock_resolved = true;
        }
        self.clock
    }

    /// A fresh read of the environment's clock in micros since the Unix epoch — used by clock_timestamp()
    /// (entropy.md §5), which reads on EVERY call (VOLATILE) and so does NOT touch the once-resolved
    /// statement clock above. No `&mut self`: it caches nothing.
    pub fn clock_now_micros(&self, env: &Environment) -> i64 {
        env.now_micros()
    }

    /// uuidv4 — 16 bytes from the environment's random source, version/variant overwritten (entropy.md §3).
    pub fn uuid_v4(&mut self, env: &Environment) -> Result<[u8; 16]> {
        let mut b = [0u8; 16];
        env.fill(&mut b)?;
        Ok(crate::uuid::build_v4(b))
    }

    /// uuidv7 — the 48-bit ms of `shifted_micros` (the statement clock, possibly interval-shifted by
    /// the caller), a per-statement monotonic counter in rand_a, and 62 random bits (8 bytes from
    /// the environment's random source) in rand_b (entropy.md §3). An out-of-48-bit ms traps `22008`.
    pub fn uuid_v7(&mut self, env: &Environment, shifted_micros: i64) -> Result<[u8; 16]> {
        let unix_ms = shifted_micros.div_euclid(1000);
        if !(0..(1i64 << 48)).contains(&unix_ms) {
            return Err(EngineError::new(
                SqlState::DatetimeFieldOverflow,
                "uuidv7 timestamp out of range",
            ));
        }
        let counter = (self.counter & 0x0FFF) as u16;
        self.counter = self.counter.wrapping_add(1);
        let mut rand_b = [0u8; 8];
        env.fill(&mut rand_b)?;
        Ok(crate::uuid::build_v7(unix_ms as u64, counter, rand_b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uuid::{extract_timestamp_micros, extract_version};
    use crate::value::parse_uuid;

    /// An environment with the provided deterministic random source (seed) + a manual clock — the
    /// test path.
    fn seeded(seed: u64, clock: i64) -> Environment {
        Environment::builder()
            .random_source(seeded_random_source(seed))
            .clock_source(manual_clock(clock))
            .build()
    }

    #[test]
    fn splitmix64_matches_pinned_vectors() {
        // The provided seeded source must fill bytes from the spec'd byte-exact stream
        // (spec/design/entropy.md §2). seed 1 → 910a2dec89025cc1, beeb8da1658eec67, f893a2eefb32555e.
        let src = SeededRandom::new(1);
        let mut buf = [0u8; 24];
        src.fill(&mut buf).unwrap();
        assert_eq!(&buf[0..8], &0x910a_2dec_8902_5cc1u64.to_be_bytes());
        assert_eq!(&buf[8..16], &0xbeeb_8da1_658e_ec67u64.to_be_bytes());
        assert_eq!(&buf[16..24], &0xf893_a2ee_fb32_555eu64.to_be_bytes());
    }

    #[test]
    fn reseed_restarts_the_stream() {
        // Reseeding restarts the stream exactly as a fresh source with that seed.
        let src = SeededRandom::new(7);
        let mut first = [0u8; 16];
        src.fill(&mut first).unwrap();
        let mut skipped = [0u8; 8];
        src.fill(&mut skipped).unwrap();
        src.reseed(7);
        let mut again = [0u8; 16];
        src.fill(&mut again).unwrap();
        assert_eq!(first, again);
        src.reseed(1);
        let mut one = [0u8; 8];
        src.fill(&mut one).unwrap();
        assert_eq!(one, 0x910a_2dec_8902_5cc1u64.to_be_bytes());
    }

    #[test]
    fn uuid_v4_is_deterministic_and_well_formed() {
        let env = seeded(1, 0);
        let mut r = StmtRng::new();
        let b = r.uuid_v4(&env).unwrap();
        // Two splitmix64 draws (910a2dec89025cc1, beeb8da1658eec67), version 4 + RFC variant.
        assert_eq!(
            b,
            parse_uuid("910a2dec-8902-4cc1-beeb-8da1658eec67").unwrap()
        );
        assert_eq!(extract_version(&b), Some(4));
        assert_eq!(b[8] & 0xC0, 0x80); // RFC variant
    }

    #[test]
    fn uuid_v7_embeds_clock_and_is_monotonic() {
        let clock = 1_721_056_591_872_000_i64; // micros; ms = 1721056591872
        let env = seeded(42, clock);
        let mut r = StmtRng::new();
        let a = r.uuid_v7(&env, clock).unwrap();
        let b = r.uuid_v7(&env, clock).unwrap();
        // Version 7, RFC variant, and the embedded ms round-trips through the extractor.
        assert_eq!(extract_version(&a), Some(7));
        assert_eq!(extract_timestamp_micros(&a), Some(1_721_056_591_872_000));
        // Same statement clock → monotonic by the per-statement counter (a < b), uuidv7's reason
        // to exist. Distinct values.
        assert!(
            a < b,
            "uuidv7 must be monotonic within a statement-millisecond"
        );
        assert_ne!(a, b);
    }

    #[test]
    fn default_environment_uses_os_entropy_and_wall_clock() {
        // The PRODUCTION path: the default environment's OS entropy (getrandom) per draw + the wall
        // clock. Assert only STRUCTURAL invariants (always true), so the test outcome is
        // deterministic while still exercising getrandom + SystemTime at runtime.
        let env = Environment::default();
        let mut r = StmtRng::new();
        let v4 = r.uuid_v4(&env).unwrap();
        assert_eq!(extract_version(&v4), Some(4));
        assert_eq!(v4[8] & 0xC0, 0x80); // RFC variant
        let clk = r.statement_clock_micros(&env);
        let v7 = r.uuid_v7(&env, clk).unwrap();
        assert_eq!(extract_version(&v7), Some(7));
        // The embedded instant is a plausible wall-clock time (after 2020-01-01).
        assert!(extract_timestamp_micros(&v7).unwrap() > 1_577_836_800_000_000);
    }

    #[test]
    fn manual_clock_sets_and_advances() {
        let clk = ManualClock::new(5);
        assert_eq!(clk.now_micros(), 5);
        assert_eq!(clk.now_micros(), 5); // a read never moves it
        clk.advance(10);
        assert_eq!(clk.now_micros(), 15);
        clk.set(-3);
        assert_eq!(clk.now_micros(), -3);
    }

    #[test]
    fn advancing_clock_steps_per_read_and_now_caches() {
        // The advancing clock yields start, start+step, … one increment per read (entropy.md §6).
        let clk = AdvancingClock::new(1_000, 1);
        assert_eq!(clk.now_micros(), 1_000);
        assert_eq!(clk.now_micros(), 1_001);
        assert_eq!(clk.now_micros(), 1_002);
        clk.reset(50, 5);
        assert_eq!(clk.now_micros(), 50);
        assert_eq!(clk.now_micros(), 55);

        // now() (statement_clock_micros) reads ONCE and caches: it pulls 1000, then stays 1000 even
        // as clock_timestamp() (clock_now_micros) keeps advancing the SAME source. This is what
        // makes the now()-stable vs clock_timestamp()-volatile distinction deterministic.
        let env = Environment::builder()
            .clock_source(advancing_clock(1_000, 1))
            .build();
        let mut r = StmtRng::new();
        assert_eq!(r.statement_clock_micros(&env), 1_000); // first read → 1000, cached
        assert_eq!(r.clock_now_micros(&env), 1_001); // per-call read advances the source
        assert_eq!(r.clock_now_micros(&env), 1_002);
        assert_eq!(r.statement_clock_micros(&env), 1_000); // still the cached statement clock
    }

    #[test]
    fn uuid_v7_rejects_out_of_range_ms() {
        let env = seeded(1, 0);
        let mut r = StmtRng::new();
        // A pre-epoch (negative) instant has no RFC 9562 v7 representation → 22008.
        assert!(r.uuid_v7(&env, -1).is_err());
        // Beyond 2^48 ms also traps.
        assert!(r.uuid_v7(&env, (1i64 << 48) * 1000).is_err());
    }
}
