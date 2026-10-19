//! Cooperative statement cancellation (spec/design/api.md §11.4).
//!
//! A [`CancellationToken`] is a clonable handle around a shared atomic flag. The host arms a clone
//! on the session that runs a statement (the cancelable query/execute methods on
//! [`Session`](crate::Session) / [`Database`](crate::Database) / [`Transaction`](crate::Transaction)),
//! and the statement's [`Meter`](crate::cost::Meter) carries it through cursor execution.
//! Execution loops poll at row/operator steps and between vectorized batches of up to
//! 256 records; host-call boundaries poll immediately. Ordinary expression cost guards do not poll. Cost accrual and
//! deterministic limit enforcement remain independent of cancellation frequency.
//!
//! A [`CancellationToken`] is an `Arc<AtomicBool>`; another thread flips the same flag
//! without a watcher. Go polls a cached context Done channel instead. TS, which cannot
//! preempt synchronous execution, honors its AbortSignal at boundaries (api.md §11.4).
//! Cancellation is per-core unit-tested, never in the corpus: its timing is nondeterministic.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{EngineError, Result};
use crate::sqlstate::SqlState;

/// A clonable cancellation handle (spec/design/api.md §11.4). Cloning shares the underlying flag
/// (`Arc<AtomicBool>`), so a clone handed to another thread cancels the statement the original is
/// arming. Cheap to clone and to poll (a single relaxed atomic load on the hot path).
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// A fresh, un-cancelled token.
    pub fn new() -> Self {
        CancellationToken::default()
    }

    /// Request cancellation. Idempotent, and callable from any thread — the statement arming a clone
    /// of this token aborts `57014` at its next [`Meter::guard`](crate::cost::Meter::guard).
    pub fn cancel(&self) {
        // Relaxed is sufficient: the flag synchronizes no other memory, only signals "abort at the
        // next checkpoint." The abort still surfaces as a clean error on the running thread.
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// The cheap boundary poll (the analog of Go's `ctxErr`): `Err(57014)` if already cancelled, else
    /// `Ok`. Used at the API entry of a cancelable call before any work, complementing the in-statement
    /// meter `guard`.
    pub(crate) fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(EngineError::new(
                SqlState::QueryCanceled,
                "canceling statement due to user request",
            ))
        } else {
            Ok(())
        }
    }
}
