//! Cooperative statement cancellation (spec/design/api.md §11.4).
//!
//! A [`CancellationToken`] is a clonable handle around a shared atomic flag. The host arms a clone
//! on the session that runs a statement (the cancelable query/execute methods on
//! [`Session`](crate::Session) / [`Database`](crate::Database) / [`Transaction`](crate::Transaction)),
//! and the statement's cost [`Meter`](crate::cost::Meter) consults it at every
//! [`guard`](crate::cost::Meter::guard) checkpoint — the same single chokepoint already run at the
//! unbounded-work points for `max_cost` (spec/design/cost.md §6). A flipped token therefore aborts a
//! **long-running** statement with `57014 query_canceled` at the next metering point, not only at the
//! cursor / entry boundary.
//!
//! Rust has real threads (CLAUDE.md §2), so the token is the whole mechanism: thread A runs the
//! query while thread B (a timeout, a request-cancel) flips a clone of the *same* token. No watcher
//! is needed — the token *is* the shared atomic the Go core spins up a goroutine to feed. It is the
//! Rust spelling of Go's `context.Context` cancellation; TS, which cannot preempt synchronous
//! execution, honors its `AbortSignal` only at boundaries (api.md §11.4).
//!
//! This rides the existing meter seam, so it touches no conformance / cost path: an un-armed meter
//! carries `None`, the `guard` short-circuits, and the cross-core cost determinism (CLAUDE.md §8) is
//! untouched. Cancellation is per-core unit-tested, never in the corpus — whether a cancel lands at
//! row 1k or 5k is timing (api.md §11.4, CLAUDE.md §10).

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
