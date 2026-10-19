# TypeScript runtime for browser and Node

> **Async API and worker runtime implemented.** The package entry points now use the
> independent TS core in dedicated Node/browser workers. Lifecycle, typed transport, scheduling,
> batching, cleanup and cancellation are specified in §10. Retained continuations now allow
> asynchronous data-page reads and cooperative cancellation at logical page boundaries and common
> execution loops without shared memory. The synchronous core remains internal for conformance
> and host tooling and drives the same algorithms. Cancellation is best effort; storage lifecycle,
> commit and crypto still use synchronous boundaries. No production encryption
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

The [elapsed-scheduling investigation](../../bench/ts/results/2026-10-19-scheduling/report.md)
retains 512-row/256 KiB batches and the existing codec, and selects a 1 ms elapsed slice checked
at existing 256-unit checkpoints. An initial interleaved comparison evaluates baseline, 4 ms
scheduling and a combined codec prototype; six later runs tune the slice to 1 ms. Relative to
the earlier baseline process medians, aggregate worker time falls from 328 to 240 ms in Chromium
and 314 to 284 ms in Node. Typed-output time is about flat in Chromium; Node rises 3.7% alongside
a 3.5% direct-reference rise, leaving worker/direct overhead essentially unchanged. The fused
codec was reverted after mixed results, including a 9% Node bulk regression. These are separate
process/isolate observations, not evidence that async execution is intrinsically faster; the
1 ms scheduling slice does not guarantee a maximum cancellation delay. In a separate rotated
cancellation comparison, ordinary Chromium typed-output abort-to-rejection rises from 13.1 to
21.1 ms by median of process medians (observed maximum 33.9 to 58.4 ms). All measured operations
cancel with `57014` and leave the session reusable, but this is a throughput/cancellation tradeoff.

Section 10 resolves API/migration policy, wire encoding, initial per-handle workers, limits,
admission errors, retained continuations and Node extension registration. Remaining decisions
include async storage lifecycle/commit and crypto providers, worker pools,
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
recursive queries and common scan/join/group loops. Every 256 checkpoint visits the driver checks
a monotonic scheduling clock and yields when its 1 ms execution slice has elapsed. Actual I/O
retains its suspension points; a new slice starts after a task yield. Awaited I/O retains the
deadline so immediately resolved reads cannot starve control messages. The clock
is physical scheduler state, separate from SQL's injected clock, values and cost. Neither the
checkpoint interval nor the slice is a latency guarantee: synchronous kernels may exceed it.
Control messages can mark
the active operation canceled, but another engine operation cannot run until it completes or
unwinds. Session scopes, writer ownership and cursor pins therefore remain valid across awaits.

The page seam has an optional asynchronous byte read. Node file reads use it; a host exposing
only synchronous reads (including OPFS sync handles) receives a task yield before the read.
Cache insertion happens only after a successful resume, with invalidation guarding suspended
loads. Expression traversal propagates lazy overflow reads and subquery suspension without
replaying expressions or host calls. Open/recovery, DDL/statistics maintenance, spill device I/O,
commit serialization/writes,
and page-service coordination still have synchronous paths inside the execution worker. This
slice does not yet make an async-only byte device or WebCrypto provider sufficient for every
storage operation.

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
maximum latency**. Further pause points require evidence of a material delay. Async-only storage
lifecycle and WebCrypto commits remain unimplemented. No SQL replay or whole-file preload
substitutes for those features.

Data-only Unicode/time-zone bundles can be provided at open. Node additionally accepts a
trusted `extensionModule` file URL exporting `configure(EnvironmentBuilder)`; it loads and
registers host functions inside the execution worker before opening storage. Arbitrary
application closures cannot cross the port. There is no application-thread callback RPC.
Host extensions retain their existing safety/determinism exclusion. Browser applications
needing custom host code supply a custom worker build; no dynamic browser extension loader
is promised by this package.
