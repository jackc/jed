# Frozen baseline findings

Baseline: `0b76dd6e9c2675b552eb8af9e820065351ffa4b3`. These observations do
not amend shared expected results or declare Swift acceptance complete.

- **ALTER rewrite pending bytes:** the frozen full-table and backing-index
  rewrite paths create fresh stores with cumulative staging, then insert the
  rewritten records without enabling per-key net staging (Go `snapshot.go`
  `alterTableRewrite`, `syncAlterConstraintIndexes`, and `rebuildAlterIndexes`;
  Rust `executor/snapshot.rs` and TypeScript `snapshot.ts` equivalents). Later
  DML tracks its own new versions without reclaiming those earlier rewrite
  bytes. Swift preserves this narrower implemented behavior even though
  memory.md §7 describes net pending writes generally. In the generated
  `resource-5` simulation, the difference is 7,355 bytes before step 153: the
  supported cores retain 43,553 pending bytes beside 22,572 source-row bytes,
  exceeding the 65,536-byte query budget. No corpus or simulation expectation
  changes; a shared staging-policy revision would be separate work.

- **Private temp writes and host admission:** temp-tables.md §5 explicitly says
  private temp writes require no global writer gate and are available to a
  read-only session. The frozen Go `shared.go` dispatch and `execute.go` write
  classification instead gate every write and reject writes in read-only modes;
  the Rust and TS dispatches use the same broad classification. Swift currently
  follows the canonical private-temp contract, with host-boundary tests. Explicit
  read-only COMMIT still discards private changes, as §5.1 and all three cores
  specify. Broader shared-host reconciliation remains separate work. A temp DML
  expression/default advancing a main sequence must still select a shared writer;
  Swift now includes a metadata-only expression/default/CHECK/FK-effect classifier;
  its dedicated host boundary tests pass in the281-file integration gate.
- **Mixed DROP capability:** the frozen cores select `allow_temp_ddl` for an
  entire DROP when any target is temporary, including its persistent targets.
  Swift preserves that behavior pending a separate shared correction; it does
  not silently strengthen the gate in one core. Ordinary persistent DDL still
  uses `allow_ddl`, and temp ANALYZE/REINDEX remain maintenance under that gate.
- **Writable CTE parameters:** the frozen cores bind query/RETURNING metadata
  before independently binding each DML part. Parameters used only in DML may
  therefore produce the existing parameter-count/gap errors. Swift retains the
  same preparation boundary; no new inference policy is selected here.
- **Second durable writer:** the process corpus stages both file changes and
  requires `0A000` at COMMIT. Swift's initial statement-level rejection was too
  early and has been removed. Commit still rejects before any staged or durable
  storage write, and the unchanged attachment-gate-order scenario now passes.
- **Historical nosort render tags:** frozen `query/statistics_p9.test:43` declares
  nine render tags for ten selected columns and twenty expected cells. The shared
  runner treats those tags as rendering hints and compares every nosort cell as
  one flat array; precise shape assertions use `#names` and `#types`. Swift now
  follows that behavior while retaining complete-row validation for `rowsort`.
  The historical header is unchanged, and no expected cell is discarded. This
  adapter correction was independently checked against the frozen Go runner.
- **Exact KMV capacity:** all three frozen collectors entered estimation when the
  retained heap reached4,096, even if no additional distinct hash had been seen.
  A4,096-distinct population with duplicate rows therefore disagreed with
  statistics.md§4.2. Local-only shared correction `dc130ecf` remembers overflow
  explicitly; all three supported cores pass the new boundary regression and
  their statistics suites. Swift implements the canonical rule and exercises
  the new shared case separately from its frozen baseline corpus.
- **Integer/decimal comparison bounds:** integer literals compared with decimal
  columns retain integer-to-decimal promotion rather than being adopted as
  decimal literals. Values and comparison types are unchanged, but the frozen
  contiguous-bound grammar admits only same-family literals/parameters and
  integer-width widening; a promoted integer remains residual. Preserving that
  resolution boundary makes `query/index_scan_vartail.test:136` charge its
  canonical 118 units instead of the incorrectly narrowed 33. No expected result
  or cross-core contract was changed.
- **Interval page estimates:** estimator.md §8.1 describes summing separately
  estimated canonical interval pages. The frozen Go `estimateScanCandidates`
  (`estimator.go:1459`), Rust `estimate_scan_candidates`
  (`executor/access_encode.rs:2507`), and TypeScript `estimateScanCandidates`
  (`executor.ts:32237`) instead apply the combined access selectivity to the
  access tree's node count and clamp once. The unchanged first query in
  `query/index_key_set_explain.test` requires that result: three estimated rows,
  two index pages, and total scan cost eleven. Swift preserves that observable
  estimate. Actual execution still sums canonical interval costs, charging only
  intervals started by a streaming LIMIT. The prose discrepancy requires a
  separate shared correction; no fixture or oracle expectation was changed.

- **Key-only index page fit:** `indexes.md` §4 and unchanged
  `dml/index_spilled_key.test` admit 4,500-byte secondary keys at an 8 KiB page,
  while `format.md` §6/§13 incorrectly applied the nominal table disposition
  target `RECORD_MAX(0)` as an index admission bound. At the frozen revision, all three native
  implementations leave key-only admission to actual formed-node page fit:
  Go `pMap.Insert`, `buildLeaf`, `buildInterior` (`pmap.go`) and `serializeNode`
  (`format.go`); Rust `PMap::insert`, `build_leaf`, `build_interior` (`pmap.rs`)
  and `serialize_node` (`format.rs`); TypeScript `PMap.insert`, `buildLeaf`,
  `buildInterior` (`pmap.ts`) and its node serializer (`format.ts`). An
  unsplittable/oversized candidate reaches the serializer's `0A000` page-fit
  guard before storage work. A one-entry leaf permits up to `C−8` key bytes,
  but copying that same key to a one-separator interior requires `C−12`
  (plain) or `C−44` (encrypted); admission can therefore depend on the formed
  tree. The shared prose correction documents this existing corpus and byte
  behavior; it changes neither format version, supported-core algorithms, nor
  golden bytes. The frozen implementation paths are corroborating evidence,
  not a replacement for the language-neutral contract.

- **Irreducible table-record page fit:** unchanged
  `ddl/views_long_definition.test` inserts into a 600-column `i32` table whose
  `RECORD_MAX` is zero at an 8 KiB page. The actual one-record leaf is 6016
  bytes (8-byte key + 2400 fixed-slot bytes + 3608 overhead), so it fits. All
  three frozen disposition planners return the remaining size after their
  compression/externalization passes without imposing a residual cap:
  Go `format.go:2885–3016`, Rust `format.rs:1387–1518`, TypeScript
  `format.ts:1240–1333`. Their serializers enforce actual page fit (Go
  `format.go:1114/1382`, Rust `format.rs:2158/2800`, TypeScript
  `format.ts:3315`). The corrected prose states the two-record-fit proof
  conditionally and records actual fit for irreducible rows. Swift's additional
  `K ≤ C/16` and residual `RECORD_MAX` admission guards were stricter than this
  shared behavior. No corpus expectation, supported-core algorithm, format
  version, or golden bytes change.

- **Poisoned page-service owner tenure:** `page-service.md` §5.6's phrase
  "stops serving and closes" describes the listener/client connections, not
  immediate release of the host handle's file ownership. Frozen Go
  `pageservice_owner.go` `stopServing`, Rust `pageservice/owner.rs`
  `stop_serving`, and TypeScript `pageservice/owner_worker.ts` poison handling
  all retain the file/presence until handle close or process death. The shared
  `service-owner-and-clients.sim` trace pins the resulting `08007` and owner kill
  set. Swift's eager `dispose(old)` allowed a client to promote during failed
  commit resolution and changed that transcript and later image history. The
  prose clarification changes no supported implementation or golden.

- **Credential barriers and adopted-generation state:** all three frozen key
  block writers (`shared.go` `writeKeyBlock`, `shared.rs` `write_key_block`, and
  `persist.ts` `writeKeyBlockSteps`) perform the prescribed key-page barriers
  without consuming the pager's adopted-generation stabilization state. The
  next data commit still issues its own pre-write stabilization. Swift cleared
  `recoverySync` during rewrap, shifting the first fault point in unchanged
  `encryption-rekey-rename-not-durable.sim` step 106 from pre-write stabilization
  to the post-meta barrier and admitting a different recovered generation.

- **ORDER-only subquery folding:** unchanged
  `subquery/values_order_by.test:93` expects cost 8 for two VALUES rows sorted
  by `(SELECT 1)`. The frozen initplan walkers omit query ORDER expressions:
  Go `exec_emit.go:1559–1610`, Rust `executor/exec_emit.rs:1426–1482`, and
  TypeScript `executor.ts:28768–28815`. Their remaining runtime subquery node
  charges one operator plus its child query for each sort input row. Swift's
  broader eager walk folded it once and cost 5. The experiment preserves the
  shared fixture with an ORDER-only runtime marker; a projection occurrence
  that already folds the same identity still supplies the folded value. This
  is a narrower implemented behavior than cost.md's general initplan wording;
  no shared corpus expectation or supported-core algorithm was changed.

- **Prepared subquery cache exclusion:** api.md §2.4 names an uncorrelated
  subquery as one reason a resolved plan cannot be reused. The frozen resolvers
  conservatively mark every subquery uncacheable (Go `resolve.go:1661–1665`,
  Rust `executor/resolve_func.rs:4616–4628`), including plain VALUES INSERT
  cache admission. Swift follows that existing exclusion. This also prevents
  cached correlated subquery plans from retaining execution roots or settings;
  detached metadata for eligible SELECT and INSERT plans binds fresh stores,
  meters, privileges and domain snapshots on every hit. No shared contract or
  expected result changes.

- **Direct cursor open charge versus enforcement (H07):** streaming.md §6
  previously claimed that a ceiling below the initial scan block aborts at
  cursor construction. Actual three-core probes contradict only that sentence:
  construction charges the block, while the first existing row checkpoint
  enforces it before `storage_row_read`. A missing point or `LIMIT 0` has no row
  checkpoint. The narrow prose correction changes no supported implementation,
  fixture expectation, format, or golden bytes.

  Probe setup: memory database, page size 512, one committed transaction creating
  `t(id i32 PRIMARY KEY, v text)` and inserting `(1,'abc')` through `(120,'abc')`.
  Each query uses a fresh session with `max_cost = 1`. All opens succeeded, and
  Rust, Go, and TypeScript produced these identical observations:

  | SQL | Cost immediately after open | First pull | Cost after pull |
  | --- | ---: | --- | ---: |
  | `SELECT id FROM t ORDER BY id` | 7 | `54P01` | 7 |
  | `SELECT id FROM t WHERE id=40 ORDER BY id` | 2 | `54P01` | 2 |
  | `SELECT v FROM t WHERE id=40 ORDER BY id` | 2 | `54P01` | 2 |
  | `SELECT id FROM t WHERE id=999 ORDER BY id` | 2 | clean EOF | 2 |
  | `SELECT id FROM t ORDER BY id LIMIT 0` | 7 | clean EOF | 7 |

  The probes used actual host cursors: Go `Session.queryValues`, `Rows.Cost`,
  `Rows.Next`/`Rows.Err`; Rust `Session::query`, `Rows::cost`, iterator `next`,
  `Rows::error`; TypeScript `Session.query`, `Rows.cost`, and
  `Rows[Symbol.iterator]().next()`. Each cursor/session was explicitly closed.
  Commands were invoked by temporary mise tasks `swift-native-cursor-cost-probe`
  and `swift-native-ts-cursor-probe`; their inner commands were:

  ```text
  (impl/go) go test -run '^TestSwiftCursorCostProbe$' -v .
  (impl/rust) cargo test --release --lib swift_cursor_cost_probe -- --nocapture
  (impl/ts) node tests/swift_cursor_cost_probe.test.ts
  ```

  Rust's temporary module was registered using the existing in-crate integration
  test mechanism (`autotests = false`). Probe sources and logs were retained at
  `/tmp/jed-cursor-cost-probe/`; all temporary repository test files, module
  registration, and tasks were removed after verification. These were probes,
  not additions to the supported-core test inventory.

  Existing frozen host tests also passed, via temporary mise task
  `swift-native-cursor-existing-probe`:

  ```text
  (impl/go) go test -run '^TestStreamingMidDrainCostAbort$' -v .
  (impl/rust) cargo test --release --lib integration_tests::streaming::mid_drain_cost_abort_surfaces -- --exact --nocapture
  (impl/ts) node --test --test-isolation=none --test-name-pattern='^streaming mid-drain cost abort throws during iteration$' tests/streaming.test.ts
  ```

  Those three existing cases each passed (3/3); the new five-observation probe
  succeeded in each core (15 matching observations). Relevant complete source
  files were byte-identical to frozen baseline
  `0b76dd6e9c2675b552eb8af9e820065351ffa4b3`, not inferred from later changes:
  Go `exec_scan.go`/`cost.go`/`streaming_test.go`; Rust
  `src/executor/exec_scan.rs`/`src/cost.rs`/`tests/streaming.rs`; TypeScript
  `src/executor.ts`/`tests/streaming.test.ts`. Construction sites are Go
  `exec_scan.go:478–511`, Rust `exec_scan.rs:1098–1140`, and TypeScript
  `executor.ts:24440–24466`. Go `cost.go:105–109` explicitly separates charge
  from enforcement, and the next-row checkpoint is `exec_scan.go:603`.
  Swift's host boundary tests cover open charge, deferred/fused leaf access,
  first-pull cost enforcement, missing points, `LIMIT 0`, and pin cleanup.

- **Host read-ahead and external-merge head accounting:** extensibility.md
  §4.2.1 says a streaming sort pull changes no query-memory reservation. Its
  remaining resident run does stay charged until emission ends, but the frozen
  spilled merge also try-reserves each replacement head before releasing the
  popped head (memory.md §6.8). These reservations never reject the query:
  a head that does not fit is held uncharged. All three cores perform this
  existing head schedule directly while filling the host read-ahead window;
  they do not move it to scalar replay. Swift preserves that behavior without
  inventing a deferred-reservation schedule. Relevant frozen sites are Go
  `spill.go:364–378` and the sorted emitter in `exec_emit.go`, Rust
  `spill.rs` `Merger::next` plus `executor/exec_helpers.rs` sorted emission,
  and TypeScript `spill.ts:256–267` plus `session.ts:820–822`. This is a narrow
  prose omission recorded for shared review, not a change to fixtures, costs,
  or the specified merge-head accounting.

- **MERGE source and hash-table lifetimes:** memory.md §5.3 named INSERT SELECT
  sources but omitted the same retained MERGE source owner. All three frozen
  cores keep the materialized source charge through writable WITH parts and
  outer EXPLAIN output construction. The target hash table is released at the
  end of MERGE, after RETURNING and pending-write admission, rather than at the
  end of matching. Evidence is Go `merge.go` (`executeMerge`, source
  `execQueryPlan` and function-scoped `defer hashed.release()`), Rust
  `executor/merge.rs::execute_merge_target`, and TypeScript
  `executor.ts::executeMergePlannedSteps` (source result and final
  `hashed?.table.release()`). The exact memory comparison found source-width
  deficits in `dml/merge.test` 57/59 and `dml/merge_explain.test` 10/22/24,
  plus the shortened hash-state lifetime in `dml/merge_hash.test` 6.
  The prose now names the existing shared lifetime; no expected result, peak,
  cost, fixture or supported-core implementation changes.

- **Pinned timezone predicates and conservative index matching:** valid stored
  `timezone` / `AT TIME ZONE` expressions retain their normal dependency pins,
  but the frozen structural matcher does not recognize their dedicated
  `AtTimeZone` node. This is an incomplete proof grammar, not an immutability or
  index-definition prohibition. Swift's unified resolved-call representation
  must not silently prove more partial-index implications than that grammar.
  Go `access_path.go:3296` (`rexprEqShifted`), Rust
  `executor/access_encode.rs:5972` (`rexpr_eq_shifted`), and TypeScript
  `executor.ts:35984` (`rexprEqShifted`) all omit the temporal node and fall back
  to false; their implication provers use that matcher recursively. In the
  frozen `ddl/index_dependencies.test` record 47, all three consequently use
  the primary-key-ordered scan (query-memory peak 0), while Swift's broader
  proof selected a partial index followed by sorting (peak 384). The narrow
  Swift correction preserves the frozen proof boundary and unchanged SQL
  results; no fixture, expected peak, dependency rule, or supported-core code
  changes. General structural-match coverage remains a separate shared audit.

- **Initial correlated SRF touched-mask scope:** the frozen planner visits every
  FROM function's arguments at correlation depth one when building the local
  touched mask, even the first function, which has no preceding sibling frame.
  Its enclosing-query ordinals can therefore mark unrelated local slots as
  touched. Go `planner.go:1703–1714`, Rust `executor/planner.rs:2364–2370`, and
  TypeScript `executor.ts:37246` share this conservative over-marking. For
  `types/record_unnest_coldef.test` record 14, outer `t.g` at ordinal 1 marks
  the local unnest text field at ordinal 1, retaining two extra text bytes
  (peak 258, rather than Swift's former 256). Swift preserves this bounded
  mask schedule at the initial function leaf without changing runtime column
  resolution or actual sibling references. It is a shared scope limitation
  for separate review, not a new type/column-resolution rule or an expected
  measurement change.

- **Floating overflow `DataTypeName` omission in Go:** the frozen implementation
  at `0b76dd6e9c2675b552eb8af9e820065351ffa4b3` used an untyped
  `overflowFloatErr()` in `impl/go/float.go`, whereas the corresponding Rust and
  TypeScript sites called their typed `overflow(type)` constructor. The
  canonical registry's `22003` template names `{type}`, and error-fields.md §5
  requires the typed range-error constructor to preserve that type in metadata.
  This is a shared-contract defect, not a permitted floating-point determinism
  exception: only numeric values/fold order have the documented float exemption.

  Before the correction, 29 public-API probes in each core (87 observations)
  confirmed 17 differing cases: `f32`/`f64` arithmetic, `exp`/`power` with either
  input width, `SUM`/`AVG` at both widths, `var_pop`, narrowing to `f32`, both
  decimal-to-float targets, and non-finite float-to-decimal conversion. Go left
  the field empty; Rust and TypeScript reported the result/target type. Both
  scalar-function widths report `f64`; `SUM`/`AVG` preserve their input width.
  All 12 control cases agreed, including typed float-text input overflow and
  underflow, generic operation underflow, generic math-domain/negative-factorial
  errors, and the distinction between generic `format` template-number overflow
  and typed `i32` dynamic-width coercion.

  The narrow correction changes only Go's `float.go` and `aggregate_stats.go`:
  all 12 finite-overflow helper calls supply their known width, and rejection of
  a non-finite float-to-decimal cast supplies `decimal`. Existing messages,
  SQLSTATEs, arithmetic, costs, error phases and generic underflow/domain/format
  errors are unchanged. Rust/TypeScript need no engine changes. Error-fields.md
  now spells out this constructor scope so the `22003` population-map row is
  not mistaken for a requirement to attach a type to every generic use of that
  SQLSTATE. The Swift experiment follows this explicitly recorded post-baseline
  correction; its frozen mechanical fixture fingerprint is unchanged.

  The existing three `error_fields` host-API suites now contain the same 31
  public draining-API cases, adding `degrees` and paired statistical moments to
  the diagnostic probe set. These fields are outside sqllogictest's SQLSTATE
  assertions, so this is the established host-test exception rather than copied
  SQL conformance coverage. All 93 case assertions passed via a temporary mise
  project under `/tmp/jed-float-fields-probe/`:

  ```text
  (impl/go) go test -run '^TestErrorFieldsFloatOverflowDataTypes$' -count=1 .
  (impl/rust) cargo test --offline --release --lib integration_tests::error_fields::float_overflow_data_types -- --exact
  (impl/ts) node --test --test-isolation=none --test-name-pattern='^error fields: float overflow data types$' tests/error_fields.test.ts
  ```

  The initial probes use external Go/Rust packages plus a TypeScript public
  tooling import and are retained with the SQL list and per-core transcripts at
  `/tmp/jed-float-fields-probe/`. Their task invocation was
  `mise run -C /tmp/jed-float-fields-probe --skip-tools probe`; the regression
  task used the same project with `verify`. Source inspection and the executed
  probes agree: frozen sites are Go `float.go:441,492,513,564,675,687,701,769,785,874,1081,1087`
  and `aggregate_stats.go:118,238`; Rust `executor/store_encode.rs:1580,1684,1690,1716,1754,1763,1792,1801,1873,1878,1964`
  plus `executor/aggregate.rs:936,1010`; TypeScript `eval.ts:3841,3909,4037,4040,4184,4212,4238`
  plus `executor.ts:41721,41816,42715`.

  After the correction, `mise run ci` passed **18/18 steps** (exit 0,
  119.2 seconds), including checked three-core conformance, all supported unit
  suites, the process corpus, simulation regressions and seeds `1..8`, browser
  and website tests, format/lint/type checks, and shared verification. Full log:
  `/tmp/jed-swift-native-default-ci-float-fields.log`. The normal gate still
  excludes the Swift experiment; its refresh is recorded separately.

- **Committed GiST loads retain a pending-write charge in all three supported cores:**
  the full debug and release V04 comparisons each execute 241,412 measurements
  and identify the same 376 differences: 188 records in disk mode and the same
  188 in disk-spill, across 22 files. All storage-byte measurements agree and
  all 21,477 memory records per core agree. Evidence remains at
  `/tmp/jed-swift-query-memory-20261106-939537-ml7e92/report.json` and
  `/tmp/jed-swift-query-memory-20261106-1135756-exgp9z/report.json`.

  The supported loaders eagerly reconstruct GiST leaf-key stores by calling
  ordinary insertion, which increments their cumulative staged-byte ledger.
  They publish those stores without clearing that ledger: Go `format.go:1854`,
  Rust `format.rs:3203`, and TypeScript `format.ts:4908` in the pre-correction
  sources. Swift builds the committed tree before constructing its store and
  correctly starts with no pending bytes. These are not optimizer-dependent
  lifetime differences: complete debug and release measurement files agree.

  Independent reviews agree this violates the existing contract, rather than
  defining a compatibility exception. memory.md §7 charges transaction-written
  versions and releases them at commit; §8.8 excludes resident committed GiST
  state from the query account. Loading a committed key is not a transaction
  write. The narrow correction clears staging only after the load-time GiST
  reconstruction loop in each supported core; actual index builds and mutations
  retain their existing charges. No Swift engine change is warranted.

  A new shared resource regression exercises range, scalar and exclusion-backed
  GiST indexes across per-record disk reopens. In particular, a zero-row
  primary-key DELETE under a one-byte query budget must not inherit committed
  index bytes. The unmodified Go actor reproduces `54P05` there while memory
  mode passes. Public host tests additionally exercise deserialization and file
  attachment, which ordinary SQL corpus records cannot express. Drafts and
  failing evidence are retained under `/tmp/jed-gist-reopen-regression` and
  `/tmp/jed-gist-load-staging`. After the service comparison passed and released
  its source-hash boundary, the exact reviewed patch and shared regression were
  integrated. All six host cases fail before and pass after in private peer
  checkouts; logs are `before-{go,rust,ts}.log` and `after-{go,rust,ts}.log` under
  that directory. Integrated default CI passes **18/18** after correcting a
  private-probe Cargo cache path and repository formatting, with log
  `/tmp/jed-swift-native-default-ci-gist-canonical-retry.log`. The complete
  corrected release V04 comparison now passes **241,412 measurements with zero
  differences** in all four cores and memory/disk/disk-spill modes. Evidence:
  `/tmp/jed-swift-query-memory-20261106-1996511-bxwolu/report.json`, log
  `/tmp/jed-swift-native-query-memory-release-gist-third.log`.
