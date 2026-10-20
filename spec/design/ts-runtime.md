# TypeScript runtime for browser and Node

> **Async API and worker runtime implemented.** The package entry points now use the
> independent TS core in dedicated Node/browser workers. Lifecycle, typed transport, scheduling,
> batching, cleanup and cancellation are specified in §10. Retained continuations now allow
> asynchronous data-page reads and cooperative cancellation at logical page boundaries and common
> execution loops without shared memory. The synchronous core remains internal for conformance
> and host tooling and drives the same algorithms. Cancellation is best effort. Commit,
> open/recovery and storage maintenance are continuations too (§11): they await asynchronous
> page-codec or device stages and run inline over today's synchronous hosts. No production encryption
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

- A shared atomic flag written by the caller and polled at the existing engine checkpoints.
- Cooperative execution that retains its continuation and yields periodically to the worker
  event loop, allowing cancellation messages and control requests to run. A chain of already
  resolved promises is not a reliable task-queue yield.

Cooperative scheduling at page boundaries and common execution loops is the browser baseline,
with atomic cancellation where shared memory is supported and permitted. A logical page boundary
includes cached and resident leaves: a warm scan must periodically yield even when it performs no
device I/O. Generated rows, recursive iterations and common join/group loops use the same
suspension mechanism. Do not make cross-origin isolation a silent prerequisite for the ordinary
browser package.

Cancellation is **best effort**, with no guaranteed maximum latency. Sorting, parsing/planning,
and long scalar kernels remain synchronous; a cancel message may wait for such a phase to finish.
Do not replace those algorithms merely to provide more pause points. Measure actual delays before
extending cooperative coverage. Cost/resource limits remain independent of cancellation, and
host extensions remain outside the built-in guarantees. Execution already lives in a dedicated
worker so a long kernel does not block the application thread; terminating that worker is still
a crash, with the storage recovery and uncertain-outcome rules below.

Cancellation must preserve the existing statement/transaction error rules. During commit, do
not interrupt an irreversible storage sequence and report a clean rollback: define the last
safe cancellation point and finish or recover the storage transition afterward. Worker
termination is a crash path, not ordinary statement cancellation. Checkpoint scheduling changes
physical time, not the logical cost accrued by completed equivalent work; canceled work may
stop at different prefixes, as with existing native cancellation.

## 7. Internal suspension and encrypted pages

Keep scalar kernels and resident work synchronous between explicit checkpoints. Represent
suspendable execution with retained generators that yield page I/O or a scheduling checkpoint.
The immediate-expression prototype returns ordinary values for common resident expressions,
creating continuations only when their operands suspend. One semantic dispatch and shared scalar
kernels serve both immediate and suspended evaluation; uncommon expression forms may still use
generators. A pending operand retains previously evaluated operands and the next evaluation index,
including short-circuit decisions, rather than replaying an expression. Synchronous internal APIs
and the async runtime drive the same authored execution path; expression traversal must propagate
a lazy-value page fault or subquery suspension. Avoid a promise or generator per resident expression
node and avoid maintaining duplicate sync/async operator implementations. Re-enter operation-local
cancellation and planning scopes for each async resume; never leave a synchronous dynamic scope
installed across an await. A synchronous drain retains its ambient planning scope and installs
cancellation tracking once; its recursive-call stack is allocated only when a call effect occurs.
Direct cursor iteration avoids redundant adapter generators without prefetching another row.
Recursive query delegation uses the driver's continuation stack so the existing nesting/stack
limits hold. The driver serializes engine operations across suspension while still receiving
cancellation and queueing controls.

Cancellation checks belong at logical page traversal and row/operator progress boundaries, not
inside the immediate-expression traversal. Physical device reads and final result emission alone
are insufficient: a cached scan can reject every row, and blocking operators, generated inputs,
recursive queries and pending-write loops can run for a long time without either event. Retain
sparse progress checkpoints for those cases. Actual task-yield frequency and the cost of retaining
continuations are separate measurements; increasing the scheduling quantum does not remove
per-expression or per-row driver overhead.

Resume each operation exactly once. Never recover from a cache miss by rerunning SQL: that
could repeat entropy, host-function effects, cost or staged writes. Do not preload the whole
database to avoid suspension. Async page loads/prefetch and pending crypto buffers must stay
bounded, including retained pins (per-page keys need no key cache, encryption.md §3.1).

The codec remains core-owned above the byte-device seam; adapters supply the selected primitive
provider. Node native AES and browser WebCrypto must match fixed-input byte fixtures. Choosing
an async API does not itself select AES, a KDF or production dependencies.

For encrypted commits, batch independent page encryption within a memory/concurrency budget.
Build manifests over the final encoded bytes and preserve validated COW's body, manifest,
alternate-meta and one-barrier ordering, including the existing storage-failure poisoning
rules. Publish/resolve commit only at the completion point allowed by its selected durability
mode. Abort/retry needs no nonce bookkeeping: every seal draws a fresh random salt and a
failed attempt's salts are simply never reused (encryption.md §3.1). Crypto errors
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
transferred to a worker. Registration is worker-local: Node loads an explicit extension module,
and browser applications supply a worker using the public startup helper (§10). Calls back to the application
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

The [continuation follow-up](../../bench/ts/results/2026-10-18-continuations/report.md)
measures the added scheduling machinery on the same host. Worker prepared-call medians were
27.69 µs and 100-statement batches 1,562.63 µs (about 15% and 47% above the initial bridge run).
The worker aggregate kept the application's largest timer gap at 2.62 ms. The retained-generator
path has measurable execution overhead; these separate warm runs do not isolate individual
changes or establish cancellation latency for synchronous kernels.

The [immediate-expression prototype](../../bench/ts/results/2026-10-18-immediate/report.md)
compares three runs per variant with longer warmup and fixed CPU affinity. Relative to the retained
generator implementation, the 4,096-row typed result's median execution time falls about 47% in
the direct core and 17% through the worker. Small calls show limited changes; direct prepared
execution is about 1 µs slower. This supports avoiding per-expression continuation machinery,
without claiming a universal gain or complete recovery of the original synchronous throughput.

The [packed-result follow-up](../../bench/ts/results/2026-10-18-packed/report.md) isolates result
delivery changes from the unchanged executor. Across three process runs per variant, the same
4,096-row worker result falls from 29.97 to 9.25 ms (69% lower latency); the direct core remains
around 5 ms. Cheap worker queries are about 4 µs slower, while prepared calls and batches show
small changes within wider process variation. These warm Node measurements support transferring
compact batches while retaining the existing row/byte limits, without claiming a general
improvement in query execution or browser throughput.

The [heavy-query browser/Node report](../../bench/ts/results/2026-10-18-heavy/report.md) measures
queries taking hundreds of milliseconds, with three process runs per host. Ordinary Chromium
without shared memory has direct/worker medians of 281/320 ms for a million-value decimal sum,
323/364 ms for a resident grouped join, and 340/372 ms for top-N ordering: about 9–14% overhead.
Isolated Chromium has about 10–29% overhead for those cases. Generating, ordering and consuming
200,000 typed rows remains substantially more expensive: 362/807 ms in ordinary Chromium and
376/754 ms in isolated Chromium. The current direct and worker paths share the same engine;
these measurements do not isolate historical continuation overhead.

Across both browser modes, all 72 worker responsiveness observations record no main-thread long
tasks, while every direct observation records one and no animation callbacks during execution.
Median largest frame gaps are about 19–21 ms through workers versus 279–386 ms directly;
the worst observed worker frame gap is 24 ms. This supports worker isolation for expensive
queries even without shared memory. It does not establish cancellation latency or the need for
extra pause points: no measured query passes an `AbortSignal`. This is warm in-memory,
headless Chromium on x64; OPFS, other browser engines, ARM and application rendering remain
separate measurements.

The [result-batching investigation](../../bench/ts/results/2026-10-18-result-batching/report.md)
raises the default row cap from 128 to 512 within the existing byte limits. Three controlled
runs per host/variant reduce the 200,000-row worker median from 735 to 676 ms in Chromium (8%)
and 643 to 506 ms in Node (21%). Browser frame gaps remain about 20 ms at the median observation
maximum, with 26 ms worst observed after the change and no worker main-thread long tasks.
Per-value conversion still costs substantial time; codec prototypes did not establish a gain
and were reverted. Larger configurable byte batches remain a throughput/responsiveness and
concurrent-result-reservation tradeoff, not a new default.

The [elapsed-scheduling experiment](../../bench/ts/results/2026-10-19-scheduling/report.md)
was subsequently [reverted](../../bench/ts/results/2026-10-19-scheduling-rollback/report.md).
Neither the 1 ms slice nor a shorter 0.125 ms prototype preserved ordinary-browser cancellation
latency in repeated tests. The runtime retains a task yield every 256 checkpoint visits, with
the existing 512-row/256 KiB transport limits and packed codec. Shared-memory result transport
is not implemented; only cancellation flags use shared memory where available. The rollback
report also records current platform support and possible future transport experiments.

The [profile-guided delivery experiment](../../bench/ts/results/2026-10-19-delivery-optimization/report.md)
retains direct ASCII scanning for successful SQL hex decoding. For 200,000 typed output rows,
worker time falls from 539 to 505 ms in Node and 666 to 461 ms in ordinary Chromium, while the
direct core improves too; this does not resolve the transport overhead. A cached byte-view codec
prototype and immediate idle-worker dispatch add only about 1% over hex alone in repeated runs,
with larger observed Node timer gaps, and are reverted. Batch limits, packed validation and
256-checkpoint task yields remain unchanged. The report preserves the variants and raw evidence.

The [integrated async-codec report](../../bench/aead/results/2026-10-19-async-codec.md)
measures §11's write side with measurement-only codecs over real OPFS. Each codec pays real
WebCrypto AES-GCM or synchronous pure-JS XChaCha per page and returns the plaintext, so file bytes
are verified unchanged. Across five fresh headless-Chromium runs, an async identity codec cost about
0.1–0.2 ms per small commit, within process-run noise. AES-GCM added about 7–9 µs per page and
XChaCha about 40–46 µs. Chromium's worker WebCrypto executes inside the call and resolves without a
task boundary. So the 16-page encode window gave no concurrency (windows 1, 4 and 16 performed
alike), and awaiting it did not make the worker responsive: commits and opens admitted no control
message with or without a codec, while scans did. A codec-side task turn every 16 operations
restored admission during open (max ping round trip about 16 ms vs about 110–145 ms). Large commits
still blocked about 119 ms on synchronous serialization and writes. The window stays as a bound for
hosts that do overlap. Whether long commits and opens need engine-side yield points is an open,
suite-independent question. No suite or provider is chosen.

Section 10 resolves API/migration policy, wire encoding, initial per-handle workers, limits,
admission errors, retained continuations and Node/browser extension registration; §11 resolves
the asynchronous write side. Remaining decisions include crypto providers, worker pools,
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

Session limit setters use `await session.configure({...})`. The initial async surface does not
yet expose compaction, changing per-database storage limits after open, the storage-byte gauge,
or resetting an individual custom session variable. `maxStorageBytes` is accepted at create/open;
new sessions start without custom variables and can initialize them through `configure({vars})`.
These remain host API follow-ons, not methods
applications can import from the public async entry points today.

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

Requests carry protocol version 2, strictly increasing request IDs, a session ID, an
optional transaction ID and operation-specific data. Values carry their own version 1
tag and an explicit recursive kind. Structured clone carries exact bigints; floats use IEEE
bit payloads (preserving NaN payloads, infinities and negative zero). Dedicated byte arrays
are copied; decimal
sign/scale/base-10000 limbs are rehydrated as `Decimal`. Array shape/lower bounds, composite
fields, ranges and JSONB numbers recurse through the same contract. Unfetched storage
references, cyclic/malformed values, arbitrary prototypes and closures are rejected.
This wire version is independent of the database file version. Engine errors retain
SQLSTATE and all public structured fields; diagnostic text has a finite transport limit.

Result batches use a separate versioned binary encoding in an owned `ArrayBuffer`, transferred
between worker and caller. This avoids structured-cloning a recursive object graph for every
row. Request parameters retain the object value encoding above. Packing preflights each row
against the same conservative transport accounting before copying it; decoding bounds both
the binary payload and its expanded values before exposing rows. UTF-16 code units, exact
integers/decimals, float bits and recursive value metadata remain lossless. Result values own
their mutable bytes and arrays. The protocol version change rejects mismatched worker/client
bundles; it does not change file bytes, SQL cost, result batch limits or cursor pull semantics.
Buffered row iteration returns a resolved promise directly when no earlier pull is pending;
concurrent pulls and close retain ordered, bounded admission.

The packed result envelope is `{version: 1, length: rowCount, data: ArrayBuffer}`.
Its buffer starts with a `u32` row count matching the envelope, followed by each row's
`u32` column count and tagged values. Multi-byte fields are little-endian. Value tags:

| Tag | Kind | Body |
|---:|---|---|
| 0 | NULL | empty |
| 1 | integer | signed 64-bit integer |
| 2 | boolean | byte 0 or 1 |
| 3 | date | signed 64-bit day count |
| 4, 5 | timestamp, timestamptz | signed 64-bit microseconds |
| 6 | interval | months and days as exact safe integers in f64 fields, then signed 64-bit microseconds |
| 7, 8, 9 | text, json, jsonpath | `u32` UTF-16 unit count, then `u16` units |
| 10 | decimal | sign byte, `u32` scale, `u32` limb count, then little-endian base-10000 `u16` limbs |
| 11, 12 | f32, f64 | 4 or 8 IEEE bytes |
| 13 | bytea | `u32` byte count, then bytes |
| 14 | uuid | 16 bytes |
| 15 | composite | `u32` field count, then tagged values |
| 16 | array | dimension count byte; each length/lower-bound pair as exact safe integers in f64 fields; then tagged elements |
| 17 | range | flag byte: empty=1, lower inclusive=2, upper inclusive=4, lower present=8, upper present=16; then present tagged bounds |
| 18 | jsonb | tagged JSON node |

Array dimensions are limited to six, with positive lengths and their exact product determining
the element count; zero dimensions means zero elements. JSON node tags are NULL=0, boolean=1,
decimal number=2, string=3, array=4 and object=5. Scalars use the bodies above; an array uses
a `u32` element count followed by nodes, and an object uses a `u32` member count followed by
string keys and nodes. Decoder validation matches the object wire's depth, decimal and shape
constraints. Unknown tags, invalid flags/counts, truncation and trailing bytes are errors.

Runtime limits are host budgets, never SQL cost units or heap-size guarantees. Defaults:

| Limit | Default |
|---|---:|
| outstanding ordinary requests | 128 |
| outstanding request payload | 8 MiB |
| rows per fetch | 512 |
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

The row limit and byte limit apply together. The 512-row default amortizes fetch messages while
retaining the 256 KiB batch and 2 MiB held-result allowances. A fetch evaluates its whole batch
before delivering the first row: larger batches can delay that row, surface a later SQL error
earlier in iteration, and do more work that an early close discards. Applications needing a
smaller evaluation prefix can set `runtime.maxBatchRows` to 128 or 1. This does not introduce
speculative fetches; another batch still requires caller demand. Increasing `maxBatchBytes`
also increases each fetch's reservation against `maxResultBytes`, reducing the number of
simultaneously held batches unless that total allowance is raised separately.

Session operations retain submission order. The scheduler parses a waiting statement once,
then parks it before execution if another local session owns the writer. It services other
sessions, including the owner releasing its gate, without retrying executed SQL. Writer
waits honor session lock timeout and cancellation. Each batch statement and fetch is an
event-loop task turn. This provides inter-operation scheduling, not CPU-parallel reads.
File page-service waits remain inside the execution worker and preserve owner-only reuse.
Within an operation, the shared generator driver retains execution state across data-page reads
and periodically yields a real task turn for cached/resident page access, row generation,
recursive queries and common scan/join/group loops. The initial physical quantum is 256
checkpoint visits; it is not a SQL cost unit or a latency guarantee. Control messages can mark
the active operation canceled, but another engine operation cannot run until it completes or
unwinds. Session scopes, writer ownership and cursor pins therefore remain valid across awaits.

The page seam has an optional asynchronous byte read. Node file reads use it; a host exposing
only synchronous reads (including OPFS sync handles) receives a task yield before the read.
Cache insertion happens only after a successful resume, with invalidation guarding suspended
loads. Expression traversal propagates lazy overflow reads and subquery suspension without
replaying expressions or host calls. Every statement data read awaits on this path: query and DML
scans, the DDL that reads rows (`CREATE INDEX`/`REINDEX`, `ALTER TABLE` rewrite and validation,
`CREATE TABLE … AS`) and `ANALYZE` (§11.1). Commit, open/recovery and storage maintenance follow §11.
Spill device I/O, temporary domains, compaction and page-service coordination keep synchronous
paths inside the execution worker (§11.1).

`close` stops ordinary admission, rejects queued operations, requests cancellation of an active
operation and waits for its cleanup (or completion past the commit boundary), completes an active
open, closes cursors/statements, rolls back open transactions, and closes storage
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
is never ordinary cancellation. A browser without shared memory processes cancellation at the
same cooperative page and execution-loop boundaries, including resident scans. Sorting,
parsing/planning and long scalar kernels remain synchronous: **cancellation has no guaranteed
maximum latency**. Further pause points require evidence of a material delay. The commit
transition past that boundary runs as an uncancelable continuation (§11.2). No WebCrypto codec
is implemented; no SQL replay or whole-file preload substitutes for one.

Data-only Unicode/time-zone bundles can be provided at open. Node additionally accepts a
trusted `extensionModule` file URL exporting `configure(EnvironmentBuilder)`; it loads and
registers host functions inside the execution worker before opening storage.

Browser `createDatabase` and `openDatabase` accept an optional `workerFactory: () => Worker`.
The factory runs in the calling thread and is omitted from the wire options. It must return a
fresh, dedicated worker for that database handle; returning a worker already claimed by jed
fails `PROTOCOL`, including after its original handle closed. Jed owns the returned worker,
terminating it after database close or initialization failure. A failed reuse attempt does not
terminate the original handle's worker. The default factory creates the package's ordinary
worker with no host registrations.

The public `jed-ts/browser/worker` entry point exports
`startDatabaseWorker({ configure? })`, called once at the top level of the application's
worker module. It installs the message listener synchronously. On opening the database, it
loads the data bundles and calls `configure(builder)` once, awaiting a returned promise before
building the environment or acquiring storage. A configuration failure rejects the opening
request and the client releases the worker. The entry point also exports `EnvironmentBuilder`,
the `HostFunctionSpec`, `HostKernel`, `HostBatchKernel` and `Value` types, and the `intValue`
and `Decimal` value helpers. This lets registration modules use a public, browser-safe import;
Node's `extensionModule` may reuse the same `configure` export when its other imports support
both hosts. Browser extensions are statically bundled with the application worker; the package
does not dynamically load browser extension modules.

Arbitrary application closures cannot cross the port. There is no application-thread callback
RPC. Worker-local closures can retain state, and changing application inputs can be passed as
SQL parameters. Configuration may be asynchronous; scalar and batch SQL kernels remain
synchronous. Host extensions retain their existing safety/determinism exclusion.

## 11. Asynchronous write-side storage

The read side (§10) suspends query execution at data-page reads. This section extends the same
retained-continuation driver to the write side: commit, open/recovery and storage maintenance.
The goal is that a page codec or byte device with **asynchronous-only** operations — the browser
WebCrypto case of [encryption.md](encryption.md) §8.1 — is sufficient for commit, recovery and
maintenance (the exceptions are listed in §11.1), without SQL replay, whole-file preload, or a
second implementation of any algorithm.
No codec, cipher suite, nonce protocol or format change is selected here; file bytes, results
and cost are unchanged.

### 11.1 Which paths suspend

Each path below is one authored generator (`Steps`) driven by `runSync` for the internal
synchronous API and conformance harness, and by `runAsync` in the execution worker:

| Path | Suspension points |
|---|---|
| Commit (main domain, durable file attachment) | stabilization barrier, growth (`setSize`), each body/free-list/manifest/meta page write and its encoding, the final barrier, reads of the just-written catalog and of orphaned leaves for the live-page delta |
| Relaxed batches (`synchronous = off`) | the cap flush after a commit, and the flush in an async database close |
| Open/recovery | page-size header, both meta slots, manifest overflow chain and every listed dependency, catalog chain, interior skeletons, eager GiST R-trees, the free-list chain, keyless rowid reconstruction |
| Maintenance | the in-commit free-list rebuild (reachability walk, including overflow chains), trailing truncation after the barrier, the live-count verification walk |
| Commit serialization | deferred overflow chains a rewritten record must re-read (see below) |
| Post-commit residency flip | the re-fault of a root leaf whose rewritten rows moved to fresh overflow chains |
| Statement reads (queries, DML, DDL, statistics) | every leaf fault and overflow-chain read: query and DML scans and probes (§10), the `CREATE INDEX`/`REINDEX` build scan, `ALTER TABLE`'s rewrite and constraint validation (including the expressions, defaults, keys and error details it evaluates), `CREATE TABLE … AS`, and the `ANALYZE` column scan |

Serialization (`incrementalImage`) is pure, unmetered CPU work and is not rewritten as a
generator, which would add per-value overhead to every commit. Statement execution settles every
deferred value an UPDATE carries (large-values.md §15), so a stored row's deferred values always
match its own disposition plan and an incremental serialization does not read storage. The
encoder still has a defensive read for a deferred value whose plan differs. So that both drivers
produce the same bytes even then, an asynchronous write side (§11.4) serves those reads from a
commit-local chain cache: a missing chain stops the pass with a private miss signal, the pass's
page assignments are undone, the continuation awaits that whole chain, and serialization runs
again. The retry repeats only this commit's pure serialization — no SQL, entropy, cost,
host-function call or staged write — so it is not replay in the sense of §7. Each retry adds a
distinct chain, so passes are bounded by such chains. The cache lives only for the commit.

These remain synchronous and are not required by the browser storage stack:

- In-memory and temporary domains (a `MemoryBlockStore`, never encoded, no barrier) and the
  temporary-domain migration to a Node scratch file. Browser hosts have no spill target.
- Page-service paths (Node only): a page client's commit through the blocking channel, owner
  adoption of client commits and the owner's maintenance commit, election and promotion.
- Host compaction (`compactFile`, Node only and not on the async surface): its from-scratch
  serializer faults leaves through the synchronous store path. OPFS compaction is already
  `0A000` pending a crash-safe replace (hosts.md §7); an async compaction needs a generator
  from-scratch serializer and belongs with that design.
- `create`'s whole bootstrap image written in place (OPFS) or through a temp file (Node): an
  encrypted create must first define how bootstrap pages are encoded (encryption.md §6).
- Host backup (`backupForTesting`, Node only and not on the async surface): it walks the snapshot
  through the synchronous page read.
- The directory barrier (Node only) and spill device I/O (spill files are not encoded).

### 11.2 Ordering across awaits

The engine already serializes operations: while a continuation is suspended, the runtime admits
control messages but starts no other engine operation (§10). Writer ownership, the session's
snapshot pin and the reader watermark therefore stay exactly as they were at the await, and
release (`finishBlock`) still runs in `finally` after publication.

**Cancellation.** The last safe cancellation point is unchanged: `commitTx`'s checkpoint, before
any commit preparation. The rest of the commit transition — commit preparation, any durable
attachment commit inside `commitTx`, and `Session.publish` with its persist — runs as an
**uncancelable continuation** (`uncancelableSteps`): both drivers suppress meter polling for its
segments, and `runAsync` neither injects `57014` into it nor checks the cancel flag before or
after its effects. A cancel or close request arriving meanwhile is recorded; close still waits for
the commit to finish and reports its real outcome. Open is not cancelable (§10).

**Validated COW (validated-cow.md §1, §3, §4).** The async path performs the same steps in the
same order as the synchronous one, each awaited before the next begins:

1. Writer admission refuses a poisoned handle; serialization, admission under
   `max_storage_bytes` and the live-page delta (which may await orphan-leaf reads) run before
   any storage work. A failure here discards the working snapshot without poisoning.
2. The commit guard is armed, then any adopted generation is stabilized (an awaited barrier).
3. Growth, then body pages, then the free-list plan (which may await reads of the new catalog),
   free-list pages, manifest overflow pages, and finally the alternate meta slot. Encoding may
   overlap within a bounded window (§11.3), but **writes are issued strictly in the synchronous
   order**, one after another, and the meta write starts only after every earlier write resolved.
4. The final barrier is awaited (deferred under `synchronous = off`). Only then does the pager
   record the commit, the shrink run, the published root swap, and the page cache invalidate.

A rejection from any awaited write, barrier or encoding after step 2 leaves the guard armed: the
handle is poisoned until reopen, exactly like a synchronous I/O error. Encoding happens after the
guard is armed, so an encode failure is a post-guard failure. The codec reserves no per-commit
state: every seal draws a fresh random salt in the canonical order encryption.md §4 fixes, after
the pre-write checks and before sealing begins. A failed cap flush revokes the storage (§4 of
validated-cow.md); a failed shrink is ignored. Trailing truncation awaits its shrink only after
the barrier that covers the newest generation.

### 11.3 Bounded pending work

The write set itself is already materialized by the synchronous algorithm (the dirty pages and
the commit's manifest), so the async path adds only bounded physical buffers:

- **Encode window.** At most 16 pages are encoded concurrently, and at most 16 encoded pages wait
  for their write. The window is physical scheduling state, never SQL cost.
- **Validation window.** Recovery may read up to 16 listed dependencies ahead of the one it is
  checking. Results and errors are consumed in manifest order, so a read failure past the first
  invalid entry is never reported and selection is identical to the sequential algorithm.
- **Walks.** The catalog chain, skeletons, GiST trees, overflow chains and free-list chain are read
  one page at a time; each page is awaited before the next pointer is followed.
- **Chain cache.** Only during one commit's serialization, bounded by the stored length of the
  chains it actually re-reads (§11.1).

No path reads ahead outside these windows, and no path preloads the file.

### 11.4 Hosts and the write-side mode

The pager decides once per stage whether the write side suspends. It is **asynchronous** when its
page transform offers an asynchronous encode or decode, or its byte device offers an asynchronous
write, barrier or resize (`writeAtAsync`, `syncAsync`, `setSizeAsync`). Otherwise every write-side
step runs inline: no promise, no task yield and no change from the synchronous behavior. In the
asynchronous mode each stage prefers its asynchronous form and falls back to its synchronous one.
`size()` remains synchronous metadata on every device. Query data-page reads keep §10's policy.

- **Browser OPFS.** The sync access handle stays the synchronous device (hosts.md §5); the worker
  opens through the async loader and closes through an async flush. Today no transform is
  installed, so the write side is inline and OPFS commits are physically unchanged. A WebCrypto
  codec makes the mode asynchronous: each page is encoded (or decoded) by an awaited operation,
  then written to (or read from) the handle synchronously.
- **Node file host.** `FileBlockStore` keeps synchronous positioned writes and barriers, so its
  write side remains inline; its optional asynchronous read continues to serve query reads only.
  Node's native crypto provider is synchronous (§7), so a Node codec does not need the async
  mode, and the lock-bundle election and page-service owner open stay synchronous. Making Node
  writes awaitable would also let page-service event-loop handlers run on the engine thread during
  a commit; that interleaving would need its own analysis before any Node device opts in.
- **Memory and temporary domains.** Always inline (§11.1).

### 11.5 The page-codec seam

`PageTransform` (`impl/ts/src/pager.ts`) is the seam where the core-owned page codec plugs in,
above the byte device and below the page cache (hosts.md §6). It offers `encode`/`decode` and/or
`encodeAsync`/`decodeAsync`, keyed by page index. Its contract:

- `decode` returns exactly the logical page bytes `encode` received. Page CRCs, manifest
  digests, the live-page count and the page cache all operate on those plaintext bytes
  (validated-cow.md §5) in the plaintext format; the encrypted variant moves the manifest
  digests after `encode` and drops the body-page CRC (encryption.md §4), and carries the child's tag across the
  seam — `encode` returns it so the parent can bind it, `decode` takes the expected one from the
  pointer (encryption.md §4.1) — which are the two changes this seam takes with that format
  version. The cache inserts a decoded page only after its read resumed.
- `Pager.fromStore` reads the 12-byte bootstrap header (page size) directly from the device;
  the encrypted format must keep the page size discoverable before any key is used
  (encryption.md §4).
- A transform without a synchronous form fails the synchronous internal API with an explicit
  error rather than blocking; such a database is usable only through the async runtime.

Only identity transforms exist, in tests. They leave bytes unchanged and prove the suspension
points. In the Node unit suite (`tests/async_write_side.test.ts`) the device and transform reject
every synchronous call; commit, recovery and maintenance through the async driver produce files,
results and costs identical to a synchronous control, in both durability modes and through the
worker runtime. The real-browser suite (`e2e/opfs.spec.ts`) runs the same paths over a real OPFS
sync access handle with a transform that awaits SubtleCrypto for every page, again byte-identical
to a run without it.

The **async-codec conformance mode** (`mise run conformance:ts:async-codec`, part of
`conformance:all`; conformance.md §3.4) covers everything SQL reaches. It is the TS disk walk with
each per-record reopen through the async loader (`Pager.fromStoreSteps` + `loadEnginePagedSteps`)
under an identity transform with only `encodeAsync`/`decodeAsync`, and every record run through
`runAsync` (`Session.querySteps` and the cursor's retained continuation). Expected rows, errors
and costs are the plain disk mode's. It skips exactly the files the disk mode skips (reopen-fragile
session state, fixtures, concurrency schedules); no file is skipped for the async codec. The
paths above that stay synchronous are not reached: the harness creates its image with the
synchronous `create` before installing the codec, opens without a lock bundle (no page service),
never encodes a temporary domain, and runs no compaction or backup. Statement paths that read
pages synchronously had failed under it (index builds, `ANALYZE`, the post-commit root re-fault);
they now run as the continuations listed in §11.1.
