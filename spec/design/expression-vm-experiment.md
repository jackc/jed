# Expression VM experiment

Status: experiment completed 2026-10-18; prototypes remain opt-in and the production evaluator
remains the recursive tree walker.
No SQL behavior, public embedding API, storage format, or semantic cost schedule changes.
This branch is research code, not a production rollout.

## Question and scope

Does flattening resolved scalar expressions improve end-to-end execution enough to justify
another lowering pass in the three independent cores? This tests the expression portion of
the VM idea left open by [streaming.md §3](streaming.md), not a statement VM or native-code JIT.
The existing relational operators, planner choices, encoded-span predicates, columnar paths,
and cursor/storage lifetimes remain in charge.

The initial admitted surface is integer and boolean expressions: columns, bound parameters,
constants, integer arithmetic and comparisons, NULL tests, boolean connectives, CASE and
COALESCE. Integer widths and overflow semantics are resolved before execution. Existing value
kernels remain authoritative for each core's operations. Unsupported expressions fall back
as a whole; an unsupported subtree must not be evaluated speculatively or partially lowered.
More complex values/functions are controls, not a claim of full evaluator coverage.

## Execution contract

Each core independently lowers into a flat instruction array and interprets it. Physical
layouts and dispatch strategies are implementation choices, not persisted bytes or a public
ABI. Programs are immutable; temporary registers belong to an execution. A cache may retain
only valid source expressions and must not confuse reused addresses or filter/value contexts.
Experimental process caches are reset explicitly and are not a production cache design.

The semantic operations are:

| Operation | Obligation |
|---|---|
| Enter | Perform the original node's guard, then its original semantic charge, before children. Leaf guards still occur. |
| Load | Read the same column/parameter/constant and preserve deferred-value handling. |
| Unary / binary | Apply the resolved kernel; preserve any post-operand guards and checked integer width. |
| Branch / merge | Follow original left-to-right evaluation, with SQL three-valued boolean logic. |
| Filter AND | Stop on FALSE **or NULL**, as the existing filter evaluator does. Value AND stops early only on FALSE. |
| CASE / COALESCE | Evaluate only the selected arm or the required prefix, respectively. |
| Return | Return the same typed value or filter decision; release transient scratch appropriately. |

Instruction dispatch is not charged. Cost means the same semantic work as the existing tree
walker, including its exact abort boundaries. Source error positions/details remain intact.
No operation moves volatile calls, clock/entropy access, or allocation checks across a branch.
The VM does not relax parsing/planning depth limits or introduce host access into built-ins.

## Shared experiment inputs

`spec/experiments/expression-vm/cases.tsv` defines direct expression comparisons using the
schema and rows in `rows.tsv`: `t(id i32 PRIMARY KEY, a i32, b i32, p boolean, q boolean)`.
The expression parameters are always `[7::i32, 3::i32]`. Each case runs as both engines over
each row, including ceilings from zero through the successful cost plus two, comparing values,
errors and accrued costs. Filter cases compare the predicate decision, not its nullable value.
These are internal evaluator equivalence tests, not replacements for the SQL corpus or PG.

`workloads.tsv` contains end-to-end SQL controls and targets. Each driver creates `t` with
20,000 rows: id=1..20000, a=id%97, b=id%13+1, p=(id%2=0), q=(id%3=0). Every 17th a,
19th b, 23rd p, and 29th q is NULL. Setup and warmup are excluded from timing.
Columns are separated by literal tabs: name, execution (`prepared` or `adhoc`), iterations,
SQL. Prepared queries are prepared once; ad-hoc queries include parsing/planning on each call.
Every timed query is fully drained and its row/cell count consumed. Full FNV-1a checksums
(rendered UTF-8, NUL after each cell, newline after each row) are verified before and after
timing. Rendering/hashing is excluded from the timed drain, especially because per-byte
BigInt hashing in TS would overwhelm the engine work. Cost and checksum must match. A metered
scan control installs a nonbinding cost ceiling; no speculative batching may change deterministic abort behavior.

Run cores sequentially, with independent processes and alternating tree/VM order. Report
the median of paired percentage changes, plus the observed minimum and maximum; these
small samples do not establish confidence intervals. Also report each mode's median query
latency. Do not select the fastest sample. Report preparation/compilation overhead,
program/cache footprint and allocations where measurable, and state where instrumentation or
coverage differs across cores. Do not compare absolute times across languages as a VM effect.

The disk workload closes and reopens its database after setup, then warms the read path.
Scan workloads warm three executions; point workloads warm up to 100. These are warm-read
measurements, not cold-cache or I/O-bound benchmarks. Go's baseline build tag and Rust's
baseline feature compile the evaluator hooks away. TypeScript's baseline is
a disposable source tree with the base revision's original evaluator and the same host driver,
so the tree walker does not pay disabled experimental checks on each recursive node.

## Decision gate

Correctness is mandatory: shared SQL corpus, internal differential checks including budget
boundaries, and existing resource/stack tests. Performance adoption requires material gains
on real expression-heavy queries without meaningful regressions in point lookup, ad-hoc,
columnar and expensive-function controls. Microbenchmarks alone do not justify adoption.
No new dependency is needed. Results and a decision are recorded below.

## Implementations and measurement limits

The experiments deliberately share behavior, not a physical opcode layout:

- **Go:** `-tags expression_vm` enables a process cache keyed by retained expression pointer
  and value/filter context. A meter owns the reusable registers. The ordinary build uses empty
  annotation/state types and constant-folds its hooks away. The separate
  `expression_vm,expression_vm_baseline` build compiles the evaluator hooks away while retaining
  the experiment driver. It still has the experiment's larger expression/meter layout, so this
  is an execution baseline, not an assertion of unchanged production object sizes. The cache is
  reset between workloads; its unbounded retention is unsuitable for adoption as written.
- **Rust:** `--features expression-vm` uses compact integer/boolean/NULL registers and statically
  checks input kinds. Programs and scratch belong to the cursor or plain columnar predicate.
  Programs compile once per execution, including prepared executions; no plan is mutated and no
  process cache retains source trees. `expression-vm-baseline` removes the production hooks and
  fields while retaining the test driver. A thread-local evaluation counter remains in the VM
  measurement to demonstrate that benchmark rows actually execute it.
- **TypeScript:** an internal synchronous scope opts into identity-keyed WeakMap program caches;
  execution environments own reusable scratch, with separate scratch on reentrancy and cleanup
  on errors. Type-erased column/parameter nodes mean this VM is only partially specialized:
  existing generic comparison/cast kernels also handle loaded noninteger values, and date
  subtraction retains its source-family dispatch despite its integer result type. Unsupported
  expression kinds fall back as a whole. The production entrypoints never opt into the scope,
  but the branch adds disabled hooks; the clean benchmark baseline therefore restores the
  original evaluator in an isolated copy, rather than measuring those hooks as tree cost.
  This does not measure the modified-but-disabled TS evaluator's overhead.

No implementation reads an environment variable or clock inside a scalar kernel. Experiment
drivers read host configuration; compiler microbenchmarks read time in the harness. The normal
planner, schema invalidation, source error decoration and value kernels remain in place.

Go's `program_bytes` counts supported program headers and instruction capacities, excluding
map overhead and all retained source trees, including unsupported expressions. Compiler probes
run separately from query timing; they compile each shared expression 10,000 times after resolving
it once. Go allocation counts/bytes in the end-to-end records cover the timed drain. Rust's
program/scratch byte counts accumulate over timed compilations and must be divided by their counts; they are not peak retained memory. TS reports
instruction/slot counts, not guessed JavaScript object sizes. These diagnostics do not establish
equal heap usage across cores or a whole-engine memory bound. TS counters include warmup and
other compilations since reset, rather than current live cache size. The original resolved
trees remain: the experiment adds a representation instead of replacing those trees.

Compiler work and program/register allocations are outside SQL cost and memory accounting.
Lowering still recursively traverses the AST; the experiment does not remove compiler stack
limits. Rust's integration is selective: DML/default/check expressions, grouped/window/synthetic
rows and several specialized residual-predicate paths still use tree evaluation. Passing the
whole corpus therefore proves compatibility of the enabled engine, not universal VM coverage;
direct fixtures assert admission and benchmark counters establish actual execution coverage.
The production TS 512 KiB stack tests and ordinary Rust unit suite did not enable VM hooks.
There is no dedicated Rust VM cancellation/lifetime-budget or small-stack stress result.

Verification before timing: the new `compiled_branches.test` regenerated byte-identically from
this checkout's PostgreSQL 18 oracle; shared metadata/fixtures/codegen verification passed.
Every VM passed the full corpus in memory (578 passed, 2 skipped) and disk (515 passed,
65 skipped). Production units passed in Go, Rust (993 library tests plus binary tests) and TS
(905 tests). Internal fixture comparisons sweep cost ceilings; Go additionally checks lifetime
budgets, cancellation poll order and concurrent cache use under the race detector; TS checks
lifetime budgets, prepared schema invalidation and parameter changes. Existing TS resource/stack
checks passed. No browser or multi-process stress run is claimed for this experiment. The final
scoped format/type/lint check passed; Biome reported only existing warnings in unchanged files.
Go formatting changed in two files after measurement; the recorded source hash identifies the
measured `e9f39919` sources exactly. The runner subsequently pinned its baseline revision to
keep future master changes from altering the comparison; a final all-core smoke run checked it.

## Measurements (2026-10-18)

The final runs use implementation commit `e9f39919`, Intel Core Ultra 9 285K, Linux, Rust 1.99,
Go 1.27.1 and Node 26.10.0. All benchmark processes inherit affinity to logical CPU 3. This
reduces migration across this host's different CPU cores; it does not isolate the CPU from
other host activity or fix frequency. Five independent process pairs cover file-backed reads;
three cover in-memory reads, alternating tree/VM order. No other agent builds/tests/benchmarks
ran alongside these measurements. Tiny controls were lengthened to 100,000 prepared point
queries, 20,000 ad-hoc queries and 500 columnar aggregates per process.

Exploratory unpinned runs had large control swings and very short point samples. Those prompted
this fixed-CPU repeat before drawing a conclusion. The final Go harness also removes repeated
cache recompilation from between workloads; compiler microtimings run separately. All final
pairs are included, with no outlier removal. Small changes and mixed signs remain inconclusive.

The table shows median **paired percentage change in query time**: negative is faster, positive
is slower. Each scan reads 20,000 rows. These are comparisons within each core, not language
rankings. The separate mode medians in the raw summary need not yield the paired percentage.

| File-backed workload | Go | Rust | TypeScript |
|---|---:|---:|---:|
| Arithmetic scan | +15.8% | -16.8% | +0.7% |
| Boolean filter | +20.2% | -29.9% | +17.5% |
| CASE / COALESCE scan | +12.4% | -6.7% | +5.1% |
| Arithmetic with cost ceiling | +13.6% | -16.5% | +3.3% |
| Prepared point lookup | +7.7% | +17.9% | +15.1% |
| Ad-hoc point lookup | +87.8% | +7.7% | +6.5% |
| Columnar aggregate control | +3.1% | +0.2% | +2.7% |
| Decimal sqrt control | +0.2% | +0.5% | -3.0% |
| Unsupported abs control | +7.2% | +6.9% | -0.7% |

The [disk raw observations](../experiments/expression-vm/results/2026-10-18/disk/results.jsonl),
[summary with observed ranges](../experiments/expression-vm/results/2026-10-18/disk/summary.json)
and [environment/source hashes](../experiments/expression-vm/results/2026-10-18/disk/environment.json)
are retained. Rust's boolean improvement is consistent across all five pairs (27.2–30.4% faster).
Its arithmetic improvement ranges from 1.0% to 17.8%; the median is useful evidence, not a
promise of a fixed speedup. Rust's columnar control ranges from 7.3% faster to 13.8% slower;
its small median does not prove zero overhead. TypeScript's mixed scan results likewise do
not establish an arithmetic/conditional improvement.

The in-memory results support the same conclusion:

| In-memory workload | Go | Rust | TypeScript |
|---|---:|---:|---:|
| Arithmetic scan | +16.9% | -16.4% | -0.1% |
| Boolean filter | +19.9% | -29.7% | +14.8% |
| CASE / COALESCE scan | +11.8% | -7.6% | +1.9% |
| Arithmetic with cost ceiling | +13.9% | -17.5% | +1.2% |
| Prepared point lookup | +6.2% | +19.0% | +22.1% |
| Ad-hoc point lookup | +84.8% | +3.6% | +8.6% |
| Columnar aggregate control | +1.1% | -0.2% | +1.7% |
| Decimal sqrt control | +1.7% | +0.8% | +2.3% |
| Unsupported abs control | +4.7% | +3.5% | +5.5% |

All 432 final observations (270 disk, 162 memory) match exactly in checksum and semantic cost
across modes and cores. The [memory observations](../experiments/expression-vm/results/2026-10-18/memory/results.jsonl),
[summary](../experiments/expression-vm/results/2026-10-18/memory/summary.json) and
[environment](../experiments/expression-vm/results/2026-10-18/memory/environment.json) preserve all samples.

### Compilation and memory

The representation does not remove the original resolved tree. For the arithmetic workload,
Go retains one 26-instruction program with 3,032 counted bytes; Rust allocates 832 instruction
bytes and 48 register bytes per execution; TS reports 19 instructions and 3 slots, without a
heap-byte estimate. These are different accounting scopes, not a cross-core memory ranking.

Go's arithmetic scan allocation count is essentially unchanged: 40,096 allocations per query
for the tree and 40,097 for the VM, with 96 additional bytes per query. Prepared point queries
add one allocation and 64 bytes. The 20,000-query ad-hoc lane leaves 40,206 supported cached
programs including warmup: 29,913,264 counted program bytes (28.5 MiB), excluding map overhead
and retained ASTs. Its median allocated bytes per query rise from 16,609 to 19,787. This cache
is unsuitable for production; the ad-hoc penalty measures this prototype, not every possible VM.

Rust recompiles per cursor, including prepared execution. Its setup penalty is therefore real
for this prototype but could change with immutable programs attached to valid prepared plans.
Unsupported roots still incur admission/fallback overhead, even though their rows execute the
original evaluator. No Rust allocator-count or TS live-heap improvement is claimed.

Compiler probes use three independent processes per core, each compiling an already resolved
expression 10,000 times. The following is the median of each process's mean compile time, in
microseconds, including compiler allocation. These are warmed compiler probes rather than
cold first-query latency, and exclude parsing/resolution; TS also includes JIT/GC effects.

| Expression | Go µs | Rust µs | TypeScript µs |
|---|---:|---:|---:|
| `a + b` | 0.284 | 0.069 | 0.240 |
| `$1 + a * $2` | 0.525 | 0.086 | 0.456 |
| Nested boolean expression | 0.845 | 0.113 | 0.566 |
| Searched CASE | 0.878 | 0.135 | 0.692 |

The [267 compiler observations](../experiments/expression-vm/results/2026-10-18/compiler/results.jsonl),
[summary](../experiments/expression-vm/results/2026-10-18/compiler/summary.json) and
[commands/environment](../experiments/expression-vm/results/2026-10-18/compiler/environment.json)
include all fixture cases: Go/TS report 29 admitted cases, and Rust additionally reports its two
rejected controls. Preparation and first-execution samples are retained in the end-to-end raw
records; they are single observations per process, not a statistically established cold-latency
comparison. The end-to-end point/ad-hoc lanes are the stronger evidence about setup penalties.

## Decision

**Keep all three prototypes opt-in. Do not replace jed's executor with a statement VM on this
evidence.** The experiment answers the scalar question more narrowly:

- Independent programs can preserve the shared semantic contract, including filter-vs-value
  NULL behavior, short circuits, overflow, and exact cost abort boundaries. A common persisted
  bytecode format or reference executor is unnecessary.
- Rust's compact typed program is promising for repeated scalar work over many rows. This
  measures that implementation, including its specialized registers and integration points;
  it does not isolate flat dispatch as the sole cause of the gain.
- These prototypes miss the general adoption gate. Go's target scans regress; TypeScript has
  no consistent target gain; prepared point queries regress in every core. Unsupported roots
  can also pay measurable overhead. Better results on one host or one scan do not resolve
  these costs.
- The next justified experiment is selective execution: retain immutable programs with valid
  prepared plans, bind them at execution setup, and bypass compilation for cheap point queries
  and unsupported roots. Start with the Rust scan paths that showed a repeatable benefit;
  preserve the common behavior contract in all cores. Repeat these controls before expanding
  coverage. This is a follow-up hypothesis, not an optimization proven here.
- Production work also needs bounded compiler/cache ownership, a resource-accounting design
  consistent across cores, and VM-enabled small-stack/lifetime/cancellation checks. Go's global
  cache and TS's disabled evaluator hooks would need separate treatment. The experiment adds
  no new dependencies and changes no standing production execution architecture.

## Reproduction

Use the repository's pinned tools and declared Node dependencies. `mise run dev` starts this
checkout's PostgreSQL oracle; no external reference checkout is needed. File runs also need
local page-service socket access. Each experiment is opt-in:

```sh
mise run experiment:check
mise run verify
mise run corpus:check spec/conformance/suites/expr/compiled_branches.test
mise run experiment:go
mise run experiment:rust
mise run experiment:ts
GOFLAGS=-race mise run experiment:go
JED_EXPR_VM_MODE=vm mise run experiment:conformance:go
JED_EXPR_VM_MODE=vm mise run experiment:conformance:rust
JED_EXPR_VM_MODE=vm mise run conformance:ts

# Linux: choose an allowed logical CPU; omit --cpu on other hosts.
# Run these sequentially, without concurrent builds, tests or benchmarks.
mise run experiment:bench --repeats 5 --storage disk --cpu 3
mise run experiment:bench --repeats 3 --storage memory --cpu 3

# Compiler-only probes, separate from query timing:
mise run experiment:go:compile
JED_EXPR_VM_COMPILE=1 mise run experiment:rust
JED_EXPR_VM_COMPILE=1 mise run experiment:ts
```

The benchmark task writes raw JSONL, per-process logs, summaries and environment metadata below
`bench/results/expression-vm-*`. It checks workload completeness, actual use of the VM on target
scans, exact cross-core checksum/cost equality, and stable source/workload hashes during timing.
The `tree` mode creates a clean baseline automatically, pinned to pre-experiment commit
`172e3b0a`; `--baseline <rev>` explicitly selects a different source baseline. Simply turning a
runtime flag off is not an equivalent baseline. The common `--iterations 1` override is for smoke tests only.
