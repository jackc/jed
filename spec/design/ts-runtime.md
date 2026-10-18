# TypeScript runtime for browser and Node

> **Async API and worker runtime implemented.** The package entry points now use the
> independent TS core in dedicated Node/browser workers. Lifecycle, typed transport, scheduling,
> batching, cleanup and shared-flag cancellation are specified in §10. The synchronous core
> remains internal for conformance and host tooling. Cooperative interruption of blocking kernels
> without shared memory and asynchronous page/crypto suspension remain follow-on work; the
> implementation does not claim those acceptance gates are met. No production encryption
> dependency, encrypted format, Rust delivery default or parallel-reader pool is selected.
> See [cores.md](cores.md), [api.md](api.md), [hosts.md](hosts.md) and
> [encryption.md](encryption.md) for the surrounding contracts.

## 1. Direction

Use an **async-only public JavaScript API** on both Node and the browser. Database operations
execute off the application's main thread/event loop. Retain synchronous internal computation
where it is efficient; suspend at explicit scheduling, page-loading and crypto boundaries.
Returning a promise around a synchronous SQL call does not satisfy this design.

Keep **one independent TypeScript semantic core**, with platform-specific execution, storage,
crypto and packaging adapters. Browser and Node need different host integration, not separately
maintained parsers, planners, evaluators or type systems. Rust/Go embedding APIs are unaffected.
Rust/WASM and Rust/Node-API remain possible delivery packages using the existing Rust core;
neither becomes another independent conformance voice.

Do not add a public synchronous JS API in the application surface. Its strongest case is
minimum overhead for tiny in-process calls, but it would require a second lifecycle contract
and constrain async-only providers. Measure batching and worker overhead before reconsidering
that tradeoff. Internal synchronous entry points can continue serving the core and test harness.

## 2. Starting state and evidence

Before this migration the browser client returned promises and sent messages to a dedicated worker. The
worker runs synchronous TS SQL and OPFS I/O. Its current result protocol renders values as
strings, and its dispatch has no explicit operation scheduler. Async open/create already
require care; introducing awaits inside queries would create more interleavings.

The internal Node TS embedding API runs SQL synchronously. Its page-service coordination
worker does not offload application SQL. The blocking page-service channel uses shared memory
and `Atomics.wait`; moving coordination into a worker has not made the calling engine async.
Before this migration TS cancellation checked `AbortSignal` only at operation entry. The
worker runtime now installs the checkpoint hook described in §10.

Relevant implementation seams:

- [Browser client](../../impl/ts/src/browser/client.ts) and
  [worker dispatch](../../impl/ts/src/browser/worker.ts).
- [Node blocking channel](../../impl/ts/src/pageservice/blocking.ts) and
  [shared coordination state](../../impl/ts/src/pageservice/shared_state.ts).
- [Current cancellation boundary](../../impl/ts/src/cancel.ts).

The [browser crypto and WASM report](../../bench/aead/results/2026-10-17-browser-lifecycle.md)
found 8 KiB AES seal/open medians of 8.27/6.95 µs for Chromium WebCrypto, versus 54.84/53.05 µs
for software Rust/WASM AES and 166.30/139.57 µs for pure JS AES on the measured host. This is
motivation to accommodate WebCrypto, not an integrated encrypted-database result.

The [Node comparison](../../bench/aead/results/2026-10-18-node.md) found no universal engine
winner: TS won the constant query, Rust/WASI the point lookup and aggregate, and native Rust
the sort and larger result set. Calls included result conversion; five prepared warm-read
probes do not establish write performance, cold behavior, or a production package choice.
The unexpected native aggregate result remains unprofiled. Tiny direct queries taking about
1–3 µs make added messaging significant; worker round-trip overhead itself is not measured
by these probes. Responsiveness, throughput and individual-call latency need separate measures.

## 3. Execution and host adapters

| Concern | Node adapter / candidate | Browser adapter / candidate |
|---|---|---|
| Public API | Promises and async result iteration | Same operation/lifecycle shape |
| SQL execution | Long-lived `worker_threads` execution worker | Dedicated Web Worker |
| Storage | Existing file/page-service adapters inside execution context | Worker-owned OPFS sync handle |
| Crypto candidate | Native Node crypto behind the TS codec | WebCrypto with explicit internal suspension |
| Optional engine delivery | Existing Rust core with async Node binding | Existing Rust core with browser/WASM adapters |

Start with one execution worker per database handle/service, with several logical sessions.
Resolve multiple opens of the same storage identity through the existing ownership rules;
an execution worker is not automatically another file owner. Reuse workers across statements,
and bound their number. A process-wide worker pool is a later resource-management choice;
sessions, prepared statements, cursors and transactions need stable ownership if it is added.

Node synchronous file/crypto calls may run inside the execution worker. This keeps their cost
off the application event loop without adding awaits to every primitive. It still blocks that
worker, so cancellation, shutdown and fair scheduling need the mechanisms below. A native Rust
binding likewise must dispatch execution off-thread: compiling SQL to native code does not
make a synchronous addon call nonblocking.

Use message ports and transfer ownership of dedicated buffers for requests/results where useful.
Shared memory is an optional control-plane optimization, not a shared heap of TS database objects.
`Atomics.wait` may block a designated worker, never the application thread; coordination that
must remain responsive needs an async wait or message-driven state machine. Consult the
[Node worker API](https://nodejs.org/api/worker_threads.html) and
[ECMAScript waitAsync contract](https://tc39.es/ecma262/multipage/structured-data.html#sec-atomics.waitasync)
when implementing the adapters.

OPFS synchronous access handles belong in a dedicated worker under the
[File System Standard](https://fs.spec.whatwg.org/#api-filesystemfilehandle). A SharedWorker
may coordinate clients, but cannot simply replace that dedicated worker as the sync-handle
owner. Browser cross-tab ownership is separate follow-on work: evaluate cooperative
[Web Locks](https://www.w3.org/TR/web-locks/) plus the storage host's locking rules, without
PID files, timed leases or automatic stale-owner stealing. Broadcast notifications alone
do not confer write ownership. The initial proposal does not promise multiple concurrent
OPFS owners or transplant Node's OS lock protocol into the browser.

## 4. Public API and lifecycle

Preserve the concepts in api.md: database, session, transaction, prepared statement and rows.
Operations that invoke engine work return promises; rows support async iteration with bounded
batch fetching and explicit asynchronous close. Exact exported names/signatures remain open.
Cheap reads of already-returned metadata need not manufacture promises.

Each transport request identifies its database, session and operation; transaction, prepared
statement and cursor handles are worker-owned identifiers. Bind parameters and results use a
versioned lossless typed-value wire contract, covering bigint, decimals, bytes, NULL and open
types. Do not adopt the browser's rendered-string protocol as the production typed API.
Keep SQLSTATE and structured engine errors intact across the boundary; distinguish transport
failure from SQL failure. Do not pass mutable engine objects or arbitrary closures through clone.

Provide prepared/batched execution to amortize transport and parsing. Define batch transaction
boundaries explicitly: a batch must not accidentally turn several autocommit operations into
one transaction, or vice versa. A transaction callback, if offered, uses a transaction-bound
proxy while the worker retains ownership; awaiting unrelated application work must not expose
that transaction to another session.

Bound queued request count, queued bytes, in-flight result bytes and worker count. Admission
must apply backpressure or reject with a documented host error, rather than accumulate unlimited
promises/buffers. Reserve control capacity for cancellation, cursor close, rollback and shutdown.
Define these physical transport budgets separately from deterministic SQL cost and query-memory
accounting. Bound client-side encoding/decoding work as well as worker batches: moving execution
off-thread is insufficient if a huge result is synchronously decoded on the application thread.

Async iteration supplies demand and releases buffers after consumption. Early iterator return
closes the cursor and releases pins/resources. This bounds transport, not every engine operator;
blocking sorts, aggregates and currently materialized results retain their existing memory/spill
obligations. Do not advertise the entire executor as streaming merely because delivery is batched.

## 5. Scheduling, transactions and failure

Serialize operations within a session, with explicit transaction/cursor state transitions.
Preserve jed's single writer, committed-reader visibility and owner-only reclamation. Keep all
required snapshot pins, pending writes and writer ownership valid across awaits. Other sessions
cannot enter a suspended statement's mutable state. Close/rollback must not race its completion.

A single FIFO that waits for writer ownership can deadlock: session B's begin may block the
queue ahead of session A's commit. Park ownership waiters separately and continue accepting
operations that release the current owner. Define fairness and cancellation of queued requests;
transport arrival order alone is not a database concurrency policy.

One worker first establishes application responsiveness. CPU-parallel reads require additional
execution contexts and a coherent page-service/pin protocol, with bounded per-worker caches;
they do not follow from promises or `Promise.all`. Existing TS objects cannot be shared directly
across isolates. True parallel readers, intra-query parallelism and browser multi-tab services
are separate measured follow-ons, not initial promises.

Worker failure rejects pending requests and invalidates its handles. A lost reply after a
commit may leave the caller unable to tell whether publication/durability completed. Report
that uncertainty explicitly; never automatically replay a write to hide a transport failure.
Reopen/recovery uses the existing storage protocol. Graceful close stops admission, coordinates
in-flight work, releases cursors and transactions according to a documented policy, and only
then releases storage/worker resources. Shutdown policy and transport error names remain open.

## 6. Cancellation and cooperative execution

Map public `AbortSignal` to operation cancellation. A cancel message cannot interrupt a worker
that is busy executing synchronous JavaScript. Two mechanisms are available:

- A shared atomic flag written by the caller and polled at bounded engine checkpoints. This
  needs a new meter/checkpoint hook; the current entry-only check is insufficient.
- Cooperative execution that retains its continuation and yields periodically to the worker
  event loop, allowing cancellation messages and control requests to run. A chain of already
  resolved promises is not a reliable task-queue yield.

Propose cooperative scheduling as the browser baseline, with atomic cancellation where shared
memory is supported and permitted. Do not make cross-origin isolation a silent prerequisite
for the ordinary browser package. The feature matrix must specify both paths and their tested
cancellation latency. Page/crypto awaits alone cannot bound latency for an entirely resident
CPU-heavy query; parser/planner and long scalar kernels also need bounded checkpoints.

Cancellation must preserve the existing statement/transaction error rules. During commit, do
not interrupt an irreversible storage sequence and report a clean rollback: define the last
safe cancellation point and finish or recover the storage transition afterward. Worker
termination is a crash path, not ordinary statement cancellation. Checkpoint scheduling changes
physical time, not the logical cost accrued by completed equivalent work; canceled work may
stop at different prefixes, as with existing native cancellation.

## 7. Internal suspension and encrypted pages

Keep expression evaluation and resident row/page work synchronous between explicit checkpoints.
Prototype a resumable execution contract at cache misses, page-service waits, bounded CPU
quanta and commit crypto. Possible representations include explicit continuations or a step
driver; select one after inspecting pager/tree/lazy-decode call sites. Avoid a promise per row
or expression and avoid maintaining duplicate sync/async operator implementations.

Resume each operation exactly once. Never recover from a cache miss by rerunning SQL: that
could repeat entropy, host-function effects, cost or staged writes. Do not preload the whole
database to avoid suspension. Async page loads/prefetch and pending crypto buffers must stay
bounded, including retained pins and historical derived-key caches.

The codec remains core-owned above the byte-device seam; adapters supply the selected primitive
provider. Node native AES and browser WebCrypto must match fixed-input byte fixtures. Choosing
an async API does not itself select AES, a KDF or production dependencies.

For encrypted commits, batch independent page encryption within a memory/concurrency budget.
Build manifests over the final encoded bytes and preserve validated COW's body, manifest,
alternate-meta and one-barrier ordering, including the existing storage-failure poisoning
rules. Publish/resolve commit only at the completion point allowed by its selected durability
mode. Abort/retry must consume nonce allocations as specified by encryption.md. Crypto errors
and cancellation before storage work must be distinguished from failures after writes begin.

## 8. Rust delivery and host extensions

Retain native TS as a possible production choice on both platforms as well as the independent
conformance core. Re-evaluate the older browser-WASM shipping preference against the new data;
this proposal does not silently replace that standing preference or select a new default.
Keep Node/browser platform entry points separate so browser packages cannot import Node modules.

The current whole-engine WASM experiment is Node/WASI, not a ready browser/OPFS package.
A browser Rust/WASM delivery needs storage, entropy, clock, typed-value and async-bridge work.
Software WASM crypto does not inherit native AES acceleration. A WebCrypto bridge through
JSPI or a helper worker needs separate support, fallback and performance validation (see
encryption.md §8.1). An async public interface accommodates either bridge without guaranteeing
its internal implementation. The Node-native package likewise needs API parity and off-thread
dispatch before it can implement the proposed surface.

Select an engine backend explicitly at open, and keep it fixed for the handle/transaction.
Require common SQL, cost, error and file contracts; expose differences in extension support
and deployment requirements. Neither WASM nor native Rust is an unconditional performance win.

Host functions are a material migration question. Arbitrary application closures cannot be
transferred to a worker. Prefer explicit worker-local extension registration; define how trusted
host code is loaded and given capabilities before shipping it. Calls back to the application
thread would need a deliberate async RPC/reentrancy contract and measurements. Do not claim
transparent compatibility or quietly run SQL back on the main thread. Host extensions remain
outside built-in safety/determinism guarantees; the codec and built-ins gain no arbitrary host access.

## 9. Sequence and acceptance gates

1. Specify the async lifecycle, typed wire contract, ordering, bounded admission and error model;
   prototype Node execution-worker and browser scheduler adapters over the existing TS core.
2. Add batched results/prepared execution, cleanup and failure handling. Measure direct-core
   versus public API latency before deciding transport defaults or worker pooling.
3. Add cooperative checkpoints and cancellation, then explicit page/crypto suspension. Test
   resident CPU work as well as actual I/O waits. Keep each semantic change corpus-driven.
4. Prototype the chosen encryption protocol across cores and browser/Node providers, with
   fixed-input fixtures and crash tests, before choosing the production suite/backend.
5. Make a separate API migration and packaging decision, updating user docs/examples/live
   panels alongside the implementation. Add parallel readers or Rust delivery only on evidence.

Validation must include shared concurrency corpus coverage for ordering, writer waiters,
visibility and failure semantics; per-host tests for typed transport, queue limits, resource
release, cancellation, worker death and ambiguous commit replies; and existing cross-core
SQL/cost/file fixtures. Use real browser OPFS tests for the browser path, not Node/WASI proxies.

Measure cheap calls and batches, large typed results, warm/cold reads, durable writes, worker
startup and memory, event-loop responsiveness, p95/p99 latency, cancellation delay and peak
queue memory. Compare TS, WASM and native Rust at equivalent public boundaries on supported
browser/Node versions and ARM as well as x86. Re-run size measurements for complete packages.
The existing primitive and synchronous SQL benchmarks are baseline evidence only.

The first [Node async boundary report](../../bench/ts/results/2026-10-18-async/report.md)
measures direct versus worker latency and bounded result delivery. On that x86 host, prepared
execution had medians of 5.61/24.11 µs and a 100-statement batch 547/1,060 µs. A resident
aggregate blocked the direct caller's timer for about 90 ms, while the worker caller's largest
gap was 2.28 ms. These are warm in-memory observations, not file/browser or ARM conclusions.

Section 10 resolves API/migration policy, wire encoding, initial per-handle workers, limits,
admission errors and Node extension registration. Remaining decisions include cooperative
continuation representation and checkpoint frequency, async page/crypto providers, worker pools,
broader platform measurements and optional engine-package defaults. A public synchronous
application interface would still need a concrete use case and evidence.

## 10. Implemented async host contract

`jed-ts` exports the Node worker API; `jed-ts/browser` exports the browser worker API.
Both expose `createDatabase(options)` and `openDatabase(path, options)` returning an
`AsyncDatabase`. `backend` is fixed to `typescript` (the default); another value fails
with `UNSUPPORTED`. A path selects a Node file or an OPFS name; create without a path
selects the same page-backed in-memory host. `jed-ts/internal` and repository-relative
core modules retain synchronous entry points for conformance and existing host tooling;
they are not the application entry point. Rust and Go APIs are unchanged.

The database offers async `session`, `readSession`, `query`, `execute`, `prepare`,
`batch`, `executeScript`, `view`, `update`, and `close`. Database conveniences use a fresh
session per operation; a batch uses one transient session. Explicit sessions retain temp
tables, SQL transaction state, settings and session variables. `begin(writable)` returns a
transaction proxy with its own worker handle token: unrelated calls on its parent session
fail `BUSY` until that transaction ends. A transaction callback awaits application work
without surrendering ownership. `view`/`update` roll back on exceptions and commit on
success. Do not await a competing writer from inside the callback that holds the writer.

`query(sql, Value[], {signal})` and prepared `query(Value[], {signal})` return async,
single-pass rows. Column names/types and command count are returned at open; cost is final
after drain. `next()` fetches only on demand, `return()`/early loop exit and `close()` release
the cursor, and `Symbol.asyncDispose` supports explicit resource management. `execute`
drains/discards results and retains the command tag. `run/get/all` accept the existing JS
parameter mapping and object rows; `all` is a bounded collector. Prepared statements are
worker-owned, reusable on their originating handle, and have async `close`/disposal.
`batch` executes statements or prepared bindings in order, stopping on the first error.
It introduces no transaction: preceding autocommits stay committed, while statements in
an explicit block share that block. `executeScript` retains the existing atomic script rule.

Requests carry protocol version 1, strictly increasing request IDs, a session ID, an
optional transaction ID and operation-specific data. Values carry their own version 1
tag and an explicit recursive kind. Structured clone carries exact bigints; floats use IEEE
bit payloads (preserving NaN payloads, infinities and negative zero). Dedicated byte arrays
are copied; decimal
sign/scale/base-10000 limbs are rehydrated as `Decimal`. Array shape/lower bounds, composite
fields, ranges and JSONB numbers recurse through the same contract. Unfetched storage
references, cyclic/malformed values, arbitrary prototypes and closures are rejected.
This wire version is independent of the database file version. Engine errors retain
SQLSTATE and all public structured fields; diagnostic text has a finite transport limit.

Runtime limits are host budgets, never SQL cost units or heap-size guarantees. Defaults:

| Limit | Default |
|---|---:|
| outstanding ordinary requests | 128 |
| outstanding request payload | 8 MiB |
| rows per fetch | 128 |
| bytes per result batch, including overhead | 256 KiB |
| client-held result batches / `all` collector | 2 MiB |
| explicit sessions / cursors | 64 each |
| prepared statements | 256 |
| execution workers per application isolate | 32 |

Options under `runtime` override these positive finite limits. Inputs are preflighted
before wire copying; result delivery and client decoding operate in bounded batches.
Multiple concurrent `next` calls are serialized and bounded. A row larger than a batch
fails `RESULT_TOO_LARGE` and closes its cursor. Exhausted queue/result credits fail
`QUEUE_FULL`; the caller can consume/close existing work before submitting more. Control
requests have a separate finite admission reserve. The worker independently bounds its
queue and handles. Blocking operators still obey the core's query-memory/spill policy;
transport batching does not make every executor operator streaming.

Session operations retain submission order. The scheduler parses a waiting statement once,
then parks it before execution if another local session owns the writer. It services other
sessions, including the owner releasing its gate, without retrying executed SQL. Writer
waits honor session lock timeout and cancellation. Each batch statement and fetch is an
event-loop task turn. This provides inter-operation scheduling, not CPU-parallel reads.
File page-service waits remain inside the execution worker and preserve owner-only reuse.

`close` stops ordinary admission, rejects queued operations, completes an already running
operation/open, closes cursors/statements, rolls back open transactions, and closes storage
before terminating the worker. Close is idempotent. Session close follows the same policy
within that session. A worker error, undecodable reply or unexpected Node worker exit
rejects outstanding operations and invalidates all handles with `WORKER_FAILED` and
`outcome: "unknown"`; a write might have committed before the reply was lost. Requests are
never replayed. A result-size failure can also follow an autocommitted statement with
`RETURNING`; a transport error does not imply rollback. Other host codes are `CLOSED`, `BUSY`, `QUEUE_FULL`, `RESULT_TOO_LARGE`,
`WORKER_LIMIT`, `PROTOCOL` and `UNSUPPORTED`; these are not SQLSTATEs.

`AbortSignal` is mapped to a shared atomic flag on Node, and on cross-origin-isolated
browsers where available. Parser/resolver and meter checkpoints poll it during synchronous
work without changing cost. Cancellation uses `57014` and preserves failed-block semantics.
The last safe commit checkpoint precedes storage/publication; once that boundary passes,
commit completes and reports its real outcome even if the signal changes. Worker termination
is never ordinary cancellation. A browser without shared memory still works and processes
cancellation between operations/fetches. **Its synchronous resident query, blocking operator,
or long scalar kernel cannot currently be interrupted by a cancel message.** Full cooperative
continuations, bounded latency across every kernel, async cache faults and WebCrypto commits
remain unimplemented. No SQL replay or whole-file preload substitutes for those features.

Data-only Unicode/time-zone bundles can be provided at open. Node additionally accepts a
trusted `extensionModule` file URL exporting `configure(EnvironmentBuilder)`; it loads and
registers host functions inside the execution worker before opening storage. Arbitrary
application closures cannot cross the port. There is no application-thread callback RPC.
Host extensions retain their existing safety/determinism exclusion. Browser applications
needing custom host code supply a custom worker build; no dynamic browser extension loader
is promised by this package.
