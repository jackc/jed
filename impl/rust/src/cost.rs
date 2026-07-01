//! Deterministic cost meter (CLAUDE.md §13).
//!
//! A `Meter` accrues the execution cost of a query from the shared unit weights in
//! [`crate::costs::COSTS`] (generated from spec/cost/schedule.toml). The cost of a given
//! `(query, database state)` is fully deterministic and **identical across every core**
//! — a CLAUDE.md §8 divergence hotspot, asserted in the conformance corpus. The accrual
//! *sites* (which executor / evaluator / storage line charges which unit) are hand-written
//! here and in `executor.rs`; only the weights are shared data. See spec/design/cost.md.
//!
//! Every unit routes through the single [`Meter::charge`] chokepoint, which enforces **two**
//! independent ceilings ([`Meter::guard`], consulted at the unbounded-work points — per scanned
//! row, per produced row, per expression node, per aggregate fold, and immediately after each
//! size-scaled `decimal_work` charge, cost.md §3):
//!
//! - **Per-statement** `max_cost` → `54P01` (spec/design/cost.md §6): the statement's own accrued
//!   cost reaching the caller-set ceiling.
//! - **Per-session** `lifetime_max_cost` → `54P02` (spec/design/session.md §5.4): the session's
//!   *cumulative* cost reaching the budget. The meter live-charges its units into the session's
//!   cumulative total (a shared [`Rc<Cell<i64>>`]), so an aborted statement's partial cost counts
//!   automatically and the cumulative is session state that survives a transaction rollback.

use std::cell::Cell;
use std::rc::Rc;

use crate::cancel::CancellationToken;
use crate::error::{EngineError, Result, SqlState};

/// The session lifetime-budget handle a [`Meter`] carries (spec/design/session.md §5.4): a shared
/// reference to the session's cumulative cost total plus the budget. The meter charges every unit
/// into `total` (live), so the cumulative is always current — partial cost of an aborted statement
/// is already folded in, with no separate end-of-statement step. `limit <= 0` ⇒ the cumulative is
/// still **tracked** (the gauge stays readable) but **never aborts**.
#[derive(Clone)]
pub struct Lifetime {
    /// The session's running cumulative cost (spec/design/session.md §5.4). Shared with the
    /// [`Session`](crate::Session): the meter live-charges into it, the session reads it back as the
    /// `lifetime_cost()` gauge and checks it at statement admission.
    pub total: Rc<Cell<i64>>,
    /// The session's cumulative cost budget, or `0` for **unlimited** (track-only).
    pub limit: i64,
}

/// Accrues deterministic execution cost and enforces an optional per-statement ceiling **and** an
/// optional per-session budget (CLAUDE.md §13; spec/design/session.md §5.4). Threaded by `&mut`
/// through the executor and the recursive expression evaluator; the accrued (per-statement) total is
/// reported on `Outcome`, while the session cumulative is updated live through [`Lifetime`].
#[derive(Default)]
pub struct Meter {
    /// Total cost accrued so far **for this statement** (CLAUDE.md §13) — the figure reported on
    /// `Outcome` and asserted by the `# cost:` directive. `i64` mirrors the engine's native integer;
    /// the per-statement ceiling compares against this counter.
    pub accrued: i64,
    /// The caller-set per-statement cost ceiling, or `0` (the default) for **unlimited**. A positive
    /// value bounds an untrusted query: the instant accrued cost reaches it, the next
    /// [`guard`](Meter::guard) aborts with `54P01` (spec/design/cost.md §6). Carried from the
    /// session's `max_cost` setting (spec/design/api.md §8).
    pub limit: i64,
    /// The session lifetime budget (spec/design/session.md §5.4), or `None` for a meter with no
    /// session context (the unit-test / build-meter path). When present, every [`charge`](Meter::charge)
    /// also accrues into the session's cumulative total, and [`guard`](Meter::guard) aborts with
    /// `54P02` once that cumulative reaches the budget.
    lifetime: Option<Lifetime>,
    /// An optional cancellation poll (spec/design/api.md §11.4): when present and its flag is set, the
    /// next [`guard`](Meter::guard) aborts the statement with `57014 query_canceled`. It rides this same
    /// chokepoint so a host's cancellation handle interrupts a long-running statement at the next
    /// metering point — NOT only at the cursor boundary. `None` ⇒ no cancellation (the default; the
    /// path every conformance / cost test takes — cost is unaffected, the §8 determinism contract
    /// intact). The poll is a single relaxed atomic load ([`CancellationToken::is_cancelled`]).
    cancel: Option<CancellationToken>,
}

impl Meter {
    /// A fresh meter with zero accrued cost, no ceiling, and no session context.
    pub fn new() -> Self {
        Meter::default()
    }

    /// A fresh meter that aborts once accrued cost reaches `limit` (`limit <= 0` ⇒ unlimited), with
    /// no session lifetime budget. The ceiling is the session's `max_cost` (spec/design/api.md §8).
    /// Used where there is no session cumulative to thread (tests, isolated build scans).
    pub fn with_limit(limit: i64) -> Self {
        Meter {
            accrued: 0,
            limit,
            lifetime: None,
            cancel: None,
        }
    }

    /// A fresh meter for a statement run on a session (spec/design/session.md §5.4): the per-statement
    /// `limit` (`max_cost`), the session lifetime budget the meter live-charges into, and the session's
    /// optional cancellation poll (`57014`, spec/design/api.md §11.4). Built by
    /// [`Session::new_meter`](crate::executor) for every statement, so the session cumulative tracks
    /// all execution cost, the `54P02` budget binds, and an armed cancellation interrupts a running
    /// statement at the next `guard`.
    pub fn for_session(limit: i64, lifetime: Lifetime, cancel: Option<CancellationToken>) -> Self {
        Meter {
            accrued: 0,
            limit,
            lifetime: Some(lifetime),
            cancel,
        }
    }

    /// Charge `units` of cost. The single accrual chokepoint. Accrues into both the per-statement
    /// counter (the `# cost:` contract) **and** — when a session lifetime budget is attached — the
    /// session's cumulative total (live), so partial cost of an aborted statement counts. Enforcement
    /// is **not** here: [`guard`](Meter::guard) does the comparisons at the work loops, so the
    /// cross-core accrual count is untouched.
    #[inline]
    pub fn charge(&mut self, units: i64) {
        self.accrued += units;
        if let Some(l) = &self.lifetime {
            l.total.set(l.total.get() + units);
        }
    }

    /// Whether NO enforcement is armed — no per-statement ceiling, no session lifetime budget, and no
    /// cancellation poll — so [`guard`](Meter::guard) is a no-op. The gate for the Track A2/A3 columnar
    /// fast path (packed-leaf.md §11): it charges the scan block in bulk (not per row with an
    /// intervening guard), which reproduces the row path's exact total only when there is nothing to
    /// abort against; a metered query keeps the row path so its deterministic `54P01`/`54P02`/`57014`
    /// abort row is unchanged. This is the conformance/bench lane (no ceiling / budget / cancellation).
    #[inline]
    pub fn is_unmetered(&self) -> bool {
        self.limit <= 0
            && self.cancel.is_none()
            && match &self.lifetime {
                None => true,
                Some(l) => l.limit <= 0,
            }
    }

    /// Enforce the ceilings: abort if the per-statement `max_cost` (`54P01`) **or** the session
    /// `lifetime_max_cost` (`54P02`) has been **reached** (`>=`, CLAUDE.md §13 — "the instant accrued
    /// cost reaches it, execution aborts"). When both are over, the one **reached first** wins — the
    /// ceiling crossed at the lower accrued value, i.e. the larger excess; an exact tie breaks to the
    /// per-statement `54P01` (the inner gate). Called at the unbounded-work points — the same mirrored
    /// points in every core, so the abort is deterministic and cross-core identical
    /// (spec/design/cost.md §6, spec/design/session.md §5.4). A no-op (one or two comparisons) when
    /// both are unlimited, so it is free on the hot path by default.
    #[inline]
    pub fn guard(&self) -> Result<()> {
        // Cancellation is checked first and independently of the cost ceilings: a flipped token aborts
        // regardless of accrued cost (spec/design/api.md §11.4). The `None` check short-circuits when no
        // cancellation handle is armed, so the cost accrual and the cross-core abort points are unchanged
        // (CLAUDE.md §8) — this never fires in the conformance / cost suites.
        if let Some(cancel) = &self.cancel
            && cancel.is_cancelled()
        {
            return Err(EngineError::new(
                SqlState::QueryCanceled,
                "canceling statement due to user request",
            ));
        }
        let stmt_over = self.limit > 0 && self.accrued >= self.limit;
        let life = match &self.lifetime {
            Some(l) if l.limit > 0 => Some((l.total.get(), l.limit)),
            _ => None,
        };
        let life_over = matches!(life, Some((total, limit)) if total >= limit);
        if !stmt_over && !life_over {
            return Ok(());
        }
        // Pick the ceiling reached first. Both counters grow in lockstep, so the one crossed at the
        // lower accrued value has the larger excess by the time this guard fires; a tie breaks to the
        // per-statement ceiling.
        let pick_life = if stmt_over && life_over {
            let (total, limit) = life.expect("life_over implies a budget");
            (total - limit) > (self.accrued - self.limit)
        } else {
            life_over
        };
        if pick_life {
            let (total, limit) = life.expect("pick_life implies a budget");
            Err(EngineError::new(
                SqlState::SessionCostLimitExceeded,
                format!("session exceeded the lifetime cost limit of {limit} (accrued {total})"),
            ))
        } else {
            Err(EngineError::new(
                SqlState::CostLimitExceeded,
                format!(
                    "query exceeded the cost limit of {} (accrued {})",
                    self.limit, self.accrued
                ),
            ))
        }
    }
}
