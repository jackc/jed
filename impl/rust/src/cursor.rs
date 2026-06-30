//! The pull source a [`Rows`](crate::Rows) cursor drives (spec/design/streaming.md §4).
//!
//! The cursor comes in two shapes, chosen by the plan (streaming.md §4):
//! - **`Buffered`** — a fully materialized result, walked one row at a time. The executor ran the
//!   query to completion (the `execute()` path that the conformance corpus drives, so it is byte-
//!   unchanged) and the accrued `cost` is already final. This is the `query()` fallback for the shapes
//!   no lazy `RowStream` covers yet (a data-modifying `WITH`).
//! - **`Streaming`** — a lazy pull source behind the [`RowStream`] trait (so this module stays free of
//!   executor internals), in three executor-side flavors: the **`StreamingScan`** (S3) for the
//!   single-table, no-blocking-operator scan — it owns a [pull B-tree scan cursor](crate::storage::StoreScan)
//!   over a pinned snapshot (streaming.md §5) and runs scan → resolve → `WHERE` → project **one row per
//!   `next_row`**; the **`BufferedScan`** (S4) for a blocking plan (sort/aggregate/join/DISTINCT/window)
//!   — it buffers its input on the first pull, then yields the output a row at a time; and the
//!   **`DeferredResult`** for a top-level set operation / pure-query `WITH` (streaming.md §7) — it
//!   defers the whole run to the first pull, then yields the result a row at a time. All three accrue
//!   cost as the cursor is pulled (streaming.md §6), so a caller that stops early does — and charges —
//!   less, and a fully-drained query observes the same rows + total cost as the eager path.

use crate::error::Result;
use crate::value::Value;

/// A lazy pull row source — the streaming pipeline behind [`Cursor::Streaming`] (streaming.md §4).
/// Implemented by the executor's `StreamingScan`; kept as a trait so `cursor.rs` does not depend on
/// the executor's plan/engine internals.
pub(crate) trait RowStream {
    /// The next projected output row, `Ok(None)` at end. May raise mid-drain (a `54P01` cost abort,
    /// a `57014` cancellation, or an arithmetic trap) — surfaced as the statement's error
    /// (streaming.md §6).
    fn next_row(&mut self) -> Result<Option<Vec<Value>>>;
    /// The cost accrued so far — final once the stream is drained (streaming.md §6).
    fn cost(&self) -> i64;
    /// Release the pinned read snapshot (streaming.md §5). Idempotent.
    fn close(&mut self);
}

/// The pull source behind a [`Rows`](crate::Rows) cursor.
pub(crate) enum Cursor {
    /// A fully materialized result, walked one row at a time. The executor ran to completion and
    /// the accrued `cost` is already final.
    Buffered {
        iter: std::vec::IntoIter<Vec<Value>>,
        cost: i64,
    },
    /// A lazy pull pipeline (S3, streaming.md §4): scan → resolve → `WHERE` → project, one row per
    /// `next_row`, accruing cost as it is pulled. Owns its pinned snapshot.
    Streaming(Box<dyn RowStream>),
}

impl Cursor {
    /// A cursor over an already-materialized result (the buffered shape).
    pub(crate) fn buffered(rows: Vec<Vec<Value>>, cost: i64) -> Cursor {
        Cursor::Buffered {
            iter: rows.into_iter(),
            cost,
        }
    }

    /// A lazy streaming cursor over the given pull source (S3, streaming.md §4).
    pub(crate) fn streaming(source: Box<dyn RowStream>) -> Cursor {
        Cursor::Streaming(source)
    }

    /// Pull the next output row, or `Ok(None)` at end. `Buffered` just advances the iterator (never
    /// errors); `Streaming` does the per-row work and accrues its cost — so it may raise mid-drain
    /// (streaming.md §6).
    pub(crate) fn next_row(&mut self) -> Result<Option<Vec<Value>>> {
        match self {
            Cursor::Buffered { iter, .. } => Ok(iter.next()),
            Cursor::Streaming(s) => s.next_row(),
        }
    }

    /// The accrued execution cost (CLAUDE.md §13). Final after the cursor is drained
    /// (streaming.md §6); for `Buffered` it is final immediately (the work is already done).
    pub(crate) fn cost(&self) -> i64 {
        match self {
            Cursor::Buffered { cost, .. } => *cost,
            Cursor::Streaming(s) => s.cost(),
        }
    }

    /// Release any pinned read snapshot (streaming.md §5). Idempotent. A no-op for `Buffered` — it
    /// owns a detached `Vec` and pins nothing; `Streaming` releases its scan snapshot here (and on
    /// `Drop`).
    pub(crate) fn close(&mut self) {
        if let Cursor::Streaming(s) = self {
            s.close();
        }
    }
}
