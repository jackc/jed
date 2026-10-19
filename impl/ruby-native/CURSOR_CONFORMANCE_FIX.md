# Cursor conformance and memory ownership

This correctness pass follows `fc705ce719bfa6c829e630163b87d4cb2e5250a6`.
Ruby remains an explicitly selected, frozen experiment. No corpus expectation,
memory allowance, supported core, normal task, or dependency changes.

## Why the previous audit failed

The reported 46 failures were real failures of the audit, and treating them as
acceptable API differences was incorrect. The Ruby adapter called `Rows#rows`,
which admits an engine-owned collector. The Rust, Go and TypeScript conformance
adapters instead drain raw cursors into host-owned storage. The shared
[memory contract](../../spec/design/memory.md) distinguishes those owners.

The extra Ruby collector caused 32 expected-success statements to fail `54P05`
and 14 subsequent transaction-abort mismatches. The corrected adapter uses
`each` uniformly for every query record, captures final metadata and cost after
draining, and closes the cursor on success or deferred failure. It does not
adjust budgets, change expected errors, skip records, or route resource fixtures
to `execute`. The former mixed-routing option is rejected when it differs from
the selected API. `Rows#rows` remains budgeted and has separate host tests.

Removing that extra collector exposed three real engine failures: statements
that should fail at their exact allowance minus one incorrectly succeeded.
Those failures are corrected too.

## Engine corrections

Root WITH queries retain their ordinary buffered result and its reservations
until the body finishes. A materialized CTE's 873-byte peak therefore fails at
872 bytes, and the recursive CTE's 960-byte peak fails at 959 bytes, exactly as
the unchanged corpus requires.

Buffered cursor results are now delivered while their statement memory account
is still alive. Each outgoing row releases its source ownership before an
engine-owned collector reserves it. Unread rows remain charged until exhaustion
or close. This also repairs the account lifetime for VALUES and set-operation
results, which previously could be delivered after the account had unwound.
Collation decoration transfers the same ownership token and payload mask to its
replacement relation, so loading a collation cannot silently release buffered
rows before the cursor drains them.

Unmetered plain column projections use the canonical gathered-column lane on the
first pull, with or without a finite memory budget. A six-row i32 lane needs 192 logical
bytes and fails at 191. Repeated projections of one source column share one
gathered lane. Gathering visits source rows and distinct ordinals in the shared
order, then admits their aggregate logical bytes at the shared checkpoint.
Opening a cursor remains lazy; admitted outputs yield one row at a time.
LIMIT/OFFSET and expression/host-function projections keep their existing lanes.
Positive statement/lifetime cost ceilings and profiling keep their previous
incremental path, including memory-versus-storage-fault ordering. Gathered text
and text arrays receive their declared column collation before escaping, without
reading storage a second time.

The complete file cursor audit also caught two result failures in
`joins/where_pushdown.test`. Spill preflight could evaluate an uncorrelated
subquery before cursor depth tracking began, treating the subquery's rows as
outer output. Cursor depth now encloses spill planning and validation as well as
execution. Loading the spill wrapper earlier preserves the page-service cursor
pin wrappers' existing order. The negative audit is retained under
[pre-nesting-fix](../../bench/ruby-native/results/cursor-conformance-fix/pre-nesting-fix/).

## Regression coverage

The existing shared resource entries cover the three incorrect admission
boundaries without new expected values. Focused host tests cover the adapter's
external ownership, deferred error/close behavior, final metadata and exact
cost; buffered ownership transfer, captured budgets and early close; unique
gathered columns, partial consumption and cost/fault ordering; and the distinct
engine-owned collector limits. Loaded-collation probes check that decoration
preserves the account across suspension. Nested spill-preflight probes exercise
JOIN and DISTINCT subqueries across direct/prepared queries and external/owned
collection, comparing exact rows, metadata, costs, errors and lazy callbacks on
memory and file hosts. No test weakens a resource check to obtain a pass.

Final bounded corpus and host-suite results are retained in
[cursor-conformance-fix](../../bench/ruby-native/results/cursor-conformance-fix/).
Historical reports and their original failure objects remain unchanged.

## Final verification

All four complete audits pass on library SHA-256
`7724a4418421e56948d036dc04e54008df97b19d2609cf8b65a7ca0f63a2fe02`, with
conformance-harness SHA-256
`230ef1ebb799353fe57c6eb0874234567c305f2aa8e50a5ee6fd4fa95c18741d`.

| API | Storage / runtime | Passed | Failures |
| --- | --- | ---: | ---: |
| `execute` | Memory, ZJIT | 20,267 / 20,267 | 0 |
| `query`, external collection | Memory, ZJIT | 20,267 / 20,267 | 0 |
| `execute` | File, YJIT | 18,250 / 18,250 | 0 |
| `query`, external collection | File, YJIT | 18,250 / 18,250 | 0 |

All 59 host-test files also pass on Ruby 4.0.7's interpreter, including storage,
page-service, golden byte fixtures, compilation and resource-boundary tests.
The focused new ownership and nesting regressions pass in both the interpreter
and YJIT. There are no unsupported files, timeouts, memory-limit hits or omitted
failure details. Neither query audit invokes `execute` through the adapter.
The largest sampled corpus-worker RSS is 170.6 MiB; the full host suite peaks at
127.4 MiB. Every worker retains the 512 MiB RSS guard.

The retained source record pins the corpus, harness files and host-test files;
each audit journal records its command, environment and runtime executable hash.
The independent evidence verifier checks these identities, counts, authored
exclusions and guards before producing its summary and recursive file manifest.
The two failures in the archived intermediate file audit remain explicitly
separate from the final zero-failure result.

## Reproduction

The recorded audits use Ruby 4.1.0-preview1 with ZJIT for memory and Ruby 4.0.7
with YJIT for files. Compilation is immediate, so eligible one-shot statements
exercise generated kernels. Disk audits select the exclusive file host. Every
corpus file has a 10-second deadline, a 512 MiB RSS guard and a 2 GiB address-space
limit. The outer audit has the same memory limits and a 900-second deadline.
The two authored memory skips remain unchanged. File mode retains its 76
explicit authored skips and one concurrency schedule that requires the
in-memory storage-limit capability.

From the repository root, run each API explicitly (existing evidence files are
protected unless `--replace` is supplied):

```sh
JED_NATIVE_TIMEOUT=1800 JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:bounded -- ruby \
  bench/ruby-native/results/cursor-conformance-fix/run_audits.rb \
  --api query \
  --source-sha256 7724a4418421e56948d036dc04e54008df97b19d2609cf8b65a7ca0f63a2fe02 \
  --harness-sha256 230ef1ebb799353fe57c6eb0874234567c305f2aa8e50a5ee6fd4fa95c18741d
# Repeat with --api execute, then run the bounded host suite:
JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 mise run ruby-native:test
```

The paired runner's individual audit commands set their own explicit bounds.
These are opt-in experiment runs. The source and harness pins require the
revision recorded in this report; later optimizations need their own audit.
