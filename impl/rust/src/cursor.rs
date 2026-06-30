//! The pull source a [`Rows`](crate::Rows) cursor drives (spec/design/streaming.md §4).
//!
//! S1 ships only the `Buffered` shape: the executor still materializes the full result and the
//! cursor walks it one row at a time — today's behavior, byte-unchanged. Its purpose is the
//! **seam**: `Rows` is now defined in terms of this `Cursor` (`next_row` / `cost` / `close`)
//! rather than a concrete `Vec`/`IntoIter`, so a later `Streaming` variant (streaming.md §4, S3)
//! can plug in without changing `Rows` or any caller. The `Streaming` variant will own the pull
//! B-tree scan cursor (S2) and the pinned read snapshot (streaming.md §5), accrue cost as it is
//! pulled (streaming.md §6), and release its watermark pin in `close`.

use crate::value::Value;

/// The pull source behind a [`Rows`](crate::Rows) cursor.
pub(crate) enum Cursor {
    /// A fully materialized result, walked one row at a time. The executor ran to completion and
    /// the accrued `cost` is already final (the `Streaming` variant accrues during `next_row`).
    Buffered {
        iter: std::vec::IntoIter<Vec<Value>>,
        cost: i64,
    },
}

impl Cursor {
    /// A cursor over an already-materialized result (the S1 shape).
    pub(crate) fn buffered(rows: Vec<Vec<Value>>, cost: i64) -> Cursor {
        Cursor::Buffered {
            iter: rows.into_iter(),
            cost,
        }
    }

    /// Pull the next output row, or `None` at end. For `Buffered` this just advances the iterator;
    /// it is the site where a future `Streaming` variant does the per-row work (and accrues its
    /// cost — streaming.md §6).
    pub(crate) fn next_row(&mut self) -> Option<Vec<Value>> {
        match self {
            Cursor::Buffered { iter, .. } => iter.next(),
        }
    }

    /// The accrued execution cost (CLAUDE.md §13). Final after the cursor is drained
    /// (streaming.md §6); for `Buffered` it is final immediately (the work is already done).
    pub(crate) fn cost(&self) -> i64 {
        match self {
            Cursor::Buffered { cost, .. } => *cost,
        }
    }

    /// Release any pinned read snapshot (streaming.md §5). Idempotent. A no-op for `Buffered` — it
    /// owns a detached `Vec` and pins nothing; the `Streaming` variant (S3) deregisters its
    /// reader-liveness pin here (and on `Drop`).
    pub(crate) fn close(&mut self) {}
}
