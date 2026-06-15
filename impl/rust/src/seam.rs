// The entropy + clock seam (spec/design/entropy.md) — two host-injectable functions that feed the
// volatile UUID generators (uuidv4/uuidv7), each defaulting to the platform primitive:
//
//   - the RANDOM SOURCE  — fills N bytes; default = the OS CSPRNG, drawn PER VALUE (so production
//     UUIDs are unpredictable, not derived from a single seeded PRNG).
//   - the CLOCK SOURCE   — returns micros since the Unix epoch; default = the wall clock.
//
// A host injects its own functions for reproducibility (e.g. a controllable clock, or the provided
// `seeded_random_source` below). The conformance harness injects exactly those via the `# seed:` /
// `# clock:` directives, which is what makes the generators byte-identical across cores. The engine
// itself contains NO production PRNG — splitmix64 lives here only as the provided DETERMINISTIC
// source a caller may opt into; it is never the default.

use crate::error::{EngineError, Result, SqlState};
use std::cell::RefCell;

/// A host random source: fills `buf` with `buf.len()` random bytes. `+ Send` keeps [`Database`]
/// `Send` (the shared read/write handles move across threads — `crate::shared`). `FnMut` because a
/// deterministic source (e.g. [`seeded_random_source`]) advances its own captured state per call.
pub type RandomSource = Box<dyn FnMut(&mut [u8]) + Send>;

/// A host clock source: returns micros since the Unix epoch. `FnMut + Send` for the same reasons as
/// [`RandomSource`] (a host may inject an advancing/simulated clock).
pub type ClockSource = Box<dyn FnMut() -> i64 + Send>;

/// The host seam carried on the [`Database`](crate::executor::Database) handle (spec/design/api.md
/// §10): the injected random + clock functions, each `None` ⇒ the platform default. Behind
/// `RefCell` so the `FnMut`s can be called through the `&`-shared handle (the evaluator reaches the
/// seam via `&Database`; the draw order is fixed by eval order regardless). Only the volatile uuid
/// generators touch it; every other expression ignores it.
#[derive(Default)]
pub struct Seam {
    random: RefCell<Option<RandomSource>>,
    clock: RefCell<Option<ClockSource>>,
}

impl Seam {
    /// Inject a random source (the deterministic / reproducible path). Replaces any previous one.
    pub(crate) fn set_random(&self, f: RandomSource) {
        *self.random.borrow_mut() = Some(f);
    }

    /// Drop the injected random source: fills fall back to the OS CSPRNG (production).
    pub(crate) fn clear_random(&self) {
        *self.random.borrow_mut() = None;
    }

    /// Inject a clock source. Replaces any previous one.
    pub(crate) fn set_clock(&self, f: ClockSource) {
        *self.clock.borrow_mut() = Some(f);
    }

    /// Drop the injected clock source: `uuidv7` falls back to the wall clock (production).
    pub(crate) fn clear_clock(&self) {
        *self.clock.borrow_mut() = None;
    }

    /// Fill `buf` with random bytes: the injected source, else one draw of OS entropy per byte span
    /// (the approved `getrandom` dependency — CLAUDE.md §14; Rust `std` has no OS RNG). Drawn per
    /// value, so production output is unpredictable.
    fn fill(&self, buf: &mut [u8]) -> Result<()> {
        match self.random.borrow_mut().as_mut() {
            Some(f) => {
                f(buf);
                Ok(())
            }
            None => getrandom::fill(buf)
                .map_err(|_| EngineError::new(SqlState::IoError, "OS entropy source unavailable")),
        }
    }

    /// The current time in micros since the Unix epoch: the injected clock, else the wall clock.
    fn now_micros(&self) -> i64 {
        match self.clock.borrow_mut().as_mut() {
            Some(f) => f(),
            None => wall_clock_micros(),
        }
    }
}

// splitmix64 constants (entropy.md §2; identical to the bench PRNG, re-authored as engine data).
const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
const MIX1: u64 = 0xBF58_476D_1CE4_E5B9;
const MIX2: u64 = 0x94D0_49BB_1331_11EB;

/// The provided **deterministic** random source: a splitmix64 stream seeded with `seed`, serialized
/// big-endian in 8-byte chunks (a final partial chunk takes the high bytes of one more draw — never
/// hit by the 16-/8-byte uuid fills). This is what a host injects for reproducibility and what the
/// conformance harness injects for the `# seed:` directive; it is byte-pinned in
/// spec/encoding/prng.toml and asserted cross-core (entropy.md §2). Not the production default.
pub fn seeded_random_source(seed: u64) -> RandomSource {
    let mut state = seed;
    Box::new(move |buf: &mut [u8]| {
        let mut i = 0;
        while i < buf.len() {
            state = state.wrapping_add(GAMMA);
            let mut x = state;
            x = (x ^ (x >> 30)).wrapping_mul(MIX1);
            x = (x ^ (x >> 27)).wrapping_mul(MIX2);
            let bytes = (x ^ (x >> 31)).to_be_bytes();
            let n = (buf.len() - i).min(8);
            buf[i..i + n].copy_from_slice(&bytes[..n]);
            i += n;
        }
    })
}

/// The provided **fixed** clock source: always returns `micros`. The `# clock:` directive injects
/// this (entropy.md §6); a host wanting a frozen instant uses it too.
pub fn fixed_clock(micros: i64) -> ClockSource {
    Box::new(move || micros)
}

/// The provided **advancing** clock source: returns `start`, then `start+step`, `start+2·step`, …
/// — one increment per read (`FnMut` captured state). The `# clock_advance:` directive injects this
/// (entropy.md §6) to make `clock_timestamp()`'s per-call reads deterministic and distinguishable
/// from the statement-stable `now()` cross-core; the draw order follows expression-evaluation order.
pub fn advancing_clock(start: i64, step: i64) -> ClockSource {
    let mut cur = start;
    Box::new(move || {
        let v = cur;
        cur = cur.wrapping_add(step);
        v
    })
}

/// The per-statement mutable seam state: the uuidv7 monotonic counter and the once-resolved
/// statement clock (entropy.md §5 — read once, reused, so a statement's time cannot vary
/// row-to-row). The PRNG state itself lives in the injected [`RandomSource`] (handle-scoped), not
/// here. `Copy` so it can live in a `Cell<StmtRng>` on the `&`-shared `EvalEnv` (interior
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

    /// The statement clock in micros since the Unix epoch, resolved once (entropy.md §5): the seam's
    /// clock source. Reused for every uuidv7 / now() in the statement (STABLE).
    pub fn statement_clock_micros(&mut self, seam: &Seam) -> i64 {
        if !self.clock_resolved {
            self.clock = seam.now_micros();
            self.clock_resolved = true;
        }
        self.clock
    }

    /// A fresh read of the clock seam in micros since the Unix epoch — used by clock_timestamp()
    /// (entropy.md §5), which reads on EVERY call (VOLATILE) and so does NOT touch the once-resolved
    /// statement clock above. No `&mut self`: it caches nothing.
    pub fn clock_now_micros(&self, seam: &Seam) -> i64 {
        seam.now_micros()
    }

    /// uuidv4 — 16 bytes from the seam's random source, version/variant overwritten (entropy.md §3).
    pub fn uuid_v4(&mut self, seam: &Seam) -> Result<[u8; 16]> {
        let mut b = [0u8; 16];
        seam.fill(&mut b)?;
        Ok(crate::uuid::build_v4(b))
    }

    /// uuidv7 — the 48-bit ms of `shifted_micros` (the statement clock, possibly interval-shifted by
    /// the caller), a per-statement monotonic counter in rand_a, and 62 random bits (8 bytes from
    /// the seam) in rand_b (entropy.md §3). An out-of-48-bit ms traps `22008`.
    pub fn uuid_v7(&mut self, seam: &Seam, shifted_micros: i64) -> Result<[u8; 16]> {
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
        seam.fill(&mut rand_b)?;
        Ok(crate::uuid::build_v7(unix_ms as u64, counter, rand_b))
    }
}

/// The wall clock in micros since the Unix epoch (the production clock path). Only reached when no
/// clock is injected — never on the conformance path, which always injects `# clock:`.
fn wall_clock_micros() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_micros() as i64,
        Err(e) => -(e.duration().as_micros() as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uuid::{extract_timestamp_micros, extract_version};
    use crate::value::parse_uuid;

    /// A seam with the provided deterministic random source (seed) + a fixed clock — the test path.
    fn seeded(seed: u64, clock: i64) -> Seam {
        let s = Seam::default();
        s.set_random(seeded_random_source(seed));
        s.set_clock(fixed_clock(clock));
        s
    }

    #[test]
    fn splitmix64_matches_pinned_vectors() {
        // The provided seeded source must fill bytes from the spec'd byte-exact stream
        // (spec/design/entropy.md §2). seed 1 → 910a2dec89025cc1, beeb8da1658eec67, f893a2eefb32555e.
        let mut src = seeded_random_source(1);
        let mut buf = [0u8; 24];
        src(&mut buf);
        assert_eq!(&buf[0..8], &0x910a_2dec_8902_5cc1u64.to_be_bytes());
        assert_eq!(&buf[8..16], &0xbeeb_8da1_658e_ec67u64.to_be_bytes());
        assert_eq!(&buf[16..24], &0xf893_a2ee_fb32_555eu64.to_be_bytes());
    }

    #[test]
    fn uuid_v4_is_deterministic_and_well_formed() {
        let seam = seeded(1, 0);
        let mut r = StmtRng::new();
        let b = r.uuid_v4(&seam).unwrap();
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
        let seam = seeded(42, clock);
        let mut r = StmtRng::new();
        let a = r.uuid_v7(&seam, clock).unwrap();
        let b = r.uuid_v7(&seam, clock).unwrap();
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
    fn unseeded_path_uses_os_entropy_and_wall_clock() {
        // The PRODUCTION path: no injected source → OS entropy (getrandom) per draw + the wall
        // clock. Assert only STRUCTURAL invariants (always true), so the test outcome is
        // deterministic while still exercising getrandom + SystemTime at runtime.
        let seam = Seam::default();
        let mut r = StmtRng::new();
        let v4 = r.uuid_v4(&seam).unwrap();
        assert_eq!(extract_version(&v4), Some(4));
        assert_eq!(v4[8] & 0xC0, 0x80); // RFC variant
        let clk = r.statement_clock_micros(&seam);
        let v7 = r.uuid_v7(&seam, clk).unwrap();
        assert_eq!(extract_version(&v7), Some(7));
        // The embedded instant is a plausible wall-clock time (after 2020-01-01).
        assert!(extract_timestamp_micros(&v7).unwrap() > 1_577_836_800_000_000);
    }

    #[test]
    fn advancing_clock_steps_per_read_and_now_caches() {
        // The advancing clock yields start, start+step, … one increment per read (entropy.md §6).
        let mut clk = advancing_clock(1_000, 1);
        assert_eq!(clk(), 1_000);
        assert_eq!(clk(), 1_001);
        assert_eq!(clk(), 1_002);

        // now() (statement_clock_micros) reads ONCE and caches: it pulls 1000, then stays 1000 even
        // as clock_timestamp() (clock_now_micros) keeps advancing the SAME source. This is what
        // makes the now()-stable vs clock_timestamp()-volatile distinction deterministic.
        let seam = Seam::default();
        seam.set_clock(advancing_clock(1_000, 1));
        let mut r = StmtRng::new();
        assert_eq!(r.statement_clock_micros(&seam), 1_000); // first read → 1000, cached
        assert_eq!(r.clock_now_micros(&seam), 1_001); // per-call read advances the source
        assert_eq!(r.clock_now_micros(&seam), 1_002);
        assert_eq!(r.statement_clock_micros(&seam), 1_000); // still the cached statement clock
    }

    #[test]
    fn uuid_v7_rejects_out_of_range_ms() {
        let seam = seeded(1, 0);
        let mut r = StmtRng::new();
        // A pre-epoch (negative) instant has no RFC 9562 v7 representation → 22008.
        assert!(r.uuid_v7(&seam, -1).is_err());
        // Beyond 2^48 ms also traps.
        assert!(r.uuid_v7(&seam, (1i64 << 48) * 1000).is_err());
    }
}
