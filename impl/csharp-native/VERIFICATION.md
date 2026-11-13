# C# experiment — initial verification

Date: 2026-11-12 (America/Chicago). **First scalar milestone only; the engine is
incomplete and not frozen.** No performance claim or package publication.

## Identity

- Semantic baseline and supported peer source: `b1a6affca7725e0b07a4f734eddb1c569ef76bcf`.
- C# implementation: tested as working-tree additions, subsequently committed
  locally as `aeb741bd` on `codex/csharp-native-experiment`.
- Source SHA-256: `35137d5de128ab1d4ae8a4813da3a0b7dda504320bb8a5c93ac087cb511e647d`.
  The task launcher computes this from sorted source paths and bytes, including
  engine/harness C#, project configuration, generated data, task tooling and the
  supplemental corpus. Documentation and ignored build outputs are excluded.
- Supplemental corpus SHA-256:
  `899b1c3b4ed47895148763fed4d31cd690076aef48ead06796f22166cd8b745c`.
- Toolchain: Microsoft .NET SDK 11.0.100, net11.0; Linux x86-64,
  Ubuntu 26.04.1, kernel `7.0.0-38-generic`.

## Passing checks

| Command | Result and scope |
|---|---|
| `mise run csharp-native:test -c Debug` | Zero build warnings/errors; host/invariant tests, negative adapter tests and all 45 supplement records pass |
| `mise run csharp-native:test` | Same checks pass in Release with default async compilation |
| `mise run csharp-native:test --runtime-async` | Same checks pass with `Features=runtime-async=on`, from a fresh compilation |
| `mise run csharp-native:aot --runtime-async` | Clean Linux NativeAOT publish; the native executable passes the same checks |
| `mise run csharp-native:codegen --check` | Four generated data catalogs match the pinned spec |
| `JED_CONFORMANCE_FILTER=query/scalar_foundations.test mise run conformance:rust` | Entire supplement passes in memory and disk modes |
| `JED_CONFORMANCE_FILTER=query/scalar_foundations.test mise run conformance:go` | Entire supplement passes in memory and disk modes |
| `JED_CONFORMANCE_FILTER=query/scalar_foundations.test mise run conformance:ts` | Entire supplement passes in memory and disk modes |
| `mise run corpus:check spec/conformance/suites/query/scalar_foundations.test` | Byte-identical regeneration against the checkout's PG 18 oracle, with four existing planning-time divergences explicitly ledgered |
| `mise run verify` | All normal shared spec/catalog/fixture checks pass; no .NET dependency added to this task |
| `git diff --check` | No whitespace errors |

The host tests cover inline async completion, owned returned values, drain-time
errors and partial costs, per-statement and lifetime limits, canceled Task and
ValueTask state with SQLSTATE `57014`, invalid host Unicode, nonnullable reads,
deep input rejection, the 256-depth boundary, UTF-8 identifier length, disposal
without evaluation and locale-independent names/rendering. They do not establish
future asynchronous I/O, prepared handles, parallel reads or thread safety of an
unimplemented database.

Negative adapter checks reject unsupported directives and conditionals, incorrect
types/costs/error codes/row multiplicity, dangling directives and errors during
cursor drain. Selected SQL files run in full; the harness never replaces a cursor
drain with an engine-owned collector.

## Full-baseline attempt — expected failure, not acceptance

`mise run csharp-native:conformance --all` attempted all **763** pinned SQL files:
**1 passed, 762 failed**, with no capability-based skips. The passing file was
`cast/boolean_prefixes.test` (5 records). The ignored raw report is
`results/baseline-attempt.log`. Failures include missing table/relational/type
support and unimplemented harness formats/directives; the conservative adapter
also rejects prose comments shaped like unknown directives. These failures are
part of the initial inventory, not regressions waived to claim parity.

The 45-record supplement is separate from that baseline; it does not move the
pinned semantic revision. Most SQL, storage and acceptance work remains missing.
No file images, encryption, process pairs, simulation traces, resource memory
limits or benchmarks have been accepted for C#.

## Corrections and limits

The initial ConfigureAwait call was ambiguous because Rows implements both
IAsyncEnumerable and IAsyncDisposable; the harness now selects enumeration
explicitly. Review corrected WHERE/OFFSET order, nesting accounting, async
cancellation state and admission-failure ownership before the final tests.
The AOT task initially tried cleaning a RID-specific build after a normal restore;
it now restores the matching SDK packs before cleaning and publishing. Reported
AOT success belongs to that corrected task.

The four oracle overrides preserve supported jed semantics around skipped
projections and NULL filters versus PostgreSQL's planning-time constant folding.
They are not new C# divergences. The full default CI suite was not run: no
supported engine implementation changed, and scoped three-core conformance plus
normal spec verification passed. Windows, macOS, ARM and browser/mobile hosts
remain unverified. Crypto provider selection and dependency approval remain open.

## Subsequent implementation checkpoints — 2026-11-12/13

The initial milestone above remains historical evidence. Dependency approval is
now granted and implemented, while full acceptance remains incomplete.

Release JIT integrated checks passed at source prefix `ad64b602` after adding:

- Exact decimal tests with 4,000 independent test-only bignum oracle trials.
- Six complete scalar encoding files (202 vectors), three complete temporal
  fixture files (319 parse/error/render vectors), and 4,000 calendar invariants.
- Nineteen packed leaf goldens, eleven interior goldens, multi-way split bytes,
  600 permuted deletions and exact tree page deltas.
- All 107 shared catalog goldens decoded/re-encoded byte-for-byte, all entry kinds.
- Recovery of 110 plaintext fixtures; failed COW stages and poisoned-handle checks.
- Database host tests for commit visibility, pending reads, rollback, retained
  cursors, uniqueness and synthetic row keys.
- Crypto provider vectors and six encrypted golden files: 309 body pages and
  their meta/key-block codecs decrypt/reseal byte-for-byte.

The provider was also exercised in earlier clean NativeAOT builds with Runtime
Async, both portable and native-instruction targets, covering scalar and AVX2
BLAKE2b paths. These earlier builds do **not** establish AOT acceptance of the
subsequently expanded SQL/database engine; a fresh full-engine gate remains due.

A complete SQL diagnostic at source
`cc0d2150e7f423cd696b795e179b02ba1eff7116522fead906ac657d447e4d5d`
passed 69/763 files (1,331 records), with 694 files failing and no capability skips.
Raw working log: `/tmp/jed-csharp-tables.log` (ephemeral). All shared SQL records
still drain the public cursor externally. No full SQL, file-host, cross-core
interchange, process, simulation, resource-accounting or benchmark acceptance is
claimed by these intermediate checks.

## Integrated native-engine checkpoint — 2026-11-13

At source SHA-256
`85011fc9fa2e593f2f9ee6e3f22c19da50fd601ed739b6d533c0a40d5a489e29`,
the expanded native engine passes:

| Command | Result |
|---|---|
| `mise run csharp-native:test` | Release JIT full host/fixture/supplement checks pass; AVX2 enabled |
| `mise run csharp-native:crypto --portable` | Provider vectors pass with AVX2 disabled |
| `mise run csharp-native:aot` | Clean SDK-default instruction-set NativeAOT build and full host checks pass; AVX2 disabled |
| `mise run csharp-native:aot --runtime-async --native-instructions` | Clean Runtime Async NativeAOT build and full host checks pass; host-specific instructions, AVX2 enabled |
| `mise run csharp-native:codegen --check` | Eight generated data catalogs match the pinned spec |
| `mise run verify` | Normal repository spec checks pass |

Both NativeAOT builds completed without warnings or errors. These results cover
implemented host and fixture paths, including exact numerics, temporal/timezone
values, recursive codecs, JSON binary goldens, prepared AST reuse, real file I/O,
encrypted storage, compressed/overflow values and process coordination. They do
not establish complete SQL, resource, simulation, maintenance or platform acceptance.

The expanded Go↔C# owner/client interoperability run passed separately at source
`e3140dd7f61f56a203f32e8125ea7fded2c8068e8862420a9d2a16cfd9950c6a`.
It covers both owner directions, plaintext and encrypted files, role handoff,
owner death, lost-acknowledgment recovery, snapshot pins, and compressed and
incompressible large SQL values. Its source identity differs from this checkpoint;
these are scoped interoperability checks, not the entire shared process corpus.

The first full-SQL run at that same `85011fc9` source aborted with a stack
overflow in sequence-write classification while expanding views through CTE
shadowing. It is an incomplete diagnostic, not a 763-file result. The host/AOT
checks above did not cover that SQL case; a corrected source needs fresh checks.

The corrected sequence-write discovery carries lexical CTE scope, uses a fresh
scope when entering a view, guards active view cycles and bounds expansion.
At corrected source SHA-256
`8d5b2a68950da35d5b73653205a281f9afb30c0b1e17bd6574e1d9db6a047d8c`,
`mise run csharp-native:conformance --all` completed **763 files**: **293 passed**,
**7,148 records in passing files**, **470 failed**. The command exits nonzero for
those failures. No unsupported capability is silently skipped. Ephemeral raw log:
`/tmp/jed-csharp-checkpoint-full2.log`.

After that complete SQL diagnostic, all four integrated host/crypto/build commands
listed above were rerun successfully at corrected digest `8d5b2a68…`: Release
JIT, portable provider vectors, SDK-default NativeAOT, and Runtime Async NativeAOT
with host-native instructions. Both clean NativeAOT publishes again completed
without warnings or errors. This verifies the corrected checkpoint's implemented
host paths while leaving the 470 SQL failures and remaining acceptance gates open.

## Subsequent development checks — 2026-11-13

These checks ran during parallel implementation and are scoped diagnostics, not
a new frozen full-core acceptance result.

- Credential rewrapping: full host checks passed at
  `94210e5d854c47b689ba6b84bd68192e95286c2be4823339916387875b5090ea`,
  including before/after faults at five write stages, divergent-copy recovery,
  retained readers, and unchanged data/meta bytes. Go↔C# service checks passed at
  `082bee42081c1b5388096a0cd5e64ca9f84ad982eff631cc247a9db3c4218ebe`,
  including a pinned Go reader across rewrapping and fresh-client credentials.
- Plaintext image serialization/deserialization: full host checks passed at
  `010a23db88479c36b9a90300d06fd60cf67c0607580355de6b460f6fa94131a0`,
  including 22 complete shared image byte comparisons, committed-only export,
  input ownership, and independent imported storage.
- Aggregates: 48/52 complete files and 798 records in passing files at
  `87ad7f95556e0b1e72f8c3dda7c484e37cb510919a0c88e9fc6914b6ac74d6e9`.
  Ordered/hypothetical sets, interval/array percentiles, statistical aggregates,
  grouping sets, and primary-key functional dependencies pass their full files.
  Remaining files require join planning/costs or collation; a subsequent named
  window correction advances the star-grouping file to its join cost check.
- JSON: 41/51 complete files and 1,192 records in passing files at
  `7ff59771602f1cba99b34c74531373331d38bb7891a1f65fc9778e484a87aef5`.
  SQL/JSON constructors and query functions, record/populate functions, typed
  JSON_TABLE columns, and most JSONPath methods have full-file coverage. The
  remaining failures include relational planning, whole-row references, regex,
  dependency checks, and cost behavior.

These source identities differ. None supersedes the checkpoint's complete
763-file SQL attempt or proves NativeAOT parity for subsequent additions.

### Integrated development SQL and retained-shape checks

Full SQL diagnostic at
`e567343811c95f4508035ce94c9763fcd51782292881b0afb6952e8035f7f06b`:
`mise run csharp-native:conformance --all` completed all **763 files**, with
**439 passing**, **11,467 records in passing files**, and **324 failing**.
The command exits nonzero. No capability skips were introduced. Ephemeral log:
`/tmp/jed-csharp-full-development.log`. In particular, the JSON point-set cost
file regressed after correcting literal-IN evaluation; adjacent fixed-width
point probes must merge, and that later source correction needs fresh evidence.

Full host tests passed at
`e1c37ef2843017732e9c902ba0e38977cc93c2e250864b504551a6c0e738c088`,
including maintenance, collation/regex/open-key fixtures, multi-domain commit
preflight/staged-failure/retry invariants, and retained B-tree shape checks.
The shape tests evict the pager completely and verify that height, node count,
and bounded node estimates cause zero device reads, both for dirty paths and
after commit/reopen. These host checks precede later SQL/planner changes and
are not a NativeAOT or full-core acceptance claim.

### Corpus render-tag adapter

The C# runner follows the shared runners' flat-cell comparison contract: query
render tags determine comparison grouping and float tolerance, while `# names:`
and `# types:` assert result metadata. It does not add an assertion that tag
count equals the engine's result-column count. Existing homogeneous records in
`expr/string_functions.test` use one tag for several columns. Rust, Go, and the
Ruby experimental runner compare those records without a width assertion; the
C# adapter now does likewise. Expected cell count, exact non-float cell values,
explicit names/types, SQLSTATE, and cost assertions remain enforced. No frozen
corpus file was changed for this adjustment.

### Integrated SQL diagnostic after host and resource integration — 2026-11-13

The complete frozen 763-file run at source SHA-256
`bb40e2c4201433bc4539d8132762547bb15a42428baad77a527b605d6bf8682e`
passes **524 files**, containing **14,227 records in passing files**; **239 files
fail**. All seven shared concurrency files pass through real session admission
and snapshot/watermark APIs. The unmodified `expr/string_functions.test` passes
161 records after aligning the adapter's render-header behavior with the shared
runners; the frozen corpus was not changed. No capability skips were added.

The raw development log is `/tmp/jed-csharp-full-next.log`; the condensed failure
list is `/tmp/jed-csharp-full-next-failures.txt`. Remaining failures concentrate
in access/join planning, GIN/GiST/exclusion, DDL/DML, resource accounting and
spill, and EXPLAIN. This is a development breadth measurement, not final JIT/AOT,
process/simulation, performance, or full-core acceptance. Subsequent source edits
require their own verification.

### Deterministic ANALYZE and GIN posting bytes (development)

At source digest `a0e6caa0824e4b8cc6689292a10fa66a5526a03edd38b21709bcfe426f389d3f`,
`mise run csharp-native:encoding` passed collection of eight complete column facts from
`statistics_table`, `statistics_decimal_table`, and `statistics_terms_table`, comparing all
summary fields and exact typed MCV/histogram/term bytes with the pinned shared images.
The same run passed every `gin_jsonb_terms.toml` case (24), all five posting-run codec cases,
three greedy-packing cases and five maintenance histories from `gin_posting_runs.toml`.
The fixtures are consumed directly through the test-only TOML-to-JSON adapter.

The handwritten collector uses storage-key hash-priority sampling, bounded KMV, exact integer
NDV scaling, MCV/histogram selection and lossy GIN term counters. `ANALYZE` publishes all selected
columns atomically, ordinary changed DML marks facts stale, and schema changes retain or discard
facts by the shared ordinal/type contract. The development sweep at `bb40e2c4` passed the complete
`analyze_oracle` (5 records), `statistics_kmv_boundary` (14), transaction statistics (21), scoped
introspection statistics (18), and fixed-width decimal (54) files. Estimator/EXPLAIN and GIN query
acceptance remain separate checks.

The initial native GIN storage slice passed the complete `ddl/create_gin_index.test` (15 records)
at `a0e6caa0`: kind-1 index creation and maintenance use canonical bounded posting runs over the
ordinary COW B-tree. Query gathers, planner selection and complete GIN conformance are still in
progress; these results do not claim full GIN parity.

At `c83395b57b9f96485839dc22ece57038ad0582ced8e3e032e744003414220ea8`, the complete
encoding task also rebuilt all eight shared GIN database images by dropping and recreating their
indexes through SQL. Every image byte matched after normalizing only the expected transaction
number and its meta checksum for those extra DDL commits. This covers array, multi-column, JSONB,
float, interval, UUID, text, and decimal/bytea index images. The complete `query/gin_scan.test`
passed 13 records at `633b3e50`; mutation, sibling-join and broader GIN query checks remain underway.

The complete filename-selected GIN diagnostic (`mise run csharp-native:conformance --match
'*gin*.test'`) at `75819bb8a25e3d3a36105c5b28e57a7f40d66dda7823420a2591bce07d6a6d96`
passed 20 of 30 files, containing 424 records in passing files. All GIN DDL, SELECT/mutation,
element-type, disjunction, skip-scan, multi-column, JSONB/JSONpath, and JSONB sibling-join files
passed. Nine files stopped at EXPLAIN and the array sibling-join file stopped at its array-equality
join cost assertion (expected 166, observed 51). The log is `/tmp/jed-csharp-gin-all3.log`.
These explicit failures remain acceptance gates; the diagnostic does not claim full GIN parity.

Further development checks (2026-11-13, each result scoped to its own source digest):

- `c83395b57b9f96485839dc22ece57038ad0582ced8e3e032e744003414220ea8`: complete encoding
  checks passed, including four GiST golden structures (27 table rows), eight rebuilt GIN images,
  recursive storage codecs, memory sizes, regex, collation and timezone fixtures.
- `91f4678cbd29bdee8d103d3ca278882ee840c9d1071aebc5f8c2ff2c25f17d81`: encrypted rekey golden
  matched byte-for-byte, including reverse insertion order and forced scratch spill; plaintext,
  encrypted and changed-key round trips and pre-replacement failure behavior passed.
- `4c527944417498bac30fc966cc65dda58749b498a0834f4609ea2fb185a3e938`: eight exclusion/storage-limit
  corpus files passed (227 records). `99660591a71c649c3b73af8c9474b783042cff7ce4e5b146c322bd5476961c48`
  passed REINDEX, REINDEX divergences, savepoints, exclusion REINDEX and no-escape checks (217 records).
- `c3b2d0bbeb35f676b322bcde7e0310e760b18875388d45f9b675afddbf636028`: complete eager query-memory
  file passed (24 records), including the exact three-relation full-width buffering boundary;
  scalar GiST scan passed (19). The other three selected files failed explicitly.
- `a96698b66273a56926c68e2930f623c0e4c9f5a1796e80d9ce3dad3735182afa`: four complete diagnostic
  files still failed. Earlier failures in derived VALUES ownership, window aggregate state and
  grouped qualified ORDER BY had advanced to CTE retention and later join/query cases. This is
  partial diagnostic progress, not a passing-file or full-memory claim.

New common access-path inventory and join costing, memory ownership, EXPLAIN, filesystem fault
simulation and periodic reclamation remain under active integration. These development checks do
not supersede the last frozen JIT/NativeAOT checkpoint or establish complete process/simulation
acceptance. A single fresh-source full verification is still required.

Integrated development checkpoint (2026-11-13):

- Full host checks passed at `d37f6ce41af47ac84e30473cbb0f509b5291c256113b786f49c2aa17a96a3307`
  (`/tmp/jed-csharp-allocator-host2.log`). This includes native/remote file maintenance,
  periodic reclamation and trailing shrink, real domain storage-limit preflight, resident large
  values, simulation filesystem/scratch adapters, and plaintext/encrypted spill primitives.
- At `0f9728cac64fc7ce0e1b371a423b14b9062f9a3e3f2ae56ebb932168eb62a2fe`, SELECT INTO (40 records),
  its jed-specific cases (34), whole-row behavior (51), and main query-memory accounting (51)
  passed complete files. Three other selected files still failed: CTAS ANALYZE actual counters,
  GiST join EXPLAIN estimates, and a GIN join cost (32 expected, 37 observed).
  See `/tmp/jed-csharp-ddl-checkpoint.log`. The full SQL diagnostic uses this same digest.
- Complete query-memory operator-state coverage passed 42 records at
  `1ae1aa59a253a4526ad9c906afd6df21753f0dc8626c0338508621cfe3e2f16a`.
- External sorter and repeatable row-spool host checks passed at
  `24d8306e210c7c98477ede84eb999e80cd6794ca07345d00e24609d63bb533eb`: stable bounded run merging,
  plaintext/encrypted scratch, query-account-driven spilling, early disposal and exact ownership.
  SQL operator integration remains separate work; these checks alone do not establish SQL spill parity.

Cumulative process evidence is now 30 of 31 scenarios across scoped runs; relaxed durability
batching remains. This is not a single passing process suite. Canonical simulation trace execution,
complete SQL conformance, SQL spill, full physical planning and final clean JIT/NativeAOT verification
remain acceptance gates. No full-core or promotion claim is made.

The next full development SQL diagnostic at
`0f9728cac64fc7ce0e1b371a423b14b9062f9a3e3f2ae56ebb932168eb62a2fe`
(`mise run csharp-native:conformance --all`, 2026-11-13) passed **595 of 763 complete files**,
containing **15,999 records in passing files**; **168 files failed explicitly**. This supersedes
only the earlier development SQL sweep, not the frozen NativeAOT checkpoint. The complete log is
`/tmp/jed-csharp-full-checkpoint3.log`; its condensed failures are
`/tmp/jed-csharp-full-checkpoint3-failures.txt`. EXPLAIN now retains physical plan identities,
reports generic/custom parameter estimates, and observes execution cost. Statistics refine
NULL/value/histogram/GIN estimates from resident catalog facts. Remaining EXPLAIN attribution,
join/statistics selection, access-path breadth, and spill integration still require verification;
this result does not establish full parity.

Further scoped development evidence (2026-11-13):

- `b39388a2c3a4aa570e115bf91b1620b6fb0af2fb80e86a58bb045451ca094efa` passes complete
  `query_memory` (51), `query_memory_state` (42), and `query_memory_transient` (18) files,
  including exact collation-key and window operand-cache boundaries. It also passes spill
  operator and real SQL sort host checks: plaintext/encrypted scratch, stable ties, expression
  keys, a one-byte query account, cost invariance and early cursor disposal.
  Logs: `/tmp/jed-csharp-memory-transient4.log`, `/tmp/jed-csharp-spill-final-heads.log`.
- `1a17fc080fc55b7b8a12148ddaf3eebb853bbe70517e2b10bac4a87b1d744941` passes four full
  ORDER BY corpus files on native file-backed handles (128 records). This runner retains one
  file handle across records; it does not yet claim the supported cores' reopen-per-record mode.
- `b88235b1d1bc30cc824f08b8b0c6620fe598c7749da76c7ed5271e14bed66f32` passes six complete
  MERGE files (200 records). Later focused work passes nine of ten MERGE files (267 records);
  complete MERGE acceptance still requires its EXPLAIN file and fresh integrated verification.
- `bae3f1615ad1be86b3d9b0e3913fa37b5fbfa1ff45d98b0d73a13304202ff43d` passes secondary-index
  ORDER BY scans (8 records), PK-order scans (15), blocking top-k (12) and scalar GiST scans (19).
  Four other selected files still fail. Secondary interval selection and complete join planning
  remain active work; the log is `/tmp/jed-csharp-single-pipeline.log`.

File-backed SQL sorts now use the real external sorter under `work_mem`, including account-driven
spilling. Hash join, grouped aggregates and DISTINCT still need their bounded spill integration.
The conformance adapter routes backend-specific files to their applicable host without dropping
SQL records. Passing isolated sorter checks does not establish complete blocking-operator parity.

### Later integrated checkpoint — 2026-11-13

- Full frozen SQL diagnostic: **630/763 files**, **16,944 records in passing files**,
  **133 failed files**, source
  `bed160a7b699f6cc62d23a7d1939249f1a4baa1b0db887428cb50c1a2acc3774`.
  Log: `/tmp/jed-csharp-full-checkpoint4.log`.
- Full host task: **PASS**, source
  `c696f1094f649a3a566b882aa6b85203a46445f02ba40c80c57625a814cbd5f9`.
  Log: `/tmp/jed-csharp-host-checkpoint4.log`. Unix-socket tests required sandbox escalation.
  This is JIT host evidence; current-source NativeAOT and complete SQL parity remain pending.
- Five plaintext simulation traces match pinned peer transcripts across scoped runs;
  encrypted traces still have cost and crash-boundary discrepancies. No complete simulator
  acceptance is claimed. Keyed spill state and relaxed batching primitives compile but still
  require dedicated tests and complete integration.

### Canonical backend checkpoint — 2026-11-13

- Source: `db0ab23f0e9fced13fca929bd46b5319bf30d595699b10e1108d3d40833ed9a1`.
- `mise run csharp-native:conformance --all`: **679 passing files**, 18,856 records
  in passing files, **82 failing files**, two declared mode exclusions, 763 selected.
  Log: `/tmp/jed-csharp-full-checkpoint5.log`. This is the canonical memory mode;
  older counts that auto-routed backend-specific files are not directly comparable.
- `mise run csharp-native:test --no-build`: **PASS**, same captured assembly.
  Log: `/tmp/jed-csharp-growth-host.log`. Includes geometric file growth, Linux
  durable data barriers, relaxed batching and simulator inode/fault-host invariants.
- SQL spill checks pass for plaintext/encrypted files with work_mem=128 and
  max_query_memory_bytes=1, comparing rows/order/cost with resident execution:
  external sort, INNER/LEFT/RIGHT/FULL joins, ordinary and ordered aggregate
  collections, grouping sets and DISTINCT. These are host checks, not a substitute
  for shared exact memory thresholds; derived-table buffer accounting remains open.
  Follow-up narrowed dispatch after the shared corpus demonstrated that ordinary
  two-input nested-loop joins retain their resident memory-admission schedule;
  standalone RIGHT/FULL spill comparisons are mechanism evidence only.
- Earlier canonical disk diagnostic: 543 passing files, 15,246 records, 134 failures,
  86 exclusions. Earlier encrypted diagnostic: 610 row/type/error successes, of
  which 542 also match the pinned Go cost/EXPLAIN transcript (1,568 events);
  68 transcript mismatches, 66 prior SQL failures, 87 exclusions. Both use source
  `ae6fe58fb541f58e21c35dc3e5e11df87b6db4cb61b4811742388d3f3fc1d8bf`.
  Logs: `/tmp/jed-csharp-full-disk-first.log`,
  `/tmp/jed-csharp-encrypted-peer-first.log`.

No full parity or current-source NativeAOT result is claimed. Implementation and
verification continue against the frozen semantic baseline.

### Planner and recursive-query checkpoint — 2026-11-13

- Source `b012d973d295869a3b8cfa9fd28dd71d2e55e1f1da3e3eb097d91d6af8e31bb4`:
  canonical memory SQL **710/763 files passing**, **20,266 records in passing
  files**, 51 failed files and two declared mode exclusions. Full JIT host task
  **PASS** at the same capture. Logs: `/tmp/jed-csharp-full-checkpoint6.log` and
  `/tmp/jed-csharp-host-checkpoint6.log`.
- Newly complete focused files include SEARCH/CYCLE (43 records), CREATE TABLE AS
  (117), views (174), composite-PK interval-set EXPLAIN (15), secondary-index
  interval-set EXPLAIN (27), hash-join EXPLAIN (12), and index nested loops (21).
- Earlier capture `dced2ab0` passes all four selected memory-account files (135
  records), file-backed query memory and spill thresholds (64), and complete host
  and SQL spill checks. A later N-way planner change exposes a remaining eager
  memory-admission discrepancy; this earlier result does not establish that the
  latest source passes every memory file.
- Forced disk spilling passes all 52 aggregate files (894 records) at source
  `fa9e76dae9dc8ec6b8ad6d59c6eebcc567da8ee2a5f9b412330b5772a31383bc`.
  Exact group and DISTINCT spill thresholds pass at that capture.
- The encrypted recovery regression now matches physical images, recovered
  generations, live/free counts and digests through its complete trace. Subsequent
  captures continue to investigate SQL cost differences separately. Two complete
  attachment/relaxed-close traces match pinned peer transcripts at `64a2f99b`.

This remains an implementation checkpoint. Full SQL, all backend/resource gates,
complete simulator coverage and fresh NativeAOT acceptance are still pending.

- Checkpoint host recapture:
  `3b092099519185655eac42c9e9527bcc05cdc50e9f9b77854d9bbc2338afdfa9`,
  `mise run csharp-native:test`: **PASS**. Log:
  `/tmp/jed-csharp-root-checkpoint7c.log`. Includes the injected owner/client
  transport test with real page-service handlers, disconnection, reconnect,
  owner death and promotion, plus expression top-k and zero-capacity accounting.
  The first simulated-transport test exposed a handshake deadlock in the test
  network's write acknowledgement; the passing capture acknowledges consumed
  bytes and separately waits for protocol quiescence.
