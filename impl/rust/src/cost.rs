//! Deterministic cost meter (CLAUDE.md §13).
//!
//! A `Meter` accrues the execution cost of a query from the shared unit weights in
//! [`crate::costs::COSTS`] (generated from spec/cost/schedule.toml). The cost of a given
//! `(query, database state)` is fully deterministic and **identical across every core**
//! — a CLAUDE.md §8 divergence hotspot, asserted in the conformance corpus. The accrual
//! *sites* (which executor / evaluator / storage line charges which unit) are hand-written
//! here and in `executor.rs`; only the weights are shared data. See spec/design/cost.md.
//!
//! Every unit routes through the single [`Meter::charge`] chokepoint, so the deferred
//! ceiling + deterministic abort (spec/design/cost.md §6) is a local, additive change.

/// Accrues deterministic execution cost. Threaded by `&mut` through the executor and the
/// recursive expression evaluator; the accrued total is reported on `Outcome`.
#[derive(Default)]
pub struct Meter {
    /// Total cost accrued so far (CLAUDE.md §13). `i64` mirrors the engine's native
    /// integer; a future ceiling compares against this same counter.
    pub accrued: i64,
}

impl Meter {
    /// A fresh meter with zero accrued cost.
    pub fn new() -> Self {
        Meter::default()
    }

    /// Charge `units` of cost. The single accrual chokepoint: the deferred enforcement
    /// (a caller ceiling + deterministic abort) becomes a check inside this method, with
    /// no executor call site re-threaded.
    #[inline]
    pub fn charge(&mut self, units: i64) {
        self.accrued += units;
    }
}
