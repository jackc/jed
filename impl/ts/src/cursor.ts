import type { Value } from "./value.ts";

// Cursor is the pull source a Rows cursor drives (spec/design/streaming.md §4).
//
// S1 ships only the buffered shape: the executor still materializes the full result and the cursor
// walks it one row at a time — today's behavior, byte-unchanged. Its purpose is the seam: Rows is
// now defined in terms of this Cursor (nextRow / cost / close) rather than a concrete Value[][], so
// a later streaming shape (streaming.md §4, S3) can plug in without changing Rows or any caller.
// The streaming shape will own the pull B-tree scan cursor (S2) and the pinned read snapshot
// (streaming.md §5), accrue cost as it is pulled (streaming.md §6), and deregister its
// reader-liveness pin in close.
//
// A Cursor is SINGLE-PASS — nextRow advances an internal position and never rewinds — so Rows is
// single-pass too, matching the Rust (`Iterator`) and Go (`Next`) cores and the streaming contract
// (a stream cannot be re-read). The old materialized TS Rows happened to be re-iterable; that was an
// accident of the implementation, not a contract.
export class Cursor {
  private readonly rows: Value[][];
  private readonly accrued: bigint;
  private i = 0;

  constructor(rows: Value[][], cost: bigint) {
    this.rows = rows;
    this.accrued = cost;
  }

  // nextRow pulls the next output row, or undefined at end. For the buffered shape this just
  // advances the index; it is the site where a streaming shape does the per-row work (and accrues
  // its cost — streaming.md §6).
  nextRow(): Value[] | undefined {
    return this.i < this.rows.length ? this.rows[this.i++]! : undefined;
  }

  // cost is the accrued execution cost (CLAUDE.md §13). Final after the cursor is drained
  // (streaming.md §6); for the buffered shape it is final immediately (the work is already done).
  cost(): bigint {
    return this.accrued;
  }

  // close releases any pinned read snapshot (streaming.md §5). Idempotent. A no-op for the buffered
  // shape — it owns a detached Value[][] and pins nothing; the streaming shape (S3) deregisters its
  // reader-liveness pin here.
  close(): void {}
}
