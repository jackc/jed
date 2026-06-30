import type { Value } from "./value.ts";

// Cursor is the pull source a Rows cursor drives (spec/design/streaming.md §4).
//
// Two cursor shapes (buffered / streaming), chosen by the plan (streaming.md §4/§7):
//   - buffered — a fully materialized result, walked one row at a time. The executor ran the query to
//     completion (the execute() path the conformance corpus drives, byte-unchanged); cost is final. The
//     query() fallback for the shapes no lazy RowSource covers yet (a data-modifying WITH).
//   - streaming — a lazy pull RowSource (this module stays free of executor internals), in three
//     executor-side flavors: the S3 single-table no-blocking-op scan (a streamRows generator: scan →
//     resolve → WHERE → project, ONE row per nextRow over a pinned snapshot); the S4 BUFFERED blocking
//     plan (a bufferedRows generator that buffers its input on the first pull, then yields the output a
//     row at a time); and the DEFERRED top-level set operation / pure-query WITH (streaming.md §7) that
//     defers the whole run to the first pull, then yields the result a row at a time. All three accrue
//     cost as the cursor is pulled (streaming.md §6) and may throw mid-drain.
//
// A Cursor is SINGLE-PASS — nextRow advances and never rewinds — so Rows is single-pass too, matching
// the Rust (Iterator) and Go (Next) cores and the streaming contract (a stream cannot be re-read).

// RowSource is the lazy pull pipeline behind a streaming Cursor (executor.ts builds one over a
// generator). Kept as an interface so cursor.ts stays free of executor internals.
export interface RowSource {
  // nextRow pulls the next output row, or undefined at end. May THROW mid-drain (a 54P01 cost abort or
  // an arithmetic trap) — the throw propagates through Rows' iterator as the statement's error
  // (streaming.md §6).
  nextRow(): Value[] | undefined;
  // cost is the cost accrued so far — final once the source is drained (streaming.md §6).
  cost(): bigint;
  // close releases the pinned read snapshot (streaming.md §5). Idempotent.
  close(): void;
}

export class Cursor {
  private readonly source: RowSource;

  private constructor(source: RowSource) {
    this.source = source;
  }

  // buffered wraps an already-materialized result (the buffered shape).
  static buffered(rows: Value[][], cost: bigint): Cursor {
    let i = 0;
    return new Cursor({
      nextRow: () => (i < rows.length ? rows[i++]! : undefined),
      cost: () => cost,
      close: () => {},
    });
  }

  // streaming wraps a lazy pull pipeline (S3, streaming.md §4).
  static streaming(source: RowSource): Cursor {
    return new Cursor(source);
  }

  // nextRow pulls the next output row, or undefined at end. For streaming this does the per-row work
  // (and accrues cost — streaming.md §6), so it may throw mid-drain.
  nextRow(): Value[] | undefined {
    return this.source.nextRow();
  }

  // cost is the accrued execution cost (CLAUDE.md §13). Final after the cursor is drained
  // (streaming.md §6); for the buffered shape it is final immediately.
  cost(): bigint {
    return this.source.cost();
  }

  // close releases any pinned read snapshot (streaming.md §5). Idempotent. A no-op for the buffered
  // shape; the streaming shape returns its generator and releases its scan snapshot here.
  close(): void {
    this.source.close();
  }
}
