# Java experiment verification

## Final correctness acceptance

All **42 of 42 gates pass** on frozen source
`419675aa4fa4b2afeaa9a645d6ebc3e68e38579e77858bb1816e12dd2848c5fc`,
against baseline `3e5962f9affc06c768df1688765162a65a893848` (format v53).
The three variants are Java 25, ordinary JDK 28 EA24 and the same JDK 28 with
three explicit value classes. Independent evidence review verified actor bytes,
all gate log hashes, inventory completeness and exact fault transcripts.

| Gate | Result for each of the three variants |
| --- | --- |
| Memory SQL | 764 passed, 2 canonical exclusions; 21,985 records |
| Plain file SQL | 679 passed, 87 canonical exclusions; 19,862 records |
| Encrypted SQL | 678 passed, 88 canonical exclusions; 19,827 records, independent Go cost/plan assertions |
| Host and byte fixtures | Complete host suite passed |
| Threaded stress | 12,000 Java + 4,000 Go snapshot checks; identical final checksum |
| Mixed processes | 31 scenarios × 16 Java/Go/Rust/TypeScript pairings: 496 passed |
| Shared fault traces | 19 saved + 66 generated: 76 exact comparisons and 9 intrinsic checks passed |
| Review regressions | 23 SQL records in memory and plain files; four exact simulation traces |

The default repository CI also passes all 18 steps, and all 17 Java generated
data catalogs verify. Committed [acceptance evidence](evidence/acceptance.json)
and compressed gate logs preserve the completed run; the final section below
records actor identities. Earlier sections are historical checkpoints, not
current failures or additional acceptance totals.

This final source includes the resident aggregate optimization. Its prior
`c06506ce` correctness acceptance is preserved separately under
[acceptance-before-aggregate](evidence/acceptance-before-aggregate/acceptance.json).

This verifies the frozen experiment on Linux x86-64. Performance evaluation is
recorded separately in [BENCHMARKS.md](BENCHMARKS.md). Production packaging,
other platforms and supported-core promotion remain separate decisions.

## Historical implementation checkpoints

The first full stable SQL inventories pass all applicable files. Memory and
plaintext runs share source
`817d39e29d34b17c517bdc138895905be63618594b78c5c760528313f5c043bc`:

| Mode | Passing | Failing | Canonically inapplicable | Records | Actor / log |
| --- | ---: | ---: | ---: | ---: | --- |
| Memory | 764 | 0 | 2 | 21,985 | `stable-20261114-63-b3nii`; `full-memory-fifteenth.log` |
| Plain file | 679 | 0 | 87 | 19,862 | `stable-20261114-63-6fct37`; `full-plain-second.log` |
| Encrypted file | 678 | 0 | 88 | 19,827 | `stable-20261114-1506549-ymujrv`; `full-encrypted-peer-first.log` |

The encrypted actor source is
`7a0080424d308bf79d052f66dad2ecc352cb970fc7244d5f65661a5c53bd41c2`.
Its costs and EXPLAIN rows are asserted against a complete passing independently
executed Go encrypted transcript from the frozen baseline. Peer evidence is
`.dev/java-native/encrypted-oracles/31930c56955f45a7c7635d19482d5eee94bf4452f95948b25cc5f1ea292b5b7d/`.
Other values/types/errors remain corpus assertions. Earlier encrypted focused
checks used plaintext cost assertions and do not replace this independent gate.

These are source-specific successes, not final acceptance: subsequent planner
review and storage-fault fixes require revalidation. Complete recovery,
all-variant verification and controlled measurements remain in progress.
All log names in this table are under `.dev/java-native/`.

Tenth complete memory inventory: **692 passing, 72 failing and two inapplicable
files out of 766**, with 19,797 records in passing files. Source
`ed1aafc13148ca68f5f46659ffbc2f023295f8e048b898406adb1d32b5a9e3b9`,
actor `stable-20261114-63-45p62m`, log `.dev/java-native/full-memory-tenth.log`.
Later focused fixes are not included in that total.

The ninth complete memory inventory: **663 passing, 101 failing and two inapplicable
files out of 766**, with 19,126 records in passing files. Source
`324ac4397082c373e3924a153f7ff89d3281c87a5d28cfc099f8fc2fb1aca1df`,
actor `stable-20261114-65-sg23cr`, log `.dev/java-native/full-memory-ninth.log`.
This predates subsequent view/schema, access-path and resource-accounting fixes.
The eighth inventory passed 642 files, failed 122 and excluded two (18,449 records),
source `d70cf28365da59aa88545765cfb8e775f3b377a08ce7dcbae2c5877cdafe6dc9`,
actor `stable-20261114-65-ttd85a`, log `.dev/java-native/full-memory-eighth.log`.
The seventh complete inventory passed 557 files, failed 207, and excluded two by
shared directives (15,378 records), source
`81a6e436c138fdef65bff50db17559eec1bc850a485037e126a8299975442362`,
actor `stable-20261114-63-u6ymvs`, log `.dev/java-native/full-memory-seventh.log`.
The sixth inventory (489 passing/275 failing/two inapplicable, 13,242 records)
remains historical evidence at source
`dc5bae06ddda5b17beeb1cd045f5de286450da77bb506981b5bf12a6a1db6287`,
actor `stable-20261114-63-hn282h`, log `.dev/java-native/full-memory-sixth.log`.
These inventories are incomplete acceptance evidence; later focused passes do
not alter their totals.

Focused encrypted write verification at source
`0fdbef18882dc4e39f35853cd0ab707bec361da36282800ddda4a9c87f4140e6`,
actor `stable-20261114-63-xad1ui`, passes UNIQUE (54), basic UPSERT (36),
UPSERT defaults (26), JSON updates (28), and short-circuit (30): 174 records.
The same invocation's partial UPSERT file reaches its final writable-view case
and fails there, so that file is not counted as passing. Log:
`.dev/java-native/upsert-second-encrypted.log`.

Encrypted index/schema verification passes create-index (37), expression-index
(36) and ALTER type oracle (17), 90 records total; evidence and actor identity
are in `.dev/java-native/index-schema-encrypted.log`. Foreign-key MATCH FULL
passes all 57 memory records at source
`bf248e4de693fe8e4d50dd3753ea00959421cb5c434224eed9ddaac4d6b3cb92`,
actor `stable-20261114-63-7nomdp`; other FK files in that run still fail.

Baseline: `3e5962f9affc06c768df1688765162a65a893848` (format v53).

Overall acceptance remains incomplete. Full stable SQL inventories have passed;
the remaining gates are tracked independently. Artifacts
under `.dev/java-native/actors/` capture immutable source, spec and generated
resources; these local artifacts are not published releases.

## Executed evidence, 2026-11-14

- Frozen inventory: 766 SQL files, 31 process cases, 19 saved simulations and
  145 database fixtures (24 integrity fixtures), within 1,212 spec files.
  Inventory digest:
  `5cbcf055e7ad24cb656c86f9e5b0bf5ebc441cd15c6a9da15841835cceaff7f8`.
- `mise run java-native:check` passed at source digest
  `e4dfcdf252704061e6aa789d962a4ba97412757eead3a91b6be9da0f0aa8e5be`,
  actor `stable-20261114-64-n547xf`. Checks include primitive values and exact
  arithmetic; parser boundaries; session/writer close races; 166 encoding
  vectors; spools and sorting; 15 table images and 16 exact leaf payloads;
  byte devices, pager and persistent-tree mutation histories; catalog/file
  lifecycle; five commit fault points; eight exact manifests; Linux host
  operations; and 21 shared plaintext recovery fixtures.
- The first complete memory conformance run attempted all 766 SQL files:
  **31 passed, 735 failed**, with 618 records in passing files. Source digest
  `ce604c88252594f3adb20fa6518b211de75e328d1014d148d558595818998c9a`,
  actor `stable-20261114-63-40qsjb`, local log
  `.dev/java-native/full-memory.log`. This is a coverage baseline, not acceptance.
- A subsequent complete `numeric_functions.test` passed all 91 records at
  source digest
  `57eb200373689c3ae03a32dba7e678b85b5492226ee13b1ac9cf0d5290f4b694`,
  actor `stable-20261114-63-nsjjyk`. Other targeted suites are continuing; their
  successes must not be added to the earlier full-run total without a rerun.

## Subsequent targeted evidence

These are source-specific slices; passing-file counts must not be summed into a
full-corpus result.

- Encrypted `column_default.test` and `column_default_expr.test`: all 45 records
  pass. Source `21d1d156751374f2ff3d0b32afb62e973e0d0c04bd926416a9577ec1515671d5`,
  actor `stable-20261114-63-2punyi`, log `.dev/java-native/default-encrypted.log`.
- Encrypted `check.test` (54 records) and `composite_default.test` (31 records)
  pass at source `6465da90de78c2deec97411554c9325ffcd0882944a0b04101b5a90bf0fc9503`,
  actor `stable-20261114-64-ytqqmw`, log `.dev/java-native/ddl-check-progress.log`.
  That batch also contains an inapplicable transaction-spanning file and a
  failing JSON DISTINCT cost case; the batch itself did not pass.
- Memory outer-level aggregate (11), grouped window (16), and window expression
  keys (12) pass at source
  `a6a174051553cb57a008f44425e64326b10916fec0295a0fb5ecb0e8f71cbf2b`,
  actor `stable-20261114-64-b6bj4v`, log
  `.dev/java-native/window-outer-aggregate.log`.
- The second broad memory run (`.dev/java-native/full-memory-second.log`) was
  interrupted while resolving deep nested operands in the resource suite.
  It is incomplete and does not replace the first complete inventory. A resolver
  fix now passes all 17 operand cases and all 49 row-valued subquery cases at
  source `b70f94e72c81e40b90ebe629dbb077127478914094e9552291c48d53df3ca69a`,
  actor `stable-20261114-63-fs9z4t`, log `.dev/java-native/operand-row-progress.log`.
- The third complete memory run attempted all 766 files: **252 passed, 512 failed,
  two inapplicable by shared directives**, with 5,786 records in passing files.
  Source `0453009925f97bd0e0b85283b3099bab41ebc3d36f5f95e917b28b4e3e99ac2a`,
  actor `stable-20261114-63-epogg0`, log `.dev/java-native/full-memory-third.log`.
  This is the latest complete inventory, not acceptance.

Durable/encrypted full corpus, cross-core interchange and process/concurrency
acceptance, complete fault traces, EA/Valhalla engine comparisons and benchmark
acceptance remain outstanding. Host storage tests do not establish those gates.

### Sequence, identity and catalog progress

- Sequence value and DDL files: 88 records pass in memory, source
  `066380f23392107b09512462e2fd0fa15b794e7b3715901778589cc1b8718fa3`,
  actor `stable-20261114-63-jk4fxq`, `.dev/java-native/sequence-second.log`.
- Host checks include independent sequence histories, failed-statement rollback,
  savepoints and read-only transactions, plus byte-exact round trips of 45 catalog
  fixtures including views, sequences, serial and identity. Full host check passes
  at `7746547c4b43759540c9da5df4aafd49b78eac6e2a9cc33e1b01ee5724147fc1`,
  actor `stable-20261114-63-p10jwp`, `.dev/java-native/identity-host-check.log`.
- Serial, identity and typed-sequence files: all 83 records pass with encrypted
  cold reopens, source
  `d1c3934a7922df46252197ee3e73b10ab862c99b215641192fbc9fc4f7f501f8`,
  actor `stable-20261114-63-nj1njk`, `.dev/java-native/identity-encrypted.log`.
- Identity ALTER, transaction, and jed-specific files: all 160 records pass in
  memory, source `3242b3d4c42e53ed6872d3ed8e3b71039ef1fc32c4807ca189b8778ef78b245b`,
  actor `stable-20261114-64-fqdsdk`, `.dev/java-native/alter-identity-third.log`.
- The fourth broad memory run (`.dev/java-native/full-memory-fourth.log`) was
  interrupted while planning the deep-view expansion suite. It is incomplete
  and supplies no replacement aggregate result; bounded planning remains work.

### Ordered index storage and mixed process checks

- Complete host checks pass at source
  `11ead189cf5c5ed69ab785f7ef76dbb708bac8151b7513cb092fdda974bc1613`,
  actor `stable-20261114-63-6vk667`, log `.dev/java-native/index-host-check.log`.
  Coverage includes 50 canonical catalog images and exact index leaf payloads,
  bounded forward/reverse tree cursors, and plaintext/encrypted index COW,
  retained reader pins, rename, drop, reopen, and exact live-page accounting.
- `mise run java-native:page-service` passes the socket host checks and 22 focused
  Go/Java cases at source
  `8fd3dd230dfec6ff968b9dfee69b5db77b9287f78266759da6f9b291f0f610f7`,
  actor `stable-20261114-276407-9if2vf`, log
  `.dev/java-native/index-page-service.log`. The peer is the immutable Go actor
  at baseline `3e5962f9affc06c768df1688765162a65a893848`.
  Cases cover both owner directions, plaintext/encrypted files, alternating
  writes under old reader pins, owner loss, retained data keys after credential
  changes, lost commit acknowledgements, voluntary and read-only handoff, plus
  unique/expression/partial ordered index mutations, deletion/key reuse and
  cold reopen. The frozen-peer read-bound compatibility exception and evidence
  are recorded in the experiment charter.

These focused cases do not replace the full shared process/concurrency/fault
acceptance matrices. GIN/GiST, relaxed durability batching, and remaining storage
host surfaces are still unfinished.

### GIN storage and initial query slices

- Full host suite passes at source
  `980db582f5973aa1f06a5fe18c77f61078e1ecceb00a431dc8979b943d2b2cc6`,
  actor `stable-20261114-63-rgkapy`, `.dev/java-native/gin-host-timezone.log`:
  59 canonical catalogs, including all eight GIN fixtures and timezone index
  dependencies; 13 shared posting-run codec/build/mutation vectors. GIN term
  extraction is checked against canonical postings, not only catalog bytes.
  Tree structural estimates are tested with leaf reads explicitly forbidden
  after opening and cache eviction.
- The 22 mixed Go/Java cases also pass with array and JSON GIN maintenance,
  alternating updates and deletions in plaintext and encrypted files, at source
  `653f2c372c00d2b64322b94b1486b0db03ea2e4221302c8e2e4e987687f71f68`,
  actor `stable-20261114-293924-nsv5bo`, `.dev/java-native/gin-page-service.log`.
- `gin_scan`, `gin_mutation`, `gin_any_eq`, and `gin_array_eq` pass all 51 records
  in memory at source
  `373e43b48df7c1f736497f72338e4a6c4b6cee8e466d4168693010fa43e39a11`,
  actor `stable-20261114-63-tuhlr`, `.dev/java-native/gin-scan.log`.
- Encrypted `gin_jsonb` passes 58 records at source
  `8298f1be5b9502ea52b9d786e03b32042baa4de654023723e21302f9e953154d`,
  actor `stable-20261114-65-72c0da`, `.dev/java-native/gin-more.log`.
- Encrypted `gin_jsonb_jsonpath` and `create_gin_index` pass all 40 records at
  source `eb36b70a9a82abd6a8ed92fdde0e1f5f74897460c5035d60f30bc91fe0376124`,
  actor `stable-20261114-64-3xd6z2`, `.dev/java-native/gin-jsonpath-second.log`.

GIN scans include posting runs, canonical skip-scan peeks and leaf groups,
multicolumn tags, disjunctive arms, and normalized JSONpath term trees. These
slices do not establish complete GIN planner, memory-budget, or EXPLAIN parity.
Some broader runs still fail residual expression costs and collation harness
support. The passing-file counts above must not be added into a corpus total.
# Foreign-key action follow-up

## Writable view and exclusion follow-up

Encrypted complete view files pass: basic writes 100 and view UPSERT 63 at source
`b1c0c589dee8026d47e335561cbd1236e82fc167b718784ec4e9dbc8709012af`,
actor `stable-20261114-63-tu4pv8` (`view-check-second.log`); check option 124,
ALTER VIEW 52 and defaults 76 at source
`f8d7cc80a1f0b0dca7de9ed029da8c13688eca840fc142df73403535dfa0e55c`,
actor `stable-20261114-63-gdudtu` (`view-alter-first.log`).
The latter run still fails the separate rename file's dropped-column alias case.

Exclusion UPSERT and rekey files pass 45 encrypted records at source
`59f11545c9e4a3b623bec1a9b645748a3ae8fd97767cd8867f36943640ce5d36`,
actor `stable-20261114-63-hio75` (`upsert-exclusion-first.log`). B-tree and partial
exclusion files pass 83 records at source
`e86ebb2fca34b281ba232642fb058bc539fbf2aacb4d029989f7dccce6afc048`,
actor `stable-20261114-63-w9rnjv` (`exclusion-alter-second.log`). ALTER TYPE/PK
passes 57 encrypted records at source
`6e7aee7cec85ab7f58436f01a8b14c00a2631c2b474d74580fedb2ae37108f93`,
actor `stable-20261114-64-akcosp` (`alter-constraints-third.log`). These runs
leave other ALTER constraint cases failing; none represents full schema parity.

Encrypted targeted verification passes the complete keyed-actions (61 records)
and referential-actions (101 records) files. Source:
`23776d27c7e51848b8cd0056ad26709475d86e1dda792b845be89595f5b146d4`;
actor `stable-20261114-63-u2vik7`; raw log
`.dev/java-native/foreign-keys-keyed-third.log`. This covers keyed reverse probes,
fallback scans, end-state action closure, many-parent mapping and ordinary cascade
assignment validation. Earlier encrypted cross-type, MATCH FULL, large-child and
lazy-child checks passed 283 records at source
`d383b1d11215f9389174bbde222c732f257d3cec2f6a70b3802d6f81e00b5058`,
actor `stable-20261114-63-pbsywr` (`foreign-keys-third-encrypted.log`).
These focused results do not establish full foreign-key, ALTER or writable-CTE parity.

### GiST and collation storage checkpoint (2026-11-14)

The host inventory now reads and exactly re-encodes 67 shared catalog images,
including the three collation-reference images, FK metadata, and range/scalar/
exclusion GiST images. GiST tests rebuild the canonical resident bounding tree
from its leaf set, allocate its existing pages in post-order, and compare each
whole plaintext node page against the shared image. This covers page types 5/6,
component bounds, child ordering and node grouping rather than catalog metadata
alone. Evidence: `.dev/java-native/gist-host-fourth.log`, source
`74f7b92c7dc05f90c3f6283b72ba919e580f37b159d36374c9bfc20d912b2c49`, actor
`stable-20261114-65-gbj1ga`.

Both complete standalone GiST query suites pass in encrypted mode (38 records),
including exact descent costs, scalar/range matching, empty ranges, NULLs and
insert/update/delete maintenance. Evidence: `.dev/java-native/gist-query-fifth.log`,
source `14310e57f8f0d5ff921a7b3603cf108f6d20d42f895c24a23455f00a8b8f044f`,
actor `stable-20261114-65-8ig6yc`.

The 22-case mixed-process inventory also passes with GiST range/scalar indexes
added to its four existing index cases: both owner directions, plaintext and
encrypted files, alternating peer updates, and old Java snapshots retained
through the updates. The client now retains an independent owner read pin from
its write-base acquisition until local post-commit reconstruction completes;
commit acknowledgement releases the owner writer pin before that reconstruction.
Evidence: `.dev/java-native/gist-page-service-second.log`, source
`26ae4594a7ce720c370d67a181e0d81e3635f0de8f91d2db7a2f1a0a00a138b1`, actor
`stable-20261114-355705-4dtla7`. This adds client coordination only; the owner's
foreground transaction path remains free of coordination syscalls.

GiST follows the frozen spec's explicit resident-tree and whole-GiST-rewrite
scope; ordinary table data remains paged. Exclusion SQL integration, join-bound
selection, full memory-limit acceptance and broad process/fault inventories are
not established by this checkpoint.

### Storage ceilings and packed memory checkpoint

The complete `resource/storage_file.test` passes all 35 disk records in
`.dev/java-native/storage-file-fifth.log`, source
`611fb45fd83093b492c7e5be72e7215fd0ea5d8a897166fcbfa6de6ccd021f53`, actor
`stable-20261114-63-yka9jo`. This includes exact live-page trip points, index
construction, delete recovery and GiST pages. The shared encrypted exclusion is
preserved. Fixing unchanged-primary-key UPDATE to replace its existing leaf row
was required to preserve the canonical tree shape and live count.

The full host check passes in `storage-limits-host-sixth.log`, source
`7c5089079c9b106cefcc979c7c5d8cdefb5b2c9fccdce4db1c8804e2193be4b0`, actor
`stable-20261114-63-c4zrb5`. `StorageLimitTest` covers database-owned setters,
custom memory page sizes, the initial commit, exact packed-memory batch limits,
failed COMMIT cleanup, deletion/reuse, retained old readers, byte-for-byte
unchanged files after rejection, and the non-persisted limit. Memory databases
now commit to a segmented byte device with clean paged roots and a watermark-gated
RAM free list. The complete shared memory-storage file still requires attachment
and temporary-table support; its host-tested main-domain scenarios are not a
claim that the whole file passes.

### MERGE and authorization first verification

- `.dev/java-native/merge-second.log`: source
  `0d062a9a2a5160f3aa843ea5c6c9193da4d3ef8f088cf3ffb1b211cf9f1ecbe8`,
  actor `stable-20261114-64-g72xxv`. Five full disk files passed 195 records:
  `merge`, `merge_errors`, `merge_constraints`, `merge_join_source`, `merge_hash`.
  The run also exposed the privilege-harness and ambiguous-alias gaps, so its
  seven-file invocation was not a full pass.
- `.dev/java-native/merge-view-first.log`: source
  `cac55a26cd54a86b9dc838fe369901357b4456e4e7e515bf89ee2dcf95c692ef`,
  actor `stable-20261114-64-4s230s`. All 42 encrypted records passed in
  `merge_view`, `merge_ambiguous_alias`, and `merge_params`.
- `.dev/java-native/privileges-first.log`: source
  `8df2d1c8c544bbfa2428daf8116d89b2ed32c212b2e5b56f0fc55ce118fcf9b5`,
  actor `stable-20261114-64-4otbxt`. The three session privilege files passed
  103 encrypted records. `merge_jed` exposed duplicate-grant harness handling;
  it was not counted as a pass.
- `.dev/java-native/foreign-keys-deferred-first.log`: source
  `5e925099dfcbf85b7a4c227901919c83d20d96d0fc848ea456ee339135f2e283`,
  actor `stable-20261114-63-gbmmq1`. Four complete encrypted files passed 195
  records: writable-CTE foreign keys/exclusions, cross-type keys, keyed actions.

No full parity, performance result, or promotion is established by these runs.

### Query planner and session host checkpoint

- `gist-join-sixth.log`: source
  `04be02a20789c83694c35a53eb1803986b3ea24693af4924d100fe5908455af0`,
  actor `stable-20261114-64-2ixmtd`. Four complete encrypted files passed
  48 records: GiST range/scalar index nested loops and INNER/LEFT joins.
- `grouped-wholerow-first.log`: source
  `438cec814fc664b7e269df4ccc9d8441bf19a397337a9448ccc4252b312268b2`,
  actor `stable-20261114-63-f8ef2j`. Whole-row references and JSON-path
  variables passed all 153 encrypted records, including grouped whole rows
  and aggregates inside SQL/JSON PASSING.
- `index-order-third.log`: source
  `15fd16aec1dd7c57feee47a9f3a09eff21135053fa724c1977337edc48e71b20`,
  actor `stable-20261114-64-8k0gwt`. Three complete disk collation-index
  files passed 43 records. The same invocation failed the separate ordered
  index-scan file; that file passed all eight records in
  `index-order-fourth.log`, source
  `be2be5f6c1deff27f254f4f02f30f6054f57a76e856deef7e4bd631fe0eea957`,
  actor `stable-20261114-79-ox0qe7`.
- `view-alias-fix-second.log`: source
  `2b39979b4c95d69a5a899b11db85d5d79fff571b158c9b69bbb5028ba2ed25b8`,
  actor `stable-20261114-76-yi82wd`. The complete view-rename file passed
  all 59 memory records, including correlated references after alias freezing.
- `session-host-first.log`: source
  `20583da58a49728f50a4f34aeb4d0fc7abb8cacba8a8e3f3f46c0c617ab03324`,
  actor `stable-20261114-64-2alp4n`. SessionVariablesTest and HostFunctionsTest
  passed before the ongoing domain-routing integration failed a later
  credential-change check. This is explicitly not a full host-suite pass.

These focused checks do not replace the complete frozen inventory. Plain MERGE
EXPLAIN and hash matching are being integrated; EXPLAIN ANALYZE remains an
explicit unsupported surface until actual execution attribution is connected.

### Integration checkpoint before usage limit

- Full host check passed: `.dev/java-native/domain-integration-host.log`, source
  `31587c713d81651d27016eb76c44346e9458934a29ae137e721d9ff5059ddde0`,
  actor `stable-20261114-64-53gahr`.
- `.dev/java-native/domain-integration-corpus.log`, source
  `bae0e4a251d83c21adf86f543a03c4c2c1a682b82f5dfbd4ad43f9e2a20cd375`,
  actor `stable-20261114-64-4bp1nx`: seven full encrypted files passed 244 records
  (three session privilege suites and MERGE basic/jed/view/hash). MERGE EXPLAIN
  reaches ANALYZE and fails explicitly with `0A000`; the invocation is not a pass.
- `.dev/java-native/domain-temp-first.log`, same source, actor
  `stable-20261114-82-d1ks9f`: no full files passed. Temp-table execution reaches
  the cross-kind FK error diagnostic (`42P16` expected, `42P01` returned).
  View follow-on files require missing `jed_tables`/`jed_columns` relations.
- `.dev/java-native/merge-when-order.log`: source
  `e33428078cbe67d45a1aad9df866a5d2e5a3e990b613233176292c17135c3300`,
  actor `stable-20261114-64-7ebyni`: filter-conjunct order and NULLIF pass all
  64 encrypted records after preserving MERGE WHEN evaluation order and excluding
  the NULLIF grammar form from EXECUTE grants.

The account usage limit stopped the three parallel implementation agents during
this checkpoint. The core remains incomplete; remaining gates are listed in
PROGRESS.md. No dependency installation by the maintainer is outstanding.

### Resumed domain integration

The parallel implementation resumed after the account-limit checkpoint above.
These focused memory results remain independent of the complete inventory:

- `domain-first-resumed.log`, source
  `4c588cd5e656cb1070a2f4210631b107be11781d87cee356f4cb9e5ff9dcda3f`,
  actor `stable-20261114-64-e70b2c`: temp tables, qualified temp names and qualified
  foreign keys pass all three files and 84 records.
- `domain-third-resumed.log`, source
  `68817915f62ef929510406fc829a8b72cdcb1ece41b06f22afd355a841e82b83`,
  actor `stable-20261114-64-4k1qgt`: temporary serial/identity, foreign keys,
  ON COMMIT, indexes, composite dependencies and attachment constraint names pass
  all six files and 262 records.
- `domain-views-second.log`, source
  `0bd71c8b3c5b102391ca448672d821d1e22d445f75e59d47b03ee2c603b460d1`, actor `stable-20261114-64-jq5blp`: temporary views pass
  40 records, including cross-domain writes, dependencies, renames and ON COMMIT.
  The other three view files in that invocation still fail; it is not a passing
  batch. Later changes require their own verification.

### Attachment/temp storage checkpoint (2026-11-14)

The full frozen `resource/storage_memory.test` passes all 48 records, including
independent attachment limits and failed multi-root publication. Evidence:
`.dev/java-native/domains-storage-memory-second.log`, source
`af430994664d5ca88d5e25bb446f71ae14db94c7e00d5a003a0c3f817cc6e64a`, immutable actor
`stable-20261114-64-99bssa`.

`AttachmentHostTest` passed with the complete host suite in
`.dev/java-native/attachment-host-first.log`. It covers host attach/detach and
read-only checks, attachment snapshot retention, independent file page sizes,
attachment-only commits leaving main byte-identical, failed memory/file domain
admission publishing no roots, and plaintext/encrypted temporary pager migration
with retained snapshots and bounded caches. These checks do not replace the
shared concurrency or process inventory.

The focused socket suite and 24 Go/Java mixed cases passed in
`.dev/java-native/attachment-page-service-first.log`, source
`c663cf622581f9704a510b9e19e2c4696191c5ccfa07c5e1692b6f245594f2e7`, actor
`stable-20261114-702150-mmj16k`. Two cases add cross-owner attachments with Java
owning the first or last canonical path, writer queueing, and retained attached
readers across peer commits. The full 31-scenario process driver remains to be
run; compact/backup/rekey/stream transports and relaxed durability remain
outstanding storage acceptance work.

### Catalog and statistics checkpoint (2026-11-14)

The computed catalog relations now cover tables, columns, indexes, constraints,
sequences, composite types/fields, views and statistics, with ordinary domain and
privilege resolution. ANALYZE collects bounded priority samples, exact/KMV NDV,
MCVs, histograms and lossy GIN term counters. Facts are transactional, durable,
retained stale after changed rows and consumed by metadata-only scan/index,
group and equality-join estimates. GIN candidate pricing is shared by EXPLAIN
and execution. This is incremental evidence, not full planner acceptance.

- `statistics-fourth.log`: source
  `6edc2304e4532ebcff05d49a032b1766adbcf240b074ca3571641537c34436e3`,
  actor `stable-20261114-64-xetf8c`: full memory statistics58, transaction21,
  scoped statistics18, SQL-input-limit9 records pass. The estimates file in that
  invocation failed and was subsequently fixed.
- `statistics-eighth.log`: source
  `fe1d1ab9e6f08498ada689b9e0c9c69c4f49afcc83a079b6a52ab09d84f485ca`,
  actor `stable-20261114-64-dezuhf`: statistics_estimates_p9 passes all39 memory
  records, including 30,000-row sample bounds, a 100,000-row skewed population,
  histogram intervals, stale NDV scaling and parameter-sensitive access.
- `statistics-durable.log`: source
  `6734e923d76c5b695eec1494d5e59fcf4ae55dc3b7e559c521b2a4c820f9ffc1`,
  actor `stable-20261114-64-wixxzu`: statistics58, GIN statistics40 and RETURNING57
  records all pass encrypted with per-record reopen.
- `statistics-second.log`: source
  `cf3f5b275fac7fe20d9d448a6eb5fea26b011346382a5dfe603b50cd86d1d844`,
  actor `stable-20261114-63-zpjbtw`: KMV boundary14 and catalog view-rename27
  records pass encrypted; other selected files were failing or mode-inapplicable.
- `statistics-fifth.log`: source
  `8ce04a86d1e97da78e090d39731e3b44c80cd2836f1bcdb12c41a2236efd5a1d`,
  actor `stable-20261114-64-wvzohw`: corr20, regr_count12 and view table-function41
  records pass memory after operand widening and final-mask dependency capture.
- `statistics-sixth.log`: source
  `8c6f6344b32e8079a0d64241b65ddaada81aad3148bc2ac45eef2ea3880283b0`,
  actor `stable-20261114-63-hu4x4j`: covar_pop/covar_samp/regr_avgx/regr_avgy/
  regr_slope all pass encrypted, 100 records across five complete files.
- `statistics-seventh.log`: source
  `c4e204d081b1eb05f9d6c435a531ff64a3deb4e6af8c0cb704aeaa553dc12001`,
  actor `stable-20261114-64-nii213`: index_key_order passes101 memory records,
  including rejection of explicit ASC in GIN/GiST keys.

The standalone CatalogCodecTest passed all72 shared catalog images, including
three statistics catalogs, using build `stable-compile-20261114-64-efc8bj`, source
`ce2398ffb98f3cfc22f66d213bb4d863ff1b91d4229acc73f5ef0db74887be47`.
Full host acceptance was not renewed by `statistics-host-second.log`: it reaches
AttachmentHostTest and fails its active sequence-domain case. General EXPLAIN
ANALYZE and the remaining planner corpus are still incomplete. The opt-in
`java-native:process` task now runs the immutable frozen31-scenario driver with
Java-only or Java/Go pairings; its existence is not a claim that the matrix passes.

### Measured EXPLAIN ANALYZE checkpoint (2026-11-14)

The first measured execution slice passes all eight records in the frozen
`query/explain_analyze.test` on encrypted storage. Source identity
`65d99a8af1ae01dac6acc49a2ba2b2ad4d0622c3d4bc5493d1cbf8d7d57133ea`, immutable
actor `stable-20261114-79-300wqb`, log `.dev/java-native/analyze-writes.log`.
The cases verify separate EXPLAIN outcome cost, scan/filter/aggregate cumulative
execution costs, and an analyzed INSERT that persists. Costs come from meter
phase deltas; estimates are never substituted for execution measurements.

The broader follow-on is still incomplete. The next captured actor
`stable-20261114-64-30y3wc` (source
`d625268bc26f0a34ebe23328c91473f8c2cded2cff9a157a3cfa1462f355c7b5`) passes the
basic eight-record suite again and advances `query/explain_followon.test` through
unused CTE, derived-frame, and hidden scalar-subquery attribution, then fails at
line 265 because verbose SQL/JSON expression printing is not connected. This is
progress evidence, not a passing-file claim. Other uninstrumented operators
continue to reject ANALYZE before execution.

Additional complete encrypted files: `query/gin_jsonb_explain.test` (16 records),
actor `stable-20261114-64-jfooly`, source
`0d36f822b50ebf932a8cd51726973cfcb42f0dd0e01927e9275c782afe675cfc`;
`query/gin_disjunction_explain.test` (19 records), actor
`stable-20261114-64-vtpn6v`, source
`a22cb17bd874320bf551259ea9f3423d243dcf2b8f026fb84288f962c64fd81f`.

Storage checkpoint: `.dev/java-native/publication-skeleton-host.log` passes the
complete host suite, including 72 catalog fixtures and 16 full plaintext image
fixtures. This follows the fix to prepare committed interior skeletons before
releasing a client writer pin. The frozen `service_client_churn` scenario passes
all four Java/Go pairings in `.dev/java-native/process-client-churn-second.log`,
source `0bd18731f23fc53d72c3a5b6f72d4620d427d86efad31d911e9ce4214bb8c01f`,
actor `stable-20261114-828075-4sk8q8`.

The later focused page-service run `.dev/java-native/reuse-history-fifth.log`
passes 24 Java/Go lifecycle/index cases and exact whole-image comparisons after
each of ten large allocation-history commits and fourteen small-page COW commits.
Source `1f4a26612a751376721e284fff530a6b04fa0d7388edc8736bd915bcdfdaab29`,
actor `stable-20261114-875970-x40gdm`. These checks include recovered-file
reclamation and retained-parent remaps when a child returns to its original page
number. They do not establish full COW/process parity.

The frozen encrypted-service scenario passes all four Java/Go pairings in
`.dev/java-native/process-rekey-first.log`, source
`186b0533c5f73999d44063e03ac340871a1164fb96ee065e4cced40cd487ee43`, actor
`stable-20261114-871939-2rg21m`. Rotation now uses a target-layout scratch rebuild,
then atomically replaces the image; the process test verifies idle stale clients
fail `28P04` and reopen under the replacement credential. The shared rekey golden
and host admission tests are added but not yet claimed passing here.

## Access and syntax follow-up, 2026-11-14

- `partial-eleventh.log`: source `993edc6134aa9f784184f74d1191971b383504b865ea0581a4c133c15acc6024`, actor `stable-20261114-65-6ebnvb`. Complete passing files: query/index_partial_full_scan.test (memory): 15 records. Other files in the same invocation remain failing.
- `tuple-implication-first.log`: source `499dce963c6e07b4737507687d308011bbd69a9dee83e30d1151817deb483a16`, actor `stable-20261114-63-qokcon`. Complete passing files: query/index_partial_implication.test (memory): 29 records; dml/insert_on_conflict_decorations.test (memory): 91 records. Other files in the same invocation remain failing.
- `tuple-correlated-third.log`: source `a0bc3e404ac5c21c25f875e38a4e248900da3fb4269754ca45b5a002f0a91bfb`, actor `stable-20261114-63-qbig6z`. Complete passing files: query/index_key_set.test (memory): 49 records. Other files in the same invocation remain failing.
- `parser-access-eleventh.log`: source `a9f1f110a32666d379f29e9cc3db261f9dca9c8a8730bc283790c0e7ca0865c1`, actor `stable-20261114-63-kcn7ho`. Complete passing files: joins/parenthesized.test (memory): 49 records; ddl/select_into.test (memory): 40 records; dml/insert_select_setop.test (memory): 70 records; joins/qualified_star.test (memory): 15 records. Other files in the same invocation remain failing.
- `index-limit-twelfth.log`: source `0d94508b3f48ca2666cdbb771497d85105a346ae81a4dcff5d100cc68ce548b3`, actor `stable-20261114-63-rr6rgf`. Complete passing files: query/table_column_alias.test (memory): 20 records; collation/collated_index_desc.test (memory): 14 records; collation/collated_index_vartail.test (memory): 11 records. Other files in the same invocation remain failing.


## Access paths, DISTINCT and VALUES follow-up

At source `ed1aafc13148ca68f5f46659ffbc2f023295f8e048b898406adb1d32b5a9e3b9`,
actor `stable-20261114-63-ex0f9i`, the focused memory run passes DISTINCT (52),
GIN multi-column (24), GIN disjunction (25), GIN JSONpath EXPLAIN (14), and
DISTINCT ON EXPLAIN/ANALYZE (6): 121 records. Its subquery point-set file still
fails later; log `.dev/java-native/access-distinct-seventeenth.log`.

At source `023de67a4611b8540bca618fbf83127ed27d561e7f08b8ef79e54e73fb5354d8`,
actor `stable-20261114-64-gfm495`, all subquery point-set (24), index key-set (49)
and partial access-path (16) records pass: 89 records. Log
`.dev/java-native/subquery-index-eighteenth.log`.

At source `fc55460f81a1ae9fb516abf6fdba72cc29e8fbd48497e6683eed823051bb41d9`,
actor `stable-20261114-64-cb7ffx`, all VALUES query (79), INSERT VALUES expression
(61), INSERT set-operation (70), and jed-specific view (174) records pass:
384 records. Log `.dev/java-native/values-insert-twentieth.log`.

Storage/process checkpoint (2026-11-14): complete frozen process matrix **124/124
PASS**, all 31 scenarios across Java→Java, Java→Go, Go→Java and Go→Go. Source
`4599564c626dd6997e92a04a4204b9c87e78d4ffe0ad5fe0c31c9c90eb7e2327`,
actor `stable-20261114-994275-qwqjqi`, log
`.dev/java-native/process-full-fifth.log`; saved committed images are under
`.dev/java-native/process-fifth-evidence`. This includes both stream scenarios,
relaxed clients, encrypted lifecycle, attachments, backup/compaction and exact
allocation histories. The previous 114/124 result is superseded only for this
process inventory.

The complete host check passes at source
`3edb3f559f5aa2d5528a362b809d352bb07602a772b9862a21443ba02e17dcaa`,
actor `stable-20261114-64-qbfwti`, log `.dev/java-native/relaxed-host-second.log`.
New storage assertions cover cumulative relaxed manifests, unchanged durable
floor meta, loss of an earlier pending page, one-barrier batch flush, and revocation
after a failed flush. RekeyTest compares the entire result to
`encrypted_rekey_index_table.jed`, and exercises protected scratch spill and
atomic storage-limit failure. This remains host evidence, not a completed saved
fault-trace inventory.

All seven frozen concurrency SQL schedules pass 72 records each in sequential
and per-session threaded memory execution (144 executions). Source
`3ade187542c0416d235334d12b02181e6d4aaa7cca4ad3c7e9d765ed974c267f`,
actor `stable-20261114-64-ic4rj4`, log `.dev/java-native/concurrency-third.log`.
The scheduled blocked writer executes the real blocking acquire in threaded mode.
Two malformed-storage files pass three records at source
`fa66e9d2f496946a338ccc1321004d3546840eb37988936c02247c1323097600`,
actor `stable-20261114-63-e8zp2q`, log `.dev/java-native/malformed-storage-second.log`.

### Query planning and measured writable CTE follow-up (2026-11-14)

The encrypted CTAS EXPLAIN/ANALYZE suite passes all 34 records, including writable
CTE sources, with the ordinary atomic mutation path and captured execution
checkpoints. The basic EXPLAIN ANALYZE suite passes eight records in the same run:
`.dev/java-native/explain-writable-ctas.log`, source
`d92a4bbfe9ed5786965cff5a0cf30ac91769720a46df959b8d1069f1b86ee3f6`, actor
`stable-20261114-65-cawsul`. This is scoped evidence, not complete ANALYZE coverage.

Merged USING/NATURAL namespaces are retained when validating pre-join WHERE
filters: `ddl/views` (110) and `query/table_column_alias` (20) pass encrypted at
source `dec443c9a80c362d3bf7907cb3208479919f445f04242a2fadd813a4e23b75cb`, actor
`stable-20261114-64-vln379`. `ddl/select_into_jed` passes all 34 applicable memory
records at source `408f165b2e4b88323203128e794f73bb7a0ae5ff13e2420687082c2cff0902b2`,
actor `stable-20261114-64-i953xb`.

Resolved non-trapping cast/function classification, parameter-time LIKE proof,
value-preserving widened set-arm filters, and two-base-relation reversed INL
selection pass three complete encrypted suites: `joins/pushdown` (12),
`joins/safe_conjunct_pushdown` (30), and `params/like_pattern_proof` (13).
Evidence: `.dev/java-native/inl-where-bounds.log`, source
`a4f8e324760794f5f1bd5602cd890a61d841e3e8c9df0f3d7cc65537e55f0698`, actor
`stable-20261114-64-y4lnxv`. The same run still fails promoted key-source joins;
N-way join search and further EXPLAIN metadata/checkpoints remain incomplete.

## Access and EXPLAIN follow-up (2026-11-14)

These focused results postdate the tenth complete inventory and do not change its
reported totals. All are complete memory-mode files through the public cursor.

- Source `1c33721d59d3c6b4da52af9b925b976c3916301e95c51e0a4a5268a84629a4d1`,
  actor `stable-20261114-64-1z6r49`: composite key-set EXPLAIN (15) and RQG
  subqueries (150) pass. The latter verifies local-name shadowing in bound-only
  resolvers. Log `key-estimates-twentyfourth.log`.
- Source `d85f8163ddd4fc393464b071cfad304104d4e731bdb9f89951d3252dca0a2ac8`,
  actor `stable-20261114-64-pw08td`: access cost selection (19) and EXPLAIN
  follow-up (67) pass, including execution-derived index-order checkpoints.
  Log `key-estimates-twentyfifth.log`.
- Source `bfe09c844a760d17454eb180074733d057459a28fc0ea8786e981a433863b661`,
  actor `stable-20261114-64-h53hyx`: ANY-array EXPLAIN (31), P6b access
  selection (43), mutation access selection (27), DML EXPLAIN (38), and
  index-key order EXPLAIN (19) pass. Log `key-estimates-twentysixth.log`.
- Source `c6725056e0297b34f57f5b4b1aa63d9c9ab81e0e31ff2f8d49c78769a7159a63`,
  actor `stable-20261114-64-s6xc3f`: promoted key sources (21), partial-index
  EXPLAIN (8), and EXPLAIN shapes (38) pass. Log `access-twentyeighth.log`.
- Source `caf1e6c1ee7d4af84f631d9cc45f8de478da6703782060fde1e7f2dc0b3c3717`,
  actor `stable-20261114-64-c59w9g`: basic EXPLAIN (16) passes, including
  unbound parameters. Log `explain-twentyninth.log`.

Logs are under `.dev/java-native/`. Other files in those invocations still fail;
these results do not certify general planner parity. A later broad resource run
exposed a native Java stack overflow while planning a valid deep view chain.
Caching parsed view syntax removes repeated parsing but does not yet establish
stack-safe planning; that acceptance gap remains open.

## Complete memory inventory and join search follow-up (2026-11-14)

The twelfth complete inventory attempted all 766 baseline files: **749 passed,
15 failed, two shared-mode exclusions**, and 21,596 records in passing files.
Source `3a1aefa76e07923693c53251e5e43c2f95e3a9eab6c296cf0076ea46f12797b7`,
actor `stable-20261114-63-5sv0m4`; log `full-memory-twelfth.log`.
This is incomplete acceptance. The previous eleventh inventory passed 724,
failed 40, and excluded two (20,527 passing records), source
`d3205763301985b83b3d5842405956d86c6d635c7ac9434298d6a2f39a941aca`.

Later focused memory checks pass the complete P7/P8 costed-join files (32/94
records), USING (39), NATURAL (31), and filter-conjunct metadata (7).
The `island-sixth.log` capture passes nine complete files, 253 records: ordered
index and interval metadata, composite/array sets, parameter custom plans,
P6b access selection, general estimates, comma joins and index-join top-N.
Source `7f4a78ca4a03f419ce17b9850ac7ca90d9bc925f71be42024b6c55711e224b2b`,
actor `stable-20261114-64-t4lrlv`. These focused results do not replace the
complete inventory. Independent review and broader storage-mode checks remain
necessary, including the full join-candidate inventory and logical-cardinality
frontier beyond cases exercised by these files.

### Deterministic local fault replay checkpoint (2026-11-14)

The Java runner executes each complete trace twice, compares exact transcripts and
byte-device calls, and independently runs the frozen Go trace actor. Its simulated
byte host models pending writes, barriers, crash-selected sector survival and
ordered directory publication. Oracles cover pinned-reader contents, acknowledged
and attempted version windows, independent attachment publication, reachable pages,
row/key/index consistency, live-page counts and independently reopened main images.
Main commits also check reclamation bounds, reuse before growth and trailing
high-water/truncation rules. Physical fault positions excluded by the canonical
simulation contract are not eligible for exact transcript claims.

Six traces pass exact peer comparison with source
`4bc1b1ce30ab03084b86f396fe591b3f0c55104ca14c06027cdee1bb6bcc8fdd`,
actor `stable-20261114-64-ph0pdq`: `analyze-then-drop-table`,
`compaction-crash-before-dir-sync`, `relaxed-close-truncation-floor`,
`delete-empty-unary-subtree`, `stale-root-leaf-chains`, and
`delete-merge-separator-overflow`. The same actor also passes the complete
`relaxed-attachments-close-order` trace. Logs are
`.dev/java-native/simulation-<trace>-growth.log` and
`.dev/java-native/simulation-attachments-first.log`.

`relaxed-deadline-flush` passes exact comparison with source
`48708804fb9ad2792d0fb3aaef944e02a5300384dc43e73cb9bc49781500aa24`,
actor `stable-20261114-79-tl5nfs`; log
`.dev/java-native/simulation-deadline-second.log`.
`relaxed-floor-unsynced-compaction-rename` passes exact comparison with actor
`stable-20261114-63-tfrzj7`; its retained `actor.json` records the full source hash;
log `.dev/java-native/simulation-floor-first.log`.
Every trace retains both transcripts, full trace text, images and immutable actor
identity under `.dev/java-native/simulation-evidence/<trace>/<actor>/`.

The full host suite passes source
`565aec5c4e8bd13edb5a0db5851fa4f6f9d75f121d45bf8268e8b5fd64aa0e1f`,
actor `stable-20261114-64-uda025`, log
`.dev/java-native/simulation-crypto-host.log`. This includes canonical absent
`affectedRows = -1`, key-copy fault ordering and derivation-before-writer-gate
checks. A later compacted-meta slot regression and UPSERT physical-placement fix
are still undergoing fault replay; this host result does not cover those later
changes. Encrypted full replay and service-mode simulation remain incomplete.

The complete 403-step `encryption-rekey-rename-not-durable` trace subsequently
passes exact frozen Java/Go comparison with actor `stable-20261114-64-hlo8wh`,
log `.dev/java-native/simulation-encryption-fifth.log`. The trace exposed and
verified four production corrections: stabilization before repeated credential
writes; derivation before attempting the writer gate, including unsuccessful busy
calls; consistent selected/floor meta slots after image replacement; and preserving
physical unchanged-primary-key UPSERT placements. Its retained actor identity
records the precise source hash. This makes ten complete local saved traces; it
does not include the nine service-role saved traces.

`relaxed-attachments-close-order` also passes after extending both the reclamation
and independent reopened-image checks to every attachment, actor
`stable-20261114-65-yjs6sc`, log `.dev/java-native/simulation-attachments-second.log`.

### Lateral scopes, joined access and measured plans (2026-11-14)

The frozen full files `joins/lateral` (34 records), `joins/parenthesized_lateral`
(41), and `joins/on_pushdown` (23) pass encrypted with source
`8a7060484f5aaaf2fffad1351260a9f84ef3668fb68c736a0c5d10a3c8b794a3`,
actor `stable-20261114-64-gkjrm7`; log `.dev/java-native/lateral-fence-check.log`.
The corrections preserve sibling ambiguity precedence, enclosing-query correlation,
and the two-input legacy hash gate inside wider outer-join fences.

`resource/query_memory` passes all 51 memory-mode records with source
`bdf9babbf1779fd7494523e78edd70ed32020fc55a7772f9a60ca2d6aec3bef8`,
actor `stable-20261114-64-5b2486`, log `.dev/java-native/left-memory-check.log`.
The LEFT probe threshold correction restores canonical staged INL selection; it
introduces no relaxed accounting or corpus exceptions.

`query/where_rewrite_explain` (21), `query/cte_window_pushdown_explain` (15), and
`joins/gin_index_nested_loop` (13) pass encrypted with source
`f386477965e25719d3111104422e423efe18e0066a59622bbb1f68dfe40c40ac`,
actor `stable-20261114-64-hd4pn8`, log `.dev/java-native/metadata-twentyfirst.log`.
This includes contradiction metadata and measured zero-work branches, window-body
Filter attribution, materialized CTE reference filters, and costed constant-GIN
versus sibling-GIN selection. That run also attempted MERGE metadata, which still
failed its writable-CTE case; the later completed MERGE result is recorded below.

The opt-in `java-native:simulation-sweep` task exports the frozen generator,
captures or validates one immutable actor, retains complete traces and hashes,
and reports every failed trace. It supports saved regressions and profile/seed
ranges with explicit diagnostic step overrides. Its smoke check used the complete
canonical `compaction` seed 1 generation with a **five-step diagnostic override**;
that Java/Go comparison passes with actor `stable-20261114-65-yjs6sc`.
Evidence is `.dev/java-native/jed-java-simulation-sweep-20261114-65-2h66e8/`;
this short tooling smoke check is not a full-profile acceptance result.

The complete `dml/merge_explain` file passes all 24 encrypted records with source
`704918683d51710e04feb8648d02c07bd89573963fc1f315e1804215fb650a45`,
actor `stable-20261114-66-x2055w`, log `.dev/java-native/merge-metadata-final.log`.
MERGE ANALYZE executes the ordinary mutation path and captures actual source,
matching-loop, probe-scan, unmatched-target and statement costs, including MERGE
inside a writable CTE. These checkpoints use meter deltas, not estimated costs.

### Saved service-fault inventory checkpoint

The second complete frozen saved-fault inventory passes **12/19 exact** traces;
seven fail. Evidence: `.dev/java-native/simulation-saved-second.log` and
`.dev/java-native/saved-sweep-second/manifest.json`. Immutable actor
`stable-20261114-66-dxwgc`, source SHA-256
`b7ef5d2adef9dae1193cad160cebc6713c3a2b566a2ddbeef4072a48924dd841`.
This includes all ten local traces plus encrypted current-key backup and the full
304-step `service-owner-and-clients` trace. Each accepted trace runs twice against
Java for transcript/byte-device determinism before exact frozen Go comparison.

Later focused `encryption-service-reconnect-keeps-held-key` passes with actor
`stable-20261114-64-jv45os`, source
`d390690fba33b4f4232f96df4e2ba3526331a79e6f759650874ef9e455ec1781`
(`simulation-service-fixes-second.log`). A disconnected Abort cleanup frame now
releases local writer state without making an otherwise empty COMMIT fail; EOF
also releases the owner's gate, matching the supported peers' best-effort Abort.
The host regression injects that lost frame into a real owner/client transaction.
`service-owner-lost-unstabilized-generation` also passes in the later focused
`simulation-service-ownerlost-second.log`; its immutable actor identity remains
in the corresponding evidence directory. These focused passes do not revise the
12/19 complete-inventory count. Outstanding service and seeded faults remain
required acceptance work.

## Stable SQL milestone and JVM comparison checks

At source `9120893e6701317b857fb718f86513c49495e7dae7273dbee5bb277a5c6ae13d`,
ordinary JDK28 and the explicit value-class variant each pass all 764 applicable
memory SQL files (two canonical exclusions), 21,985 records. Both also pass all
679 applicable plaintext files (87 exclusions), 19,862 records, and all 678
applicable independently checked encrypted files (88 exclusions), 19,827 records.
Actors are
`ea-20261114-63-spxv6t` and `valhalla-20261114-63-2ouag1`; logs are
`.dev/java-native/full-{memory,plain,encrypted}-{ea,valhalla}-first.log`.
Both host suites and all three storage modes of threaded stress pass as well
(`host-ea-first.log`, `host-valhalla-third.log`, `stress-ea-first.log`,
`stress-valhalla-first.log`). This is an all-mode checkpoint, not final-source acceptance.

At the same source, actor `stable-20261114-1785002-sfwaam` passes the canonical
threaded balance-transfer profile in memory/plain/encrypted modes: four
contending writers complete 1,000 transactions while eight readers check
4,000 separately pinned snapshots per mode. The independently pinned Go runner
adds 4,000 checks. All four final checksums are `f3e75496a3111181`.
Evidence: `.dev/java-native/stress-stable-20261114-1785002-tvhgug/report.json`;
profile digest `474d387221395587874f488b5de52e9cac2d6001f93d64ad56834a9b5abb67d8`.
This tests the shared unordered stress contract, not merely a sequential replay.

The normal repository CI run completed all 18 steps. Seventeen passed, including
all supported-core checked conformance, unit/CLI/migration tests, verification,
formatting/lint, real-process tests, saved/generated fault traces and metamorphic
sweeps. The TypeScript browser suite failed one cross-realm worker startup.
A focused rerun passed all 20 browser tests. Logs:
`.dev/java-native/default-ci-first.log` and `default-e2e-ts-rerun.log`.
This is a failed aggregate run plus a successful focused rerun, not an invented
all-green single CI execution. No Java task is part of the default gate.

The live PostgreSQL18 oracle matches the declared locale, encoding, timezone
and session profile. `corpus:check query/distinct.test` regenerated byte-identically,
but emitted warnings after a replay transaction had aborted; those later records
are not independently oracle-verified by this run. The persisted planner review
corpus has a separate clean byte-identical PostgreSQL result recorded below by
its owner. The frozen Go core returns an incorrect row for its always-false ON
regression; Java keeps PostgreSQL's empty result and does not weaken the check.

### Join suffixes and independent planner review (2026-11-14)

The stable Java actor `stable-20261114-64-pkm2x1`, source SHA-256
`9120893e6701317b857fb718f86513c49495e7dae7273dbee5bb277a5c6ae13d`, passes
all 104 query files (2,612 records) after the join suffix, namespace, statistics
and hidden ON-subquery estimate changes. The complete log is
`.dev/java-native/island-hidden-query-seventh.log`. The preceding statistics
checkpoint also passed all 34 join files (620 records). Join search now keeps a
fenced prefix fixed while selecting the following eligible base-relation suffix;
resolved reference tracking includes whole-row and correlated subquery references.

The separate experimental review corpus
`src/test/resources/review/planner.test` preserves predicate ownership, physical
ordering, whole-row/correlated references, and fixed-prefix USING/CTE namespaces.
Its 23 records pass Java memory and durable plaintext execution and the configured
PostgreSQL oracle (`planner-review-memory.log`, `planner-review-plain.log`, and
`planner-review-pg.log` under `.dev/java-native`). It is additional review evidence,
not an addition to the immutable 766-file acceptance inventory. The frozen Go
peer fails its first constant-false ON query by returning a row; PostgreSQL and
Java return the required empty result. This peer discrepancy is retained in
`planner-review-peer.log`, rather than changing expected results or declaring a
successful peer comparison.

Restoring attached-table primary-key order recognition also passes the complete
507-event generated `attachments-1.sim` trace with exact frozen Go comparison;
evidence is `.dev/java-native/simulation-evidence/attachments-1/stable-20261114-64-ky949z`.
This preserves the attachment access restriction while allowing its existing
ordered scan to stop at LIMIT. Aggregate-stage hidden subquery cost provenance
has not yet received the same independent selection audit as ON and ungrouped
projection subqueries; these checks do not establish that broader claim.

### Four-core process matrix (2026-11-14)

The same stable actor `stable-20261114-64-pkm2x1`, source `9120893e6701317b857fb718f86513c49495e7dae7273dbee5bb277a5c6ae13d`,
passes all **496 process cases**: 31 frozen scenarios across all 16 ordered
owner/client pairings of Java, Go, Rust and Node/TypeScript. This includes the
peer-to-peer controls as well as both directions of every Java pairing. The
unmodified scenario assertions and final-image byte comparisons all pass;
strict core selection prevents role substitution.

The opt-in command is `mise run java-native:process --cores java-native,go,rust,node`.
Peers are exported from baseline `3e5962f9affc06c768df1688765162a65a893848` and built
offline from existing approved caches. Node's installed package lock must match
the frozen lock before its existing packages are reused. No packages were
downloaded for this matrix. Log: `.dev/java-native/process-four-core-first.log`.
The `actors.json` manifest and per-scenario committed images under
`.dev/java-native/process-four-core-evidence/` preserve the Java artifact hashes,
peer toolchain/source/binary identities and compared images. JVM actor commands
disable HotSpot perf-data files to prevent sandbox PID collisions from injecting
diagnostic text into deterministic transports; transcript output is not filtered.

### Index mutation arrival order (2026-11-14)

The generated `ddl-churn-1` trace exposed a physical mismatch in a non-unique
index after a multi-row UPDATE: values, costs and live-page counts agreed, but
Java removed all old index entries before inserting replacements. The canonical
non-PK path completes each row's removal/insertion before the next row. Ordered
and GIN maintenance now preserve that order; final-state UNIQUE validation still
allows valid swaps. PK-assigning UPDATE vacates affected rows first, including
an identity assignment, then reinstalls them in scan arrival order. UPSERT
maintains its inserts before updated entries and uses the two-pass path when any
updated row changes storage key.

The new experimental `src/test/resources/review/index-maintenance.sim` checks
these physical contracts, including an index-driven UPDATE whose arrival order
differs from primary-key order and mixed unchanged/rekeyed UPSERT rows. Its
45 events pass exact frozen Go comparison in both plaintext and encrypted modes
on actor `stable-20261114-65-s3d4wi`, source
`1ffde044ddc6f43f014a439519b0fbcfcbbf87c0fb9e63bdce110b3cba586dfe`.
The earlier actor `stable-20261114-63-9l2sul` fails this regression at the first
non-PK UPDATE image. Logs are `index-maintenance-review-seventh.log` and
`index-maintenance-encrypted-final.log` under `.dev/java-native`; immutable
transcripts/images are in its `simulation-evidence/` tree. The original
304-event DDL-churn trace also passes exact comparison on actor
`stable-20261114-63-sgogy1` (`index-mutation-ddl-final.log`).

The final actor passes all 41 applicable durable DML files (1,688 records, one
canonical exclusion), recorded in `index-mutation-dml-final.log`. The preceding
arrival-order snapshot passes pending-query-memory, general-query-memory and
rekey-savepoint files: 173 records (`index-mutation-memory.log`). Arrival keys
are bounded physical pending-set bookkeeping, uncharged under memory.md §7.1;
their spool retains the handle's scratch host, encryption policy and independent
entropy stream. The added review trace is separate from the frozen acceptance
inventory and does not change it.

## Complete saved fault acceptance and remaining generated faults

The fourth complete saved inventory passes **19/19 exact frozen Java/Go traces**
at source `32368fdd809eb955543495d546d39185f06aa429dcf500c5c442f148a20e6153`,
actor `stable-20261114-63-9l2sul`. Evidence is retained in
`.dev/java-native/saved-sweep-fourth`, with aggregate log
`simulation-saved-fourth.log`. This replaces the earlier incomplete saved-inventory
checkpoint, without rewriting that historical evidence. The same actor passes the
complete 503-event `encryption-replay-1` profile, including pointer authentication
and backup restore. Generated profiles remain a separate incomplete gate.

Two storage corrections were necessary. Owner service commits retain each surviving
free page's retirement generation, instead of assigning every free page the newest
client transaction. This preserves safe reuse under older retained readers. Direct
non-serving attachments conservatively disable reuse while another reader is pinned,
matching the frozen peers' common-reader gate. The complete 507-event attachment
trace and 303-event service-backup trace now compare exactly.

The encrypted manifest-reservation loop now seals each candidate free-list chain
before retrying with a larger manifest reservation. Frozen Go `validated_commit.go`,
Rust `format.rs` and TypeScript `format.ts` all consume these real candidate nonces,
even when the candidate is discarded. Skipping them preserved logical contents and
recovery but displaced later key entropy by 24 bytes. This is a frozen serialization
compatibility detail; the shared format and entropy contract are unchanged.

A package-private `ScratchHost` factory now supplies disposable byte devices to the
existing row-spill and temp-domain codecs. Native operators retain private scratch
files and close cleanup; temp pagers retain unlinked files. The simulation factory
keeps scratch outside durable state and injects canonical lane-11 create/read/write
errors. The complete 303-event `spill-1` profile passes both cores' intrinsic oracles
at source `ca5769b95d6a1894770aa1f940b85a0bf553cede7555b543b2d21900f8e0739e`,
actor `stable-20261114-64-m3s40y`; actual injected failures return `58030` and recover
only recorded durable contents. Exact transcript comparison is inapplicable under
simulation-testing.md §6.3, not silently skipped. The later native scratch lifecycle
refinement passes the complete ordinary host suite at source
`474dcf1846918557e9e7fca3e7dc40ca57240fac94ce6d10b60bc5a37be39ecd`
(`simulation-scratch-host-second.log`).

### Generated fault corrections after the second sweep

The second complete seed-1 sweep recorded **16/22** passes (14 exact, two intrinsic)
at source `32368fdd809eb955543495d546d39185f06aa429dcf500c5c442f148a20e6153`.
Its complete logs and failures remain in `.dev/java-native/sweep-second`.
Later focused corrections must not be added to that inventory's count.

The encrypted attachment trace now passes all 502 events exactly at source
`d89eb342d7a7627e71fca77f0b4fff1e2033fe8d3e5d3f8269cfb6d3c97bf6d2`,
actor `stable-20261114-65-2eivzy`. A credential barrier with no pending commit
must preserve the alternate meta-slot choice established by equal-generation
initial metas; it must not reset that choice to the tie-selected physical slot.
`DurableCommitterTest` now covers this invariant. Attachment fault recovery also
checks old/new credential atomicity and records the raw recovered image before
writable key-block repair, just as the shared driver does.

The encrypted service trace passes all 302 events exactly at source
`5afb8591d07bd200f2ddaf9149f1bf43b358e70d4846eddcff6ac579df5bb0f1`,
actor `stable-20261114-64-7ydd8s`. The driver now reads the refreshed generation
after its digest query and clears a prior step's credential-attempt state before
manual process kills. These are harness fixes, not core error suppression.

The service attachment trace passes all 302 events exactly at source
`eea94276c132356594ffde5486747c421c44cd02daa82194239338de5f4110fa`,
actor `stable-20261114-64-yz6ahe`. Full reclamation now requires the reader
watermark to equal the new transaction, as transactions.md and incremental-reclaim.md
require. Only the writer's own explicit base pin is excluded; another reader at
the previous committed generation blocks the rebuild. A subsequent injected owner
maintenance write failure can occur after the client commit is acknowledged.
The driver then uses the shared narrow `write`/`08006` rule: recovery must prove
that acknowledged transaction, rather than requiring a read from the poisoned
owner. The ordinary post-publication oracle remains active otherwise.

Full generated seeds 1–3, saved traces and host checks are being rerun together;
these focused results do not claim the combined inventory is green.

### Empty-body ANALYZE publications (2026-11-14)

Repeated ANALYZE of an empty one-column table can publish a new generation with
zero changed body pages and a zero-entry manifest. The committer now admits that
case while retaining generation, dependency, page-identity and write-count checks.
Previously the third transaction failed before publication. No independent pending
flag was added: relaxed durability retains the canonical body-entry pending set.

The persisted review traces `review/analyze-empty.sim` (34 events) and
`review/analyze-empty-relaxed.sim` (79 events) both compare exactly with frozen Go,
in plaintext and encrypted variants. The relaxed trace includes a batch, close,
compaction, repeated reopen and power-loss recovery. Source
`21eacb4c8ea05de03ed03c99ac5e9b81e198d891890a23011c3985d71492822a`, actor
`stable-20261114-63-qr8sd5`; logs are `analyze-empty-second.log`,
`analyze-empty-relaxed-third.log`, `analyze-empty-encrypted.log` and
`analyze-empty-relaxed-encrypted.log` under `.dev/java-native`. Encrypted traces
were derived by adding the canonical encryption header to the persisted input;
all transcript and image checks remain enabled.

The saved `service-attachments-owner-and-clients.sim` now passes all **304 events**
with exact frozen Go transcripts and recovered images at source
`9dcf50fbae2e10aebba2e1a9f832e71469bba93ef26bb0447de81742bb3643b2`, actor
`stable-20261114-64-mm5ax9` (`service-attachments-review-third.log`). The host-test
published-generation accessor now reads the already-published storage meta rather
than the owner's lazily refreshed SQL snapshot. A remote attachment commit had
correct contents but a stale cached generation, causing the oracle to compare it
with the preceding generation. This accessor performs no storage or protocol I/O;
the pending-content and independent image checks remain enabled.

### Generated seeds 2–3 storage fixes (2026-11-14)

The complete 66-trace inventory exposed additional failures on its frozen actor;
these focused results resolve specific failures and do not yet assert that the
combined inventory passes:

- `attachments-3.sim`: all 503 events match exactly, source
  `2d803b48919f4528c909f62a3b32a37e17cc3eb44391fc0b21a3980b177425c9`, actor
  `stable-20261114-63-24li17`. Stabilized prior-manifest dependencies that are no
  longer reachable may enter the new free list. They remain unavailable for reuse
  in that same commit, and pending batch dependencies remain protected.
- `compaction-2.sim` and `compaction-3.sim`: all 303 events each match exactly,
  source `a618161b0f64cd33f0c6f79ef7c5c7aebff4b3ac289c86b696edc0edbc649dc6`, actor
  `stable-20261114-63-5b5cz5`. The file-host temporary-image seam creates or truncates
  a leftover image directly, without an extra directory deletion. The difference
  matters to directory-journal prefix survival after interrupted replacement.
- `encryption-service-3.sim`: all 307 events match exactly, source
  `8ac7c6f14a64d6ffe2eb6673fbef6a5ccd892d0fedddfc5aaca02325d47ee9fb`, actor
  `stable-20261114-64-9j5msg`. Credential changes first flush the relaxed data batch
  before starting independent key-copy barriers. The harness also includes
  attachment generations in implicit-spawn events.
- `encryption-service-2.sim`: all 304 events match exactly, source
  `039742b44b89018ba16679de413b75b2f45afe924868d09d03501a0bb971eed6`, actor
  `stable-20261114-64-cae6c9`. A promoted owner holding only the retained DEK adopts
  the selected key block without repairing a differing pair, as encrypted.md §14
  requires. Credential-open still repairs the pair. Unchanged attachment commits
  compare their pending logical contents with the prior digest without introducing
  fresh read-back protocol frames; changed commits retain the read-back oracle.

The corresponding logs are `simulation-attachments-third-fixed.log`,
`simulation-compaction-second-fixed.log`, `simulation-compaction-third-fixed.log`,
`simulation-encryption-service-third-fixed-second.log`, and
`simulation-encryption-service-second-fixed-second.log` under `.dev/java-native`.
Each complete trace retains its exact Java/Go transcript and raw recovery images.
A host regression additionally verifies that retained-DEK promotion preserves the
interrupted key-block pair while a subsequent credential-open repairs it.

The generated `service-3.sim` also passes all **304 events** exactly against frozen
Go (`service-three-review-second.log`, actor `stable-20261114-64-y4f5q3`, source
`e9715c795513339cd498887065a0e7fdf0bc770f4c75e5bd8a9d845fff44cf6e`). Its
oracle now obtains the durability floor from the serving owner. A client's local
committer reflects the acknowledged generation, while only the owner knows whether
that generation remains in a relaxed batch. Previously the harness incorrectly
required an unflushed generation to survive owner loss and later power loss. The
corrected floor preserves the shared recovery window and changes no engine storage
or durability behavior.
The same actor also passes generated `service-relaxed-3.sim`, **402 events**, with
exact frozen Go comparison (`service-relaxed-three-review-first.log`). Its former
recovery-window failure had the same client-versus-owner floor cause; no additional
change was required.

The first combined seeds 1–3 inventory completed **56/66** on source
`bcc7c250751a7fd00739c7a430b73605727ed9b6f47766a53abfdb0b282f172c`, actor
`stable-20261114-63-uj4i7`: **47/57 exact** and **9/9 intrinsic**. Evidence is
`.dev/java-native/sweep-third`, with summary `simulation-sweep-third.log`.
Its ten failed traces are attachments-3, compaction-2/3, encryption-service-2/3,
growth-3, service-3, service-relaxed-3, and truncation-2/3. All ten now have
complete focused passes documented here; the combined rerun remains outstanding. The later ordinary host suite passes at source
`b203bae596ad7a46db275051608648de57b2841fde1aa42453cd2090a1fd5c29`, actor
`stable-20261114-63-dxzi69`, log `storage-fault-fixes-host.log`.

A subsequent saved sweep on source
`1697434ee1637fd56b85411567cc96d053659d4958a04f7cdf79088b55088036`, actor
`stable-20261114-63-svq83p`, passed its first 17 complete exact traces, then correctly
aborted when the sweep task changed during execution. This is not a completed
19-trace result. Evidence remains in `saved-sweep-sixth`; a stable-tooling rerun is
required (`simulation-saved-sixth.log`).

### Leaf residency, separator history and deferred truncation

Decoded immutable table-leaf entries now share the lifetime of their authenticated
pager page. The cache accepts only the identical decoder instance, so another
schema or snapshot bound cannot reuse its interpretation. Eviction releases the
entries with the page. Host tests cover repeated decoding, eviction and an
incompatible decoder. The full host suite, all 764 applicable memory files
(21,985 records), and all 678 applicable encrypted files with independent Go
cost/plan assertions (19,827 records) pass at source
`7dc4e3e33dbf4f92cf64bb6f4eb60ce1aeedf9091d36ca177c519a3e257cd260`, actor
`stable-20261114-63-4914io`. Logs are `decoded-leaf-check-second.log`,
`decoded-leaf-memory.log` and `decoded-leaf-encrypted.log`. Allocation measurements
and their limits are recorded separately in BENCHMARKS.md.

The generated growth-3 trace passes all 502 events with exact Go images at source
`dfa2ed91cf048099be8ab87698bafb129fa2ea6104b7914524bdaa981d417fc6`, actor
`stable-20261114-63-lzby9g` (`growth3-separator-history.log`). A changed separator
set invalidates retained-interior eligibility for the entire mutation history,
even when a later mutation restores the same bytes. The persistent tree now keeps
the monotone `sameKeys` guard specified by deferred-interior.md. The subsequent
full host suite also passes (`separator-history-host.log`). Independent review
found no additional cache or separator-history defect.

Repeated ANALYZE of an unchanged populated table still publishes its changed
statistics domain. A byte-identical body does not make that logical write a no-op.
The complete growth-1 trace (503 events) and persisted review trace
`repeated-analyze.sim` (19 events) compare exactly with frozen Go at source
`bb419355430f9a41763ee510bd958adb74350a673a402d271eb4ecc2ddc7404c`, actor
`stable-20261114-63-ltvb0x` (`growth-analyze-publication.log`). Empty transactions
and zero-row DML retain their ordinary no-op behavior.

The generated truncation-2 trace passes all 502 events exactly at source
`962faf5efe886d147d9e5726370202a6bd20875e84c363026df01800b85d33ec`
(`truncation-second-shrink.log`, evidence `truncation-second-shrink`). Local relaxed
commits remember a lowered page high-water until a durable barrier, including an
automatic batch-cap barrier, then trim excess physical allocation. Page-client
barriers leave that owner maintenance deferred. This fixes the shared physical
slack invariant without moving truncation ahead of durability. The later
truncation-3 correction is recorded below; final combined validation remains required.

Generated `truncation-3.sim` passes all **503 events** with exact frozen Go
transcripts and images after correcting the reuse generation of batch-held
orphans. Those pages become reusable from the generation that first lists them
as free after the batch flush, not the earlier generation that orphaned them.
Previously a reader pinned at the intervening generation allowed two free-chain
pages to be reused too early in the canonical allocation schedule. The one-page
resident free residue keeps its existing generation. Evidence:
`truncation-three-review-second.log`, actor `stable-20261114-64-4x6t1w`, source
`31a6131bc7761baf38115ac24e3e9e77c8cff3047e5825075bd9c1aa514c4a48`.

### Default repository gate and generated data

The complete second `mise run ci` passes all **18 of 18 steps**, including checked
Rust/Go/TypeScript conformance, unit and host checks, mixed processes, 19 saved
fault traces, the 176-trace default sweep, PostgreSQL corpus probes, website checks
and both browser suites. The TypeScript browser suite passes all 20 tests. Log:
`.dev/java-native/default-ci-second.log` (154.3 seconds). This supersedes the
earlier aggregate run's browser-worker failure; Java remains absent from default
tasks. The explicit `mise run java-native:codegen --check` also passes all 17
generated data catalogs (`codegen-final.log`).

### Final frozen three-runtime acceptance (2026-11-14)

**PASS: 42/42 gates**, complete and all passed, process exit 0. The source snapshot
is `c06506cef70fb89b0369552d396ce79b46d2868310eb7825e991c590579d1118`, against
baseline `3e5962f9affc06c768df1688765162a65a893848` and format v53. The command was
`mise run java-native:acceptance --evidence-dir .dev/java-native/acceptance-final-first --jobs 3`.
Source and task policy were frozen for the whole run; parallel verification times
are not performance measurements.

| Runtime | Immutable actor | Result |
| --- | --- | --- |
| Stable Java 25 | `stable-20261114-2477637-p9fkmf` | 14/14 gates |
| Ordinary JDK 28 EA | `ea-20261114-2477637-aaxpgk` | 14/14 gates |
| Explicit Valhalla variant | `valhalla-20261114-2477637-oa2hie` | 14/14 gates |

Each runtime passed the same complete inventory:

- The ordinary host suite, including byte fixtures, storage boundaries, resource
  accounting, metadata, encryption and credential promotion checks.
- All 766 SQL files attempted in each storage mode, with zero failures:
  memory **764 applicable / 21,985 passing records**; plaintext **679 / 19,862**;
  encrypted **678 / 19,827**. The remaining files are excluded only by shared
  storage-mode directives. Encrypted cost and EXPLAIN checks use independently
  executed frozen Go results.
- Threaded stress: **16,000 snapshot checks** across Java memory/plain/encrypted
  and frozen Go, with exact checksum `f3e75496a3111181`.
- Four-core process interoperability: **496 cases**, all 31 scenarios across all
  16 Java/Go/Rust/Node ordered pairings, including recovered image comparisons.
- **19/19 saved fault traces**, all exact Java/Go comparisons.
- **66/66 generated fault traces**, all 22 profiles with seeds 1–3: **57/57 exact
  comparisons and 9/9 intrinsic checks**, with canonical intrinsic exclusions.
  Thus each runtime passes 85 complete shared fault traces, 76 exact and nine
  intrinsic; all ten failures from the prior combined inventory are resolved in
  this complete run.
- Both memory and plaintext SQL review inventories, plus four additional exact
  review simulations: `analyze-empty`, `analyze-empty-relaxed`,
  `index-maintenance`, and `repeated-analyze`.

The machine-readable evidence is
`.dev/java-native/acceptance-final-first/report.json`; the top-level run log is
`.dev/java-native/acceptance-final-first.log`. Per-gate logs, source snapshot,
actor identities, process images, complete fault inputs/transcripts/images and
nested inventory manifests are retained under that evidence directory. A separate
read-only audit verified all 42 unique gate keys and log hashes, all per-runtime
inventory totals, all 76 exact shared fault transcript pairs per runtime, and all
four additional review transcript pairs per runtime. Verification processes
exited before the controlled benchmark window began. Promotion and production
delivery remain separate decisions.

### Aggregate resident-state acceptance refresh (2026-11-14)

**PASS: 42/42 gates**, complete and all passed, process exit 0. This revalidates
bounded resident aggregate state after removing repeated state serialization from
resident folds. The frozen source is
`419675aa4fa4b2afeaa9a645d6ebc3e68e38579e77858bb1816e12dd2848c5fc`, against
baseline `3e5962f9affc06c768df1688765162a65a893848` and format v53. Independent
source review, the full host suite, all 52 encrypted aggregate files (894 records)
and the encrypted resource inventory (35 attempted, 27 applicable, 768 passing
records) passed before capture. Source and task policy remained frozen during
`mise run java-native:acceptance --evidence-dir .dev/java-native/acceptance-final-second --jobs 3`.

| Runtime | Immutable actor | Result |
| --- | --- | --- |
| Stable Java 25 | `stable-20261114-2737182-z373h5` | 14/14 gates |
| Ordinary JDK 28 EA | `ea-20261114-2737182-usjd9c` | 14/14 gates |
| Explicit Valhalla variant | `valhalla-20261114-2737182-7krf7l` | 14/14 gates |

Each runtime passed its full host suite; all 766 SQL files attempted in each
storage mode with zero failures; 16,000 threaded snapshot checks with exact
checksum `f3e75496a3111181`; and all 496 four-core process cases (31 scenarios,
16 Java/Go/Rust/Node ordered pairings). SQL totals remain memory 764 applicable /
21,985 records, plaintext 679 / 19,862, and encrypted 678 / 19,827. Exclusions are
only the shared storage-mode directives; encrypted cost and EXPLAIN results are
checked against independently executed frozen Go results.

Each runtime also passed all 19 saved fault traces and all 66 generated traces
(22 profiles, seeds 1–3): 76 exact shared Java/Go comparisons and nine intrinsic
checks. Both SQL review inventories and all four additional review simulations
passed. The independent receipt audit verified all 42 gate log hashes, all 1,249
actor artifact hashes per runtime, all 255 shared traces including the 228 exact
transcript pairs, all 12 additional review transcript pairs, and all process-case
counts. Evidence is `.dev/java-native/acceptance-final-second/report.json` and
its retained nested manifests/logs; the run log is
`.dev/java-native/acceptance-final-second.log`.

The acceptance process exited successfully before benchmark handoff. A subsequent
host process check identified a separate worktree's active Go test, so timing was
temporarily held. The final comparison proceeded as an explicitly qualified
shared-host diagnostic, with process attribution and load observations retained
in [BENCHMARKS.md](BENCHMARKS.md). Parallel acceptance timings are not performance
measurements. This refresh changes neither experimental status nor the separate
promotion and production-delivery decisions.
