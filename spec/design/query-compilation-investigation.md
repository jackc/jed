# Query compilation in Rust, Go, and TypeScript

Investigation, 2026-10-19. This is evidence and a proposed experimental direction,
not a production architecture decision. No SQL, public API, cost schedule, core,
or dependency change. The isolated JS probes live in
[`bench/query-compilation`](../../bench/query-compilation/).

Follow-on proposals are now recorded independently for
[Rust](rust-expression-specialization.md), [Go](go-execution-specialization.md),
and [TypeScript](ts-expression-specialization.md), with a
[shared experiment contract](execution-specialization-experiments.md). The TS
proposal prioritizes closures/static kernels under strict CSP. These proposals
do not change the historical measurements or select a production backend.

## Findings

Query-specific code can help all three languages: compiling the database engine
does not mean the compiler knows the queries it will later receive. Rust and Go
currently execute a runtime expression tree too. The differences are how readily
each host can turn a newly received query into executable code, and what that
costs in packaging, startup, memory, and safety.

| Core | Closest analogue to Ruby's generated methods | Assessment for jed |
|---|---|---|
| TypeScript | Emit ordinary JS functions from typed plans; let the JS engine optimize them | Most direct experiment. Keep optional, with a complete path that generates no source. Compare against closures as well as the tree evaluator. |
| Rust | Lower typed expressions to native code with a backend such as Cranelift | Technically feasible, but adds a compiler, executable-memory management, and an unsafe execution boundary. First pursue the already promising selective typed VM and static kernels. |
| Go | A specialized executor built from precompiled functions/closures or typed instructions | Best fit under pure-Go and memory-safety constraints. There is no supported built-in runtime Go-source compiler. Native JIT/plugin approaches are a much larger departure. |

The best first target is a repeatedly evaluated scalar predicate/projection on a
scan, not point lookup, storage, or an entire replacement executor. Selective
fusion of existing row loops may follow if profiling shows it pays. Decimal,
JSON, collation, I/O, result allocation, joins and spills can dominate queries
that source generation barely helps.

## Evidence from the other experiments

Fetched and verified these remote branch tips before inspection:

- Ruby: `origin/codex/ruby-native-experiment`,
  `fc12f42798420fabc91dd6ee1274b961647071f8`.
- Expression VM: `origin/codex/expression-vm-experiment`,
  `a85f2127cc64b8440cf86c3604aaac0ec10a0676`.

The Ruby [compiler report](https://github.com/jackc/jed/blob/fc12f42798420fabc91dd6ee1274b961647071f8/impl/ruby-native/COMPILED_FOLLOWUP.md)
distinguishes isolated expression gains from whole-query gains. The initial
YJIT kernels improved 3.4–7.1×; prepared arithmetic filter/projection and CASE
queries improved 1.64×, 1.70×, and 2.02×. Generation also helped without a JIT.
The later [pipeline report](https://github.com/jackc/jed/blob/fc12f42798420fabc91dd6ee1274b961647071f8/impl/ruby-native/PIPELINE_FOLLOWUP.md)
found smaller incremental fusion gains: roughly 3–13% for eager scans and 1–6%
for ordinary lazy queries in its alternating controls. That report flags
historical correctness defects fixed by a subsequent follow-up. These are
reported historical results, not measurements rerun here or a language ranking.

The separate [VM report](https://github.com/jackc/jed/blob/a85f2127cc64b8440cf86c3604aaac0ec10a0676/spec/design/expression-vm-experiment.md)
is directly relevant. Its median paired file-query time changes were:

| Workload | Rust | Go | TypeScript |
|---|---:|---:|---:|
| Arithmetic scan | -16.8% | +15.8% | +0.7% |
| Boolean filter | -29.9% | +20.2% | +17.5% |
| CASE/COALESCE | -6.7% | +12.4% | +5.1% |
| Prepared point | +17.9% | +7.7% | +15.1% |

Negative means less time. Rust used compact typed registers but recompiled per
execution; Go's cache retained too much and TS still dispatched some generic
types. Those implementation choices matter. This is evidence for a targeted Rust
follow-up, not proof that every VM helps Rust or that Go/TS cannot benefit from
specialization. A JS source compiler removes dispatch that a JS opcode loop
still performs.

## Where compilation would fit

Production entry points inspected here are Rust `RExpr::eval` in
[`eval.rs`](../../impl/rust/src/executor/eval.rs), Go `(*rExpr).eval` in
[`eval.go`](../../impl/go/eval.go), and TS `evalExpr` in
[`eval.ts`](../../impl/ts/src/eval.ts). All guard cost, dispatch on node kind, and
recursively evaluate children. They already call specialized value kernels.

Preparation also already caches more than parsing where eligible. TS's
[`PreparedStatement`](../../impl/ts/src/api.ts) carries a resolved scan-plan
cache, validated in `executor.ts`; the other cores have equivalent obligations.
Generated execution belongs after resolution and physical-plan selection, and
inherits the plan's validity. It should not invent different access paths.

Useful tiers, in increasing investment:

1. Bind column ordinals, resolved types and operation implementations once.
   Precompiled closures or compact typed instructions can remove repeated
   selection without generating machine code.
2. Generate one expression function with explicit locals and branches. This
   removes recursive dispatch and exposes a particular expression to a host JIT.
3. Fuse supported filter/projection loops, then selected key/aggregate loops,
   at existing executor boundaries. Keep scanner, cursor, spill and ownership
   behavior intact. Whole-query fusion is neither necessary nor automatically
   semantics-preserving.

The existing [packed-leaf paths](packed-leaf.md) already implement encoded-span
predicates, columnar gather and vectorized kernels. Preserve these as controls;
replacing a good typed loop with a new compiler can regress it.

## TypeScript: direct feasibility and local measurements

Generate **JavaScript**, not TypeScript source. `new Function` can define a
strict-mode factory receiving only the compiler's helper table and constant
pool; its returned function receives current row, parameters and meter.
Unlike direct `eval`, it does not capture the emitter's lexical scope. It is
still a code-execution facility, not a security sandbox. Function-constructor
semantics are specified by [ECMAScript](https://tc39.es/ecma262/multipage/fundamental-objects.html#sec-createdynamicfunction).

The new probe compares the unchanged TS evaluator, a closure tree, and generated
straight-line JS. All use the real `Meter`, `bigint` values and checked integer
arithmetic helper. It tests three hand-authored integer-typed resolved trees;
it does not implement production type/admission proofs or SQL integration.
Generated kernels retain boxed `Value`s and helper calls: this is a conservative
first mechanism test, not the maximum possible specialization.

Node 26.10.0 / V8 14.6.202.34-node.34, Linux, Intel Core Ultra 9 285K, pinned to
logical CPU 3. Three fresh normal processes, five alternating samples per mode
per process, 50,000 warmups and 100,000 evaluations per sample. GC remains on.
The same non-minified JS bundle runs normally and with `--jitless`. Medians of
the three process medians, nanoseconds per expression:

| Expression | Tree | Closures | Generated JS | Tree / generated |
|---|---:|---:|---:|---:|
| `a + b` | 36.1 | 28.3 | 26.2 | 1.38× |
| `a * $1 + (b - 7)` | 96.9 | 84.6 | 78.4 | 1.24× |
| Previous expression `> a % 11` | 181.0 | 151.2 | 139.8 | 1.29× |

Closures capture much of the benefit without source evaluation. Generation
adds approximately 8% relative throughput over closures in these kernels.
One JIT-disabled process also improves with generation: approximately
1.64×/1.82×/1.88× versus the JIT-disabled tree. Normal V8 is much faster in all
modes; ordinary evaluator/closure code also benefits from JIT compilation.
A separate trace run confirms completed TurboFan optimization of **each** named
generated function (`jedCompiled0`, `jedCompiled1`, `jedCompiled2`), not merely
optimization of the surrounding benchmark. Trace timings are excluded above.

Source construction plus `new Function` for the three jobs took roughly
17–19, 71–78, and 102–111 microseconds in the normal runs. These are single
observations after other diagnostic work, not pristine cold-start distributions;
they exclude subsequent optimizing-JIT work. Repeated identical source can also
benefit from host caches. Even this setup cost needs hundreds or thousands of
row evaluations to amortize. The crude threshold is
`compile_time / (tree_time_per_row - generated_time_per_row)`; full-query setup,
warmup, code churn and cache eviction must be measured before choosing admission.

Each process makes 10,692 differential comparisons, covering values, error
messages/state/code, exact statement and lifetime costs, changing parameters,
NULLs, eager right-hand errors, overflow and divide-by-zero, before and after
warming the timed kernels. There is no new SQL behavior to oracle-check. This
does **not** establish full corpus conformance, lazy-storage fault coverage,
whole-query improvement, browser performance, memory reduction, or a production
compiler. No Rust/Go native JIT was built or benchmarked in this investigation.

## Browser and Node restrictions

The [CSP specification](https://www.w3.org/TR/CSP3/#unsafe-eval-usage) gates `eval`
and function constructors. A restrictive `script-src` (or its `default-src`
fallback) blocks ordinary string compilation unless permitted. A nonce/hash
authorizing the script file does not itself authorize `new Function`.
`'wasm-unsafe-eval'` permits WebAssembly compilation without permitting JS string
evaluation. Dynamic module/blob tricks should not be used to evade a host policy.

The local browser probe serves actual external scripts and Worker scripts over
loopback HTTP with real response headers. DevTools/Playwright evaluation only
reads the finished result, avoiding its ability to bypass page CSP. Measured
Chromium 149.0.7827.55:

| Environment/policy | `eval` / `Function` | WASM compilation |
|---|---|---|
| Browser, no CSP | Allowed | Allowed |
| `script-src 'self'` | Blocked | Blocked |
| Add `'unsafe-eval'` | Allowed | Allowed |
| Add only `'wasm-unsafe-eval'` | Blocked | Allowed |
| `'unsafe-eval'` plus `require-trusted-types-for 'script'`, ordinary string | Blocked | Allowed |
| Node default | Allowed | Allowed |
| Node `--disallow-code-generation-from-strings` | Blocked | Allowed |
| Node VM context with `strings:false, wasm:false` | Blocked | Blocked |
| Node `--jitless` in this runtime | Allowed, interpreted | WASM global unavailable |

Workers require precision: creation permission (`worker-src`) and execution
policy are separate. In the probe, URL Workers followed their own response CSP;
a URL Worker without a CSP could compile even when its parent page could not.
Blob Workers inherited the page restriction. Moving execution off-thread is
therefore not a general permission to compile. A host could explicitly serve a
dedicated worker under a suitable policy; the library should not arrange this
silently. No Firefox/Safari policy or performance run is claimed.

Trusted Types adds another gate. The current
[CSP draft's string-compilation algorithm](https://www.w3.org/TR/CSP3/#can-compile-strings)
includes `trusted-types-eval`; support and trusted-script construction need a
separate host integration. Do not install a permissive default policy to make
compilation work. The conservative path uses the evaluator when policy refuses.

[Chrome Manifest V3 extension pages](https://developer.chrome.com/docs/extensions/reference/manifest/content-security-policy)
cannot relax their minimum policy to add `'unsafe-eval'`. Sandboxed extension
pages have a different policy and lose ordinary extension privileges; that is a
different deployment architecture. A complete source-generation-free package
entry point is preferable for restrictive applications and extension review.

[Node's CLI documentation](https://nodejs.org/api/cli.html#--disallow-code-generation-from-strings)
explicitly blocks `eval`/`Function` with that flag, while exempting `node:vm`.
Using `vm.compileFunction` to get around the flag would disregard host intent.
[VM context options](https://nodejs.org/api/vm.html#vmcreatecontextcontextobject-options)
can independently prohibit strings and WASM; Node also states that `node:vm` is
not a security mechanism. Ordinary Node has no browser CSP requirement.

[JIT-less V8](https://v8.dev/blog/jitless) illustrates the distinction between
source-to-bytecode compilation and native JIT. Locally `new Function` still
worked with `--jitless`, but Node's native TS stripping failed with
`ERR_WEBASSEMBLY_NOT_SUPPORTED`: its stripper needs WASM. Prebuilding JS with
the already installed Vite/Rolldown toolchain allowed the kernel comparison.
That packaging issue is separate from whether generated JS is permitted.

For a production experiment, make source compilation host-controlled and
optional. Detect permitted capability once at setup if the host selects automatic
mode; a blocked capability probe can generate a CSP report, so an explicit
disabled mode must avoid even the probe. Do not catch query errors and retry
interpreted execution. Compile before execution or fall back before touching rows.

Another TS option is to emit WASM from the TS core's own typed IR, allowing native
optimization in hosts that permit WASM but prohibit JS string compilation. This
would remain an independent semantic implementation, not a Rust-core wrapper.
It needs a binary emitter, checked integer semantics, NULL representation,
linear-memory/batch interfaces and imported metering helpers. Per-row JS/WASM
transitions and boxed-value conversion could erase the benefit; batch kernels
are the more plausible target. It is a later hypothesis, not a measured result.
For application-known queries, shipping precompiled static JS functions can also
avoid runtime string compilation, but arbitrary ad-hoc SQL still needs a general
execution path and all cached-code/schema validity checks still apply.

## Rust options

Rust's ahead-of-time compiler optimizes the interpreter and existing typed
kernels, but cannot specialize arbitrary runtime SQL as if it were source code.
Runtime compilation therefore remains a real option. PostgreSQL's
[JIT documentation](https://www.postgresql.org/docs/current/jit-reason.html)
provides relevant precedent: it compiles expression evaluation and tuple
deforming, rather than requiring a replacement for every executor component.

Cranelift is a plausible backend for a small typed IR. Its
[JIT module API](https://docs.rs/cranelift-jit/latest/cranelift_jit/struct.JITModule.html)
returns a finalized function as a raw code pointer whose validity depends on the
module's code lifetime. Integrating this entails an ABI, checked data access,
code ownership and executable pages, plus unsafe invocation somewhere in the
trusted implementation. Rust's ownership/type checks do not automatically
validate the emitted machine code. Arbitrary loads, wrong layouts, missing
overflow checks or stale code pointers would invalidate the safety argument.

LLVM is another plausible backend with more optimization infrastructure and
packaging investment. Calling an external `rustc`, building a dynamic library,
and loading it is possible as a host-controlled experiment, but compiler
availability, subprocesses, artifacts, load latency and ABI/lifetime management
make it a poor default for a small embedded library. Neither is needed for the
existing typed VM or handwritten fused kernels.

Native JIT introduces target/deployment work: executable-memory policy, code
cache limits, relocations, CPU features and platform signing. Apple's
[Apple-silicon JIT guidance](https://developer.apple.com/documentation/apple-silicon/porting-just-in-time-compilers-to-apple-silicon)
describes `MAP_JIT` and hardened-runtime requirements. These are costs the Ruby/JS
VM normally absorbs for an application.

A WASM sandbox could provide a stronger validation boundary, but embeds another
runtime and requires marshalling or linear-memory layouts. A WASM module alone
does not make every imported host function safe. Wasmtime's
[security model](https://docs.wasmtime.dev/security.html) is relevant background,
not an approved jed dependency or an implemented option.

Recommendation: first retain immutable typed VM programs with validated repeated
scan plans, avoid compilation on point/unsupported paths, and compare against
typed fused kernels. Only then consider an opt-in native JIT if profiling still
shows large interpreter overhead. Cranelift/LLVM/Wasmtime would need explicit
dependency and architecture review; existing crypto/lock exceptions do not
authorize them. Keep the semantic lowering handwritten in Rust.

## Go options

Go's standard runtime does not offer Ruby-style evaluation of source into a new
optimized Go function. A closure assembled at prepare time executes already
compiled Go bodies; it is useful specialization, but not native code generation
for the arbitrary whole expression. Indirect calls, captured state, allocations
and missed inlining can offset saved dispatch, so measure rather than assuming.

The standard plugin path is particularly unsuitable here. The Go
[plugin documentation](https://pkg.go.dev/plugin) lists platform and toolchain
constraints and states that plugins cannot be closed. The actual
[loader source](https://go.dev/src/plugin/plugin_dlopen.go) requires cgo and
`dlopen`; the [alternative stub](https://go.dev/src/plugin/plugin_stubs.go) does
not provide loading. This conflicts with jed's pure-Go rule, beyond requiring a
compiler and filesystem artifacts at runtime.

A custom native JIT in Go is technically possible without an external C engine,
but normally needs architecture-specific trampolines/executable memory and an
unsafe boundary. Calling into generated code must account for Go's calling
convention, stack growth, GC pointer visibility and preemption. A cgo-free JIT
is not thereby a safe, ordinary pure-Go implementation under jed's constraints.
An embedded Go interpreter merely adds another interpreter unless it also has
a JIT; it does not supply the Ruby effect automatically.

A concrete cgo-free alternative is **typed query IR → WASM → wazero**.
[wazero](https://wazero.io/) is a Go runtime without cgo, and its
[compiler documentation](https://wazero.io/docs/how_the_optimizing_compiler_works/)
describes native compilation with an interpreter fallback. Thus the pure-Go
requirement alone does not make query JIT impossible. Jed would still need its
own semantic lowering, WASM validation/import boundaries, batching and value
layout, plus explicit approval of a substantial runtime dependency and its
native execution internals. This is more credible than `plugin` under the cgo
rule, but it is not a small evaluator optimization or an approved dependency.
No wazero performance or integration claim is made here.

The promising route is typed, precompiled kernels selected once: for example
column/constant comparisons, checked arithmetic, short-circuit boolean regions,
and batched scan/filter/projection operations. Closures and compact VM programs
are candidates, but the prior VM's regressions argue against blindly porting it.
Prioritize reducing `Value` movement/allocation and retaining existing columnar
advantages. Profile-guided builds can help the static engine: Go's
[PGO guide](https://go.dev/doc/pgo) describes compile-time optimization from
representative profiles. That is complementary to, not runtime compilation of,
new SQL. Rust has a corresponding
[PGO workflow](https://doc.rust-lang.org/rustc/profile-guided-optimization.html).

## Semantic and resource contract for a follow-up

The emitter must use compiler-owned templates and validated numeric ordinals.
SQL source, names, literals, regexes, error messages and host-supplied data stay
in data pools, never source snippets or dynamic property names. Bind types before
generation; the current TS column/parameter nodes alone are not a sufficient
proof of integer operands. Default to whole-expression fallback on unsupported
types/forms. This remains a handwritten per-core compiler, not shared generated
engine implementation.

Preserve NULL/three-valued behavior, filter AND versus value AND, left-to-right
operand reads, lazy CASE/COALESCE, integer overflow, decimal rules, collation
labels and all original guard/charge boundaries. No speculative divide-by-zero,
reordered deferred-page fault, early aggregate overflow or moved host callback.
Do not sum cost charges merely because operations were fused. Existing logical
memory reservations and cursor LIMIT/cancellation/pull behavior stay observable.

For floating point, preserve the specified width rounding and no-FMA behavior;
for JS integers keep `bigint` unless an independently proved specialization
preserves every intermediate overflow. JS text relational order is UTF-16 order,
not automatically jed's UTF-8/C order. Regex, collation, decimal and open-type
semantics continue through each core's established helpers until separately
admitted and verified.

Use bounded compiler depth/nodes/source/pool sizes, negative-admission caching,
and bounded retained plans/code. Generated code may capture immutable pools,
not live sessions, transactions, readers or bindings. Reuse existing schema,
database/attachment, parameter-type, environment and estimator invalidation.
Pass current bindings at each call. Active cursors may legitimately retain an
older pinned plan, so LRU eviction alone cannot bound all live generated code.
GC/VM code-cache reclamation is not immediate.

Compilation and JIT work need their own bounded-resource design; existing SQL
cost and query-memory counters do not cap a host compiler's total heap/code.
Admission based on query shape/reuse can be an internal optimization only if it
leaves results, errors, cost and selected plans identical. Do not use timing to
alter semantics or promise a whole-engine memory cap.

Proposed next experiment: TS integer/boolean expressions with tree, closures and
generated-source controls; selective Rust typed VM with plan-owned caching; Go
typed static kernels with allocation profiles. For each, force admission in a
corpus lane, test every budget prefix and invalidation path, then measure complete
prepared/ad-hoc queries on memory and file hosts, point-query controls, finite
budgets, mixed types, churn and first-use latency. Browser runs should include
strict CSP with compilation disabled and at least Chromium/Firefox/WebKit.
No adoption decision follows from the isolated kernel table alone.
