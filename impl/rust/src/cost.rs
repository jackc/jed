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

/// Releases that exceeded their account's balance — always an engine accounting bug. Read by the
/// conformance harness's whole-corpus accounting mode (`rake conformance:query_memory`).
pub static QUERY_MEMORY_UNDERFLOWS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The highest balance any account has reached since the conformance harness last reset it — the
/// minimal passing `max_query_memory_bytes` of the record just run. The harness's peak mode
/// (`rake conformance:query_memory`) compares it across cores for every record.
pub static QUERY_MEMORY_PEAK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// A statement's live query-memory account (spec/design/memory.md §2): the shared running total and
/// the budget (`limit <= 0` ⇒ unlimited). Cloned into every meter of the statement and into the
/// cursor that outlives it, so all of them reserve against one total.
#[derive(Clone, Default)]
pub(crate) struct QueryAccount {
    pub(crate) used: Rc<Cell<i64>>,
    pub(crate) limit: i64,
    /// The balance the account opened with — its transaction's pending writes (memory.md §7). The
    /// statement's releases return only what it reserved, so none may take `used` below this.
    pub(crate) floor: i64,
}

impl QueryAccount {
    #[inline]
    pub(crate) fn active(&self) -> bool {
        self.limit > 0
    }

    /// Reserve `bytes`, or fail `54P05` when `used + bytes > limit` (equality allowed).
    pub(crate) fn reserve(&self, bytes: i64) -> Result<()> {
        if self.try_reserve(bytes) {
            return Ok(());
        }
        Err(EngineError::new(
            SqlState::QueryMemoryLimitExceeded,
            format!("query memory exceeded the limit of {} bytes", self.limit),
        ))
    }

    /// Reserve `bytes` if they fit; `false` (nothing reserved) when `used + bytes > limit`.
    pub(crate) fn try_reserve(&self, bytes: i64) -> bool {
        if self.limit <= 0 {
            return true;
        }
        if bytes > self.limit - self.used.get() {
            return false;
        }
        let used = self.used.get() + bytes;
        self.used.set(used);
        QUERY_MEMORY_PEAK.fetch_max(used, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// Return `bytes`; never errors, never below the opening balance (`floor`).
    pub(crate) fn release(&self, bytes: i64) {
        if self.limit <= 0 {
            return;
        }
        let used = self.used.get();
        if bytes > used - self.floor {
            // An accounting bug (a release without its reservation). Clamp, and count it so the
            // conformance harness's accounting mode can fail the record that caused it.
            QUERY_MEMORY_UNDERFLOWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.used.set(self.floor.min(used));
            return;
        }
        self.used.set(used - bytes);
    }

    /// Admit a projected row (memory.md §5.1).
    pub(crate) fn admit_row(&self, row: &[crate::value::Value]) -> Result<()> {
        if !self.active() {
            return Ok(());
        }
        self.reserve(crate::memsize::row_bytes(row))
    }

    /// Release a projected row leaving engine ownership (memory.md §5.3).
    pub(crate) fn release_row(&self, row: &[crate::value::Value]) {
        if self.active() {
            self.release(crate::memsize::row_bytes(row));
        }
    }

    /// The bytes of a buffer of projected rows, or `0` when unlimited (nothing is measured).
    pub(crate) fn measure_rows(&self, rows: &[Vec<crate::value::Value>]) -> i64 {
        if !self.active() {
            return 0;
        }
        rows.iter().map(|r| crate::memsize::row_bytes(r)).sum()
    }
}

/// The query-memory charge an operator-state structure holds (spec/design/memory.md §6): the bytes it
/// has reserved against the statement's account and not yet returned. Released explicitly at the
/// structure's spec'd release point; dropping it returns whatever is still held, so an error path
/// never strands a charge. Inert (and free) when the account is unlimited.
#[derive(Default)]
pub(crate) struct StateCharge {
    acct: QueryAccount,
    held: i64,
}

impl StateCharge {
    pub(crate) fn new(acct: QueryAccount) -> Self {
        StateCharge { acct, held: 0 }
    }

    /// Whether the account has a finite budget — callers skip measuring otherwise.
    #[inline]
    pub(crate) fn active(&self) -> bool {
        self.acct.active()
    }

    /// Reserve `bytes` through `meter` (so a rejection consults the cost guard first, memory.md §6.7).
    pub(crate) fn reserve(&mut self, meter: &mut Meter, bytes: i64) -> Result<()> {
        if !self.acct.active() {
            return Ok(());
        }
        meter.reserve_query(bytes)?;
        self.held += bytes;
        Ok(())
    }

    /// Reserve `bytes` straight against the account, for a structure with no meter in reach. The
    /// caller wraps the outcome in [`Meter::cost_first`] (memory.md §6.7).
    pub(crate) fn reserve_direct(&mut self, bytes: i64) -> Result<()> {
        if !self.acct.active() {
            return Ok(());
        }
        self.acct.reserve(bytes)?;
        self.held += bytes;
        Ok(())
    }

    /// Reserve `bytes` straight against the account if they fit, for a spill-capable structure: `false`
    /// (nothing reserved) tells it to spill instead of failing `54P05` (memory.md §6.6).
    pub(crate) fn try_reserve_direct(&mut self, bytes: i64) -> bool {
        if !self.acct.try_reserve(bytes) {
            return false;
        }
        if self.acct.active() {
            self.held += bytes;
        }
        true
    }

    /// Return `bytes` of this structure's charge.
    pub(crate) fn release(&mut self, bytes: i64) {
        if !self.acct.active() {
            return;
        }
        self.acct.release(bytes);
        self.held -= bytes;
    }

    /// Return everything this structure still holds.
    pub(crate) fn release_all(&mut self) {
        if self.held != 0 {
            self.acct.release(self.held);
            self.held = 0;
        }
    }

    /// The bytes currently held.
    pub(crate) fn held(&self) -> i64 {
        self.held
    }
}

impl Drop for StateCharge {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// Accrues deterministic execution cost and enforces an optional per-statement ceiling **and** an
/// optional per-session budget (CLAUDE.md §13; spec/design/session.md §5.4). Threaded by `&mut`
/// through the executor and the recursive expression evaluator; the accrued (per-statement) total is
/// reported on `Outcome`, while the session cumulative is updated live through [`Lifetime`].
#[derive(Default)]
pub struct Meter {
    ceiling_hit: u8,
    pub(crate) scalar_bytes: Rc<Cell<i64>>,
    pub(crate) scalar_limit: i64,
    /// The statement's live query-memory account (spec/design/memory.md §2), shared by every meter
    /// of the statement like `scalar_bytes`; an unlimited account never computes a size.
    pub(crate) query: QueryAccount,
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
    /// Admit cumulative logical scalar allocation before constructing it. Zero uses the default.
    pub fn reserve_scalar(&mut self, bytes: i64) -> Result<()> {
        let limit = if self.scalar_limit > 0 {
            self.scalar_limit
        } else {
            crate::costs::DEFAULT_SCALAR_BYTES
        };
        if bytes > limit - self.scalar_bytes.get() {
            return Err(EngineError::new(
                SqlState::ScalarMemoryLimitExceeded,
                format!("scalar allocations exceeded the limit of {limit} bytes"),
            ));
        }
        self.scalar_bytes.set(self.scalar_bytes.get() + bytes);
        Ok(())
    }

    /// Whether the statement has a finite query-memory budget (spec/design/memory.md §2). Every
    /// admission site tests this first, so the unlimited default computes no sizes.
    #[inline]
    pub fn query_memory_active(&self) -> bool {
        self.query.active()
    }

    /// Reserve `bytes` of live query memory, or fail `54P05` (equality allowed). A rejected
    /// reservation consults the meter's cost guard first, so a step that has already reached a cost
    /// ceiling reports the cost error (spec/design/memory.md §2) — even where the charging site
    /// itself does not guard. A successful reservation adds no guard point.
    pub fn reserve_query(&mut self, bytes: i64) -> Result<()> {
        if let Err(e) = self.query.reserve(bytes) {
            self.guard()?;
            return Err(e);
        }
        Ok(())
    }

    /// Return `bytes` of live query memory; never errors, never below zero.
    pub fn release_query(&mut self, bytes: i64) {
        self.query.release(bytes);
    }

    /// Apply the cost-wins rule to a reservation made without this meter (a spill structure or sorter
    /// reserving through its [`StateCharge`], memory.md §6.7): a `54P05` first consults the cost guard,
    /// so a step that already reached a cost ceiling reports the cost error. Sound at any point on the
    /// error's way out, because accrued cost only grows and nothing is charged in between.
    pub(crate) fn cost_first<T>(&mut self, r: Result<T>) -> Result<T> {
        match r {
            Err(e) if e.state == SqlState::QueryMemoryLimitExceeded => {
                self.guard()?;
                Err(e)
            }
            r => r,
        }
    }

    /// A fresh operator-state charge against this statement's account (memory.md §6).
    pub(crate) fn state_charge(&self) -> StateCharge {
        StateCharge::new(self.query.clone())
    }

    /// Admit a projected row appended to a row buffer (memory.md §5.1).
    #[inline]
    pub fn admit_row(&mut self, row: &[crate::value::Value]) -> Result<()> {
        if !self.query_memory_active() {
            return Ok(());
        }
        self.reserve_query(crate::memsize::row_bytes(row))
    }

    /// Admit a pre-projection row under the plan's touched mask (memory.md §3).
    #[inline]
    pub fn admit_row_masked(&mut self, row: &[crate::value::Value], mask: &[bool]) -> Result<()> {
        if !self.query_memory_active() {
            return Ok(());
        }
        self.reserve_query(crate::memsize::row_bytes_masked(row, mask))
    }

    /// Admit a value appended to a buffered row (memory.md §5.1).
    #[inline]
    pub fn admit_value(&mut self, v: &crate::value::Value) -> Result<()> {
        if !self.query_memory_active() {
            return Ok(());
        }
        self.reserve_query(crate::memsize::value_bytes(v))
    }

    /// Release a discarded projected row.
    #[inline]
    pub fn release_row(&mut self, row: &[crate::value::Value]) {
        if self.query_memory_active() {
            self.release_query(crate::memsize::row_bytes(row));
        }
    }

    /// Release a discarded pre-projection row under the plan's touched mask.
    #[inline]
    pub fn release_row_masked(&mut self, row: &[crate::value::Value], mask: &[bool]) {
        if self.query_memory_active() {
            self.release_query(crate::memsize::row_bytes_masked(row, mask));
        }
    }

    /// Release a whole discarded buffer of projected rows.
    pub fn release_rows(&mut self, rows: &[Vec<crate::value::Value>]) {
        if self.query_memory_active() {
            let n = rows.iter().map(|r| crate::memsize::row_bytes(r)).sum();
            self.release_query(n);
        }
    }

    /// Release a whole discarded buffer of pre-projection rows under the touched mask.
    pub fn release_rows_masked(&mut self, rows: &[Vec<crate::value::Value>], mask: &[bool]) {
        if self.query_memory_active() {
            let n = rows
                .iter()
                .map(|r| crate::memsize::row_bytes_masked(r, mask))
                .sum();
            self.release_query(n);
        }
    }

    /// A fresh meter with zero accrued cost, no ceiling, and no session context.
    pub fn new() -> Self {
        Meter::default()
    }

    /// A fresh meter that aborts once accrued cost reaches `limit` (`limit <= 0` ⇒ unlimited), with
    /// no session lifetime budget. The ceiling is the session's `max_cost` (spec/design/api.md §8).
    /// Used where there is no session cumulative to thread (tests, isolated build scans).
    pub fn with_limit(limit: i64) -> Self {
        Meter {
            ceiling_hit: 0,
            scalar_bytes: Rc::new(Cell::new(0)),
            scalar_limit: crate::costs::DEFAULT_SCALAR_BYTES,
            query: QueryAccount::default(),
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
            ceiling_hit: 0,
            scalar_bytes: Rc::new(Cell::new(0)),
            scalar_limit: crate::costs::DEFAULT_SCALAR_BYTES,
            query: QueryAccount::default(),
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
        if self.ceiling_hit == 0 {
            let stmt = self.limit > 0 && units >= self.limit - self.accrued;
            let life = self
                .lifetime
                .as_ref()
                .is_some_and(|l| l.limit > 0 && units >= l.limit - l.total.get());
            if stmt {
                self.ceiling_hit = 1;
            }
            if life
                && (!stmt
                    || self
                        .lifetime
                        .as_ref()
                        .is_some_and(|l| l.limit - l.total.get() < self.limit - self.accrued))
            {
                self.ceiling_hit = 2;
            }
        }

        self.accrued = self.accrued.saturating_add(units);
        if let Some(l) = &self.lifetime {
            l.total.set(l.total.get().saturating_add(units));
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
        let mut pick_life = if stmt_over && life_over {
            let (total, limit) = life.expect("life_over implies a budget");
            (total - limit) > (self.accrued - self.limit)
        } else {
            life_over
        };
        if self.ceiling_hit != 0 {
            pick_life = self.ceiling_hit == 2;
        }
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

#[cfg(test)]
mod resource_tests {
    use super::*;

    /// A statement's account opens holding its transaction's pending writes (memory.md §7): a
    /// release past the statement's own reservations is an accounting bug, clamped at that floor and
    /// counted.
    #[test]
    fn release_never_dips_into_the_pending_write_floor() {
        let acct = QueryAccount {
            used: Rc::new(Cell::new(30)),
            limit: 100,
            floor: 30,
        };
        acct.reserve(20).unwrap();
        let before = QUERY_MEMORY_UNDERFLOWS.load(std::sync::atomic::Ordering::Relaxed);
        acct.release(25);
        assert_eq!(acct.used.get(), 30);
        assert!(QUERY_MEMORY_UNDERFLOWS.load(std::sync::atomic::Ordering::Relaxed) > before);
        acct.reserve(10).unwrap();
        acct.release(10);
        assert_eq!(acct.used.get(), 30);
    }
    #[test]
    fn saturation_keeps_first_crossed_ceiling() {
        let total = Rc::new(Cell::new(i64::MAX - 2));
        let mut m = Meter::for_session(
            i64::MAX,
            Lifetime {
                total: total.clone(),
                limit: i64::MAX - 1,
            },
            None,
        );
        m.charge(i64::MAX);
        assert_eq!(m.accrued, i64::MAX);
        assert_eq!(total.get(), i64::MAX);
        assert_eq!(m.guard().unwrap_err().code(), "54P02");
        m.charge(10);
        assert_eq!(m.accrued, i64::MAX);
    }
}
