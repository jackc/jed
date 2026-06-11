//! Deterministic cost meter (CLAUDE.md §13).
//!
//! A `Meter` accrues the execution cost of a query from the shared unit weights in
//! [`crate::costs::COSTS`] (generated from spec/cost/schedule.toml). The cost of a given
//! `(query, database state)` is fully deterministic and **identical across every core**
//! — a CLAUDE.md §8 divergence hotspot, asserted in the conformance corpus. The accrual
//! *sites* (which executor / evaluator / storage line charges which unit) are hand-written
//! here and in `executor.rs`; only the weights are shared data. See spec/design/cost.md.
//!
//! Every unit routes through the single [`Meter::charge`] chokepoint; the caller-set
//! ceiling + deterministic abort (spec/design/cost.md §6) is enforced by [`Meter::guard`],
//! consulted at the unbounded-work points (per scanned row, per produced row, per
//! expression node, per aggregate fold, and immediately after each size-scaled
//! `decimal_work` charge — cost.md §3) so a runaway query stops deterministically.

use crate::error::{EngineError, Result, SqlState};

/// Accrues deterministic execution cost and enforces an optional ceiling (CLAUDE.md §13).
/// Threaded by `&mut` through the executor and the recursive expression evaluator; the
/// accrued total is reported on `Outcome`.
#[derive(Default)]
pub struct Meter {
    /// Total cost accrued so far (CLAUDE.md §13). `i64` mirrors the engine's native
    /// integer; the ceiling compares against this same counter.
    pub accrued: i64,
    /// The caller-set cost ceiling, or `0` (the default) for **unlimited**. A positive
    /// value bounds an untrusted query: the instant accrued cost reaches it, the next
    /// [`guard`](Meter::guard) aborts with `54P01` (spec/design/cost.md §6). Carried from
    /// the handle's `max_cost` setting (spec/design/api.md §8).
    pub limit: i64,
}

impl Meter {
    /// A fresh meter with zero accrued cost and no ceiling.
    pub fn new() -> Self {
        Meter::default()
    }

    /// A fresh meter that aborts once accrued cost reaches `limit` (`limit <= 0` ⇒
    /// unlimited). The ceiling is the handle's `max_cost` (spec/design/api.md §8).
    pub fn with_limit(limit: i64) -> Self {
        Meter { accrued: 0, limit }
    }

    /// Charge `units` of cost. The single accrual chokepoint. Enforcement is **not** here:
    /// `charge` only accrues, so the cross-core accrual count (the `# cost:` contract) is
    /// untouched; [`guard`](Meter::guard) does the comparison at the work loops.
    #[inline]
    pub fn charge(&mut self, units: i64) {
        self.accrued += units;
    }

    /// Enforce the ceiling: abort with `54P01` if a ceiling is set and accrued cost has
    /// **reached** it (CLAUDE.md §13 — "the instant accrued cost reaches it, execution
    /// aborts"). Called at the unbounded-work points (the scan loop, the produce step, the
    /// expression evaluator, the aggregate fold) — the same mirrored points in every core,
    /// so the abort is deterministic and cross-core identical (spec/design/cost.md §6).
    /// A no-op (one comparison) when unlimited, so it is free on the hot path by default.
    #[inline]
    pub fn guard(&self) -> Result<()> {
        if self.limit > 0 && self.accrued >= self.limit {
            return Err(EngineError::new(
                SqlState::CostLimitExceeded,
                format!(
                    "query exceeded the cost limit of {} (accrued {})",
                    self.limit, self.accrued
                ),
            ));
        }
        Ok(())
    }
}
