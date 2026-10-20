# Adopt Go streaming input-row reuse

**Decision: adopt.** One private input slice per ordinary Go streaming range
cursor removes repeated row allocations and resolution copies. Complete-query
scans improve on memory and warm-file hosts, with no reproducible control
regression. Outputs remain independently owned. Point scans, host-call read-ahead,
columnar execution, and blocking execution retain their existing ownership.
No G1/G2/G3 machinery, dependency, unsafe code, cgo, or FFI was introduced.

## Baseline and method

Clean production baseline: `c105eed383cae6ef98cfc8872b71aa3657f1c999`, recorded
before creating `codex/go-row-materialization`. The investigation inspected
`571e3e9fd2e3de2d4eb3daa2b3dd1715781de7b1` and its specialization report;
only its tag-independent complete-query benchmark was adapted. The production
change touches four Go files (48 additions, four deletions). Production source
hashes were unchanged between timing and verification; additional ownership and
trace tests were added after timing.

Go 1.27.1, linux/amd64, Intel Core Ultra 9 285K, default compiler/runtime settings,
no affinity or PGO, with other activity on the physical host. Five fresh,
sequential process pairs alternated baseline/candidate order at **150 ms per
workload**. Each operation opens, drains, checksums, checks exact cost, and closes
a query over 4,096 deterministic rows. Setup and warmup are excluded; named ad-hoc
and first-call lanes include parse/plan work. File hosts reopen and warm before
measurement. SkipFsync affects fixture setup only; these are warm-read results.

## Setup-separated allocation profile

With `MemProfileRate=1`, snapshot allocations after setup and warming both
bindings, run **100 complete composed-filter queries**, then snapshot again.
`pprof -base` subtracts setup; filtering `runtime/pprof` stacks removes snapshot
writer overhead. Instrumented timing is not performance evidence.

| Allocation over 100 queries | Baseline | Candidate |
|---|---:|---:|
| Input row backing slices | 38,400 KiB / 409,600 allocations | 9.38 KiB / 100 allocations |
| Projected output slices | 4,517.19 KiB / 144,550 allocations | unchanged |
| Total query allocations | 43,088.13 KiB | 4,700.32 KiB |

Source-row backing accounts for **89.1%** of baseline query bytes and outputs
**10.5%**, confirming the earlier setup-inclusive profile's hypothesis. Reuse
removes row backing allocations, not value decoding or payload allocations.
Outputs become 96.1% of remaining bytes without increasing in absolute size.

## Complete-query results

Selected medians in µs/query; negative change means faster:

| Query | Memory baseline → candidate | Change | File baseline → candidate | Change |
|---|---:|---:|---:|---:|
| Arithmetic | 584.05 → 420.98 | -27.9% | 563.90 → 451.23 | -20.0% |
| Arithmetic filter | 565.80 → 416.39 | -26.4% | 544.07 → 435.66 | -19.9% |
| Composed filter | 675.85 → 530.03 | -21.6% | 672.83 → 517.36 | -23.1% |
| Simple projection | 491.54 → 352.95 | -28.2% | 469.70 → 309.64 | -34.1% |
| Nullable arithmetic | 788.28 → 680.85 | -13.6% | 817.14 → 637.70 | -22.0% |
| Text/bytea projection | 1878.43 → 1416.66 | -24.6% | 1851.45 → 1422.51 | -23.2% |

Arithmetic allocation falls about **526 KB → 133 KB**, **8,210 → 4,115**
allocations/query. Composed-filter allocation falls **441 KB → 48 KB**
(**89.1% fewer bytes**), **5,560 → 1,465** allocations. Text/bytea projection falls
**3.28 MB → 2.23 MB**, **47,679 → 39,488** allocations.

The raw samples beside this report cover all 38 host/query lanes, including
LIMIT/OFFSET, finite cost, CASE/COALESCE, former-specializer unsupported `abs`,
point hit/miss, scalar, and columnar controls. Both bindings' checksums and exact
costs agree in every pair. The [README](../../README.md) gives commands to
regenerate full tables, ranges, and per-sample statistics from these files.

Initial file columnar aggregation was **+14.0%** (four of five pairs slower),
memory point-hit **+3.1%**, and memory point-miss **+3.0%**. One targeted recheck
used the **same binaries**, three alternating pairs at **250 ms**, only point and
columnar controls on both hosts. File columnar became **-7.2%**, memory point-hit
**+1.0%** (faster in two of three pairs), memory point-miss **-1.0%**, and file
point-hit **-1.7%**. No control slowdown reproduced. Apparent gains in unchanged
columnar execution are noise/layout effects, not an optimization claim. Recheck
raw samples are in `recheck/`; no further timing chased significance.

## Verification and retained evidence

- Full Go unit suite and full `go test -race ./...` pass.
- Go corpus: **678 memory passes / 2 skips**, **597 disk passes / 83 skips**, no
  failures. The new fixture regenerates identically from the PostgreSQL 18 oracle.
- Two Go NoREC seeds: **148 files / 1,676 query relations**, all pass, including
  streaming versus blocking scans with NULLs, filtering, direction, and windows.
- Baseline/candidate **13,984-line** traces match: plans, types, values, per-pull
  costs, memory balances/peaks, errors, and guard counts across 0–96 statement,
  lifetime, and cancellation prefixes, three memory budgets, and both hosts.
  SHA-256: `841a159748dc92634d14f1aadfdbea00e725117f0c0b2fcd008b48824c92fcc8`.
- Resource suites pass in memory/disk/forced-spill modes (**28 / 24 / 24**);
  every record's logical-memory peak and storage bytes match. A finite **1 TiB**
  accounting override leaves explicit tighter test limits in force. An initial
  64 MiB override hit query memory before an existing baseline scalar-limit test's
  intended limit; this diagnostic was preserved and is not an engine regression.
- Internal tests cover retained Row/Values/Scan outputs, nullable and nested
  values, packed and dirty decoded leaves, interleaving, concurrent prepared
  statements, deferred overflow failures, LIMIT, and early close.
- Shared spec/codegen and formatting checks pass. Other cores and repository-wide
  CI were not run for this Go-only implementation change.

Only this report and compact raw timing samples are tracked. The complete original
bundle—including profiles, source inventories, commands, generated summaries,
logs, and resource traces—is preserved locally in ignored
`bench/results/go-row-materialization/2026-10-19/`. Binary profiles and executables
remain in `.dev/`. The [experiment README](../../README.md) documents ownership
analysis and reproducible build, profile, timing, trace, and check commands;
future generated output also defaults to the ignored results directory.

## Integration onto master

Rebased onto `dfb0699f18e7c5bb120bc0f885f257791b19e2b9`. Its Go cancellation
implementation uses channels instead of callbacks, so the diagnostic now cancels
through the public context API before open or a cursor pull. The production
optimization is unchanged. Full Go unit/race, oracle, NoREC, memory/disk corpus,
resource comparisons, and shared spec/codegen checks pass again. The updated
13,984-line trace matches this master baseline exactly; its SHA-256 is
`e28629eca75a34f22432e492af14b94c3ce902402c0a9e66cb3b3eefa3cbae1d`.
The timing results above remain measurements of the original production baseline;
integration did not repeat performance measurements.

To reproduce integration checks, set `JED_ROW_BASELINE` to the revision above,
`JED_ROW_WORK=.dev/row-materialization-integration`, and
`JED_ROW_RESULTS=bench/results/go-row-materialization/integration`, then run
the experiment's `build`, `trace`, and `checks` modes followed by `mise run verify`.
Integration logs and traces remain in that ignored results directory.
