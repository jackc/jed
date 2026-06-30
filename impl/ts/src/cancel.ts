// Cooperative statement cancellation (spec/design/api.md §11.4).
//
// TS divergence (CLAUDE.md §2 — best experience per language, not a uniform-shape failure). Go and
// Rust have real threads, so another thread flips a token the cost meter polls MID-statement (the
// `guard()` checkpoint), interrupting a long-running statement at the next metering point. TypeScript
// runs on ONE event loop: nothing else — no timer, no I/O callback, no other thread — runs DURING a
// synchronous query()/execute(), so an AbortSignal's `aborted` state CANNOT change while a statement
// runs; it is frozen at the value it had on entry. Polling it at the meter checkpoint (the Go/Rust
// mechanism) would re-read that same value N times, so it is pointless here. TS therefore honors the
// signal at OPERATION BOUNDARIES only: the cancelable query/execute methods check it before any work
// and throw `57014 query_canceled` if it is already aborted — which usefully skips work for an
// already-canceled operation (e.g. a client that disconnected before the handler ran). Mid-statement
// cancellation would require the engine to become async (a streaming cursor that `await`s, §4); the
// boundary check is the forward-compatible seam for that day.
//
// The cancellation primitive is the platform `AbortSignal` (an `AbortController`), the same handle
// `fetch` and the web APIs use — so there is no custom token type (Rust ships `CancellationToken`
// only because it has no built-in equivalent). The meter and the SessionState are deliberately NOT
// wired, unlike Go/Rust: there is nothing for a synchronous run to poll.

import { engineError } from "./errors.ts";

// throwIfAborted throws `57014 query_canceled` when `signal` is already aborted, else returns. The
// boundary poll the cancelable host methods (Database/Session/Transaction `*Cancelable`) run before
// executing a statement. `undefined` (no signal supplied) is a no-op — zero overhead, the path every
// non-cancelable call and every conformance / cost test takes.
export function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted) {
    throw engineError("query_canceled", "canceling statement due to user request");
  }
}
