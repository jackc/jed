# Swift experiment progress

Status: **active implementation; incomplete**. Baseline:
`362e0798ac3a8bd04bd2da556044b6ec73bf468a` (2026-11-05).
Branch: `codex/swift-native-experiment`, local-only, not pushed.

## Accepted constraints

Read [experiment design](../../spec/design/swift-native-experiment.md),
[scope matrix](../../spec/design/swift-native-scope.md),
[host/tool matrix](../../spec/design/swift-native-host-matrix.md), and
[crypto audit](../../spec/design/swift-native-crypto.md). Full baseline surface is the
target; never silently filter failures or infer completion from a partial SQL pass.
No default tasks, website integration, master merge, or package publication.
The separate user work under `tasks/memory` is not part of this experiment.

## Environment verified

- `git log -1 --format='%H %s'`: baseline above, correlated aggregate FILTER/window
  touched-set fix.
- `swift --version`: Swift 6.4.2 (`swift-6.4.2-RELEASE`),
  `x86_64-unknown-linux-gnu`.
- Swift path: `/home/jack/.local/share/swiftly/bin/swift`.
- `git switch -c codex/swift-native-experiment`: succeeded after requesting sandbox
  access for the read-only Git metadata; no unrelated files changed.

## Work ledger

- Initial architecture/lifecycle and full scope/host acceptance matrices persisted.
  All 475 capability rows remain unverified: foundation tests are not SQL conformance.
- Documentation erratum: the format's meta table and encrypted-format introduction
  still said v49 although format.md's version scope, UNIQUE-ownership change, all
  supported cores and golden bytes are v50. Corrected those two stale labels only;
  no byte format, fixture or expected result changed.
- Implemented foundation: byte-exact UTF-8 text/names; structured errors; open type
  and lossless value representations; base-10^9 exact arithmetic (including uncapped
  aggregate addition, PG division scales, decimal key/body codecs); bounded byte
  reader/writer; CRC32/CRC64; pinned LZ4; cost/scalar/query-memory accounts and logical
  sizes; atomic cancellation; memory and Unix positioned-I/O byte hosts.
- Approved crypto integration: swift-crypto 5.1.1 at
  `f8fad2f95ad67b45f6340c0960e2396094d67e07`; required cryptographic serialization
  dependency swift-asn1 1.7.4 at `917b472dbe53ba699cbb0495f9a95a084bc6364d`;
  reference Argon2 20190702 at `62358ba2123abd17fccf2a108a301d4b52c01a7c`, portable
  `ref.c` with `ARGON2_NO_THREADS` preserving lanes. Package.resolved and vendored
  source provenance/checksums are tracked. SHA224 uses the pinned bundled backend
  through a narrow synchronous buffer borrow; other primitives use public Swift APIs.
- Opt-in build/test/resolve/vendor/codegen mise tasks added, with package-local
  caches and frozen baseline metadata. Default tasks do not depend on Swift.
- Independent foundation review found no current byte/accounting defect; flagged
  future session lifetime-limit freezing and per-record concurrent peak isolation.
  Independent crypto/tooling review verified pinning and native-code confinement.
  Full durability, accounting and adapter review remains required after integration.
- Added byte/position-preserving lexer, lazy script splitting, exact numeric
  functions and guarded transcendental kernels. Separate numeric and crypto reviews
  found no actionable defect in those scoped modules; SQL corpus execution is still
  required. Added key-block selection/rewrap and plaintext/encrypted page codecs.
- Added v50 meta encoding, manifest construction, bounded page-at-a-time recovery,
  bootstrap handling, overflow identity checks and explicit host-error propagation.
  Encrypted meta authentication follows keyless selection at the pager seam.
- Added native SQL AST/parser, recursive value/key codecs, temporal parsing and
  functions, and explicitly loaded pinned JTZ/TZif timezone data. Fixture adapters
  use every case in the eight key-encoding datasets and shared temporal/TZ vectors.
- Added pager stabilization/publication, cumulative relaxed manifests, failure
  poisoning/revocation, deterministic allocation, persisted free-list handling,
  packed PAX leaf regions, and overflow chains. These are tested storage primitives;
  a database/tree/catalog transaction path is not yet connected.
- Independent storage review caught an equal-txid fallback defect: an incomplete
  sibling's valid header cannot make it the durable floor. Swift now validates the
  sibling manifest before parity selection. The same existing issue was found in
  supported cores; the separate correction/regression slice is committed locally
  as `196feb8a`. Focused Go/Rust tests, all 1,276 TypeScript unit tests and default
  spec verification pass. The pinned Swift scope baseline remains unchanged.
- Independent codec work found reversed f32/f64 stable type codes in the initial
  Swift enum; corrected to canonical f64=12/f32=13, with a byte-boundary regression.
- No complete SQL resolver/executor, database, complete tree/catalog commit path or
  conformance adapter yet. Parser acceptance is not SQL conformance.
  No database-level encryption, recovery, process, simulation or interchange result.
- No performance baseline or Apple-platform verification yet.
- Frozen parser audit covers all 21,525 SQL records in 749 files: 21,171 parse,
  354 reject with the expected SQLSTATE, zero unexpected rejection. The 262
  accepted records expecting eventual `42601` still require later semantic checks.
- Added persistent packed B+tree mutation, bounded cursors, interior skeletons,
  CLOCK leaf pool with single-flight loading and invalidation, incremental and
  whole-image serialization, and deterministic large-value disposition/kept chains.
  Three frozen B+tree goldens rebuild byte-exactly. A near-capacity delete regression
  exposed an inherited empty unary subtree; Swift fixes it. The same issue has a
  confirmed Go reproduction; the separate supported-core correction is saved locally
  as `7b4883b6`. Focused tests in all three cores, two shared simulation traces,
  TypeScript type checks, selected formatting and default spec verification pass.
- Added datetime and exact numeric template processors, exact IEEE-to-decimal
  conversion, JSON syntax/spans/canonical values, accessors/mutations/builders and
  streaming JSON SRF helpers. SQL resolution/evaluation is not connected yet.
- Added v50 catalog metadata, two-pass nominal type resolution, reference/statistics
  validation and continued-entry page chains. Catalogs from all 87 ordinary golden
  images (including six encrypted images) decode and rebuild byte-exactly. All 112
  file fixtures are now snapshotted from the frozen baseline; malformed/recovery
  database-level acceptance remains separate and incomplete.
- Added handwritten regex compiler/VM, SIMILAR translation and scalar/SRF helpers.
  All shared regex program/match/SIMILAR vectors pass, including exact instruction
  listings, captures and step counts. Regex SRFs retain compact capture/span state
  after the specified whole-search admission and materialize one row per next call.
- Added native float input, exact shortest-round-trip output and scalar math kernels;
  three internal float tests pass. Added recursive total/SQL comparison and static
  type rules; an independent key-order agreement invariant passes. Independent
  review caught and corrected JSON-family element-wise array cast admission.
- Added JSONPath compiler/evaluator and a retained query cursor. The frozen kernel
  adapter passes 723 records / 898 cells; 133 records are explicitly excluded because
  they need SQL resolution, relations, session state or SQL/JSON syntax. This is not
  SQL conformance. Initial probes had 30 failures, corrected against unchanged shared
  inputs/expectations: adapter SQLSTATE extraction, numeric member names/comments,
  and decimal method negative/over-precision scales. Exists/first/match retain at most
  one value while preserving required evaluation; arithmetic/comparison operands
  collect the sequences required by jsonpath.md. Relation integration must reserve
  producer rows and charge generated_row. The array function intentionally collects
  and charges its growth.
- Added GiST scalar/range tuple bounds, canonical resident R-tree construction/search
  and page codecs. Four plaintext goldens and malformed/encrypted boundary tests
  pass, together with actual encrypted golden rebuilding and empty-root/equal-endpoint
  refinements. Independent review found no additional primitive defect.
- Added GIN term/query extraction, exact posting runs, packed-tree edits and span-based
  gathering. Six tests cover shared run/history/JSON term vectors, nine plaintext or
  encrypted golden images, candidate/cost/cancellation behavior and malformed data.
  GIN candidate buffers have explicit lifetime but no new logical-memory charge:
  the frozen Q1/Q2 schedule does not charge this storage-read staging. SQL row admission
  remains required; this is not a new whole-engine memory guarantee.
- GIN integration uncovered a supported-core false negative for a strict root-string
  JSONPath equality. PostgreSQL 18's forced GIN scan has the same bug, while its direct
  predicate/sequential scan returns the row. Separate local correction `2e13d1b8`
  widens the candidate term to KEY OR VALUE in all cores; shared regression, oracle
  check, 36 scoped memory/disk suite executions and default verification pass.
  The divergence is documented in gin.md §9; the original frozen baseline is retained.
- Added loaded Unicode/JCOLL/JUCD decoding/merge, UCA sort keys and casing; three
  tests include 46 frozen sort-key vectors and loaded-versus-bare environment behavior.
  Initial delta decoding followed a stale README layout; corrected to the canonical
  frozen counts-first bytes. No host Unicode/ICU fallback or compiler claim.
- Scalar casts and the first typed expression resolver/evaluator now compile and
  pass 432 frozen expression records, 527 result cells, 18 costs and five type
  assertions; 966 records are explicitly excluded. This uses the real expression
  path, but does not establish query/database conformance. Initial probes caught
  missing adapter timezone/clock directives, literal JSONPath/NULL-cast costs,
  temporal arithmetic wiring and an interval positional-field bug, all corrected
  against unchanged shared expectations. Clock-relative dates remain runtime nodes.
  Full function/container/subquery resolution and query execution remain incomplete.
- The immutable corpus snapshot now contains all 749 baseline files (742 SQL,
  seven concurrency). Expanded expression probes pass 642 records / 740 cells,
  72 costs and 11 type assertions; 3,630 records in the selected cast/type/expression
  files remain excluded by the explicit AST/query-shape gate. Expansion initially
  exposed 73 failures (preserved in `/tmp/jed-swift-expression-expanded.log`):
  text/bytea/jsonb concatenation routing, interval scaling, and preparation-only
  temporal literal charges. Independent review corrected NULLIF's left-width
  preservation and conditional parameter-registry adoption; host metadata and all
  256 octal input bytes now have boundary checks. A zero interval scales to zero
  before factor-width rejection, after divide-by-zero checking.
- Connected catalog-driven scalar overload selection, named/default/variadic
  arguments, native text/bytea functions, and recursive array/range kernels.
  Container input/output, casts, constructors, subscripts, quantified comparisons,
  range accessors/set operations and compatible array widening now enter the
  real expression resolver/evaluator. The expanded gate passes 2,202 frozen
  records / 2,425 cells / 281 cost checks / 13 type checks; 2,070 records remain
  explicitly outside its AST/query-shape gate. All 163 current tests pass.
  This still does not establish database/query conformance.
- Expansion first produced 118 failures (including one stale internal shape
  assertion), then 19 and two before passing. Fixes include handwritten overloads
  whose signatures are specified outside the catalog, recursive rendering costs,
  array upper-bound representation versus stricter operation limits, transient
  record/range arrays, subscript indexing, typed NULL cast costs, and numeric/
  timezone template edge cases. No frozen expected outputs were changed.
  Independent review checked container resolution and text/bytea kernels.
- The storage lifecycle source set compiles: domain snapshots/stores, exact
  commit layout/preflight, catalog-root patching, live-page deltas and deferred
  free-page accounting. Focused lifecycle tests and database writer/pin
  coordination are in progress; compilation is not durability acceptance.

## Verification ledger

2026-11-05, Linux x86-64, Swift 6.4.2, debug configuration.
XCTest timestamps below are checkout-local CST (UTC−06:00); earlier ledger
entries incorrectly labeled these local timestamps UTC.

| Command | Result |
|---|---|
| `mise run verify` | PASS: all default spec/fixture/catalog/codegen checks; 112 file goldens, 24 integrity images, 738 encoding vectors, corpus taxonomy and other existing checks |
| `mise run oracle:status` | PASS after sandbox socket access: existing PG18 `jed_oracle`, builtin C.UTF-8 and all profile pins agree; no cluster provisioning |
| `mise run swift-native:codegen --check` | PASS with expanded pinned encoding/temporal/TZ inputs and three binary fixtures; fingerprint `8d60f8b77c8a63bd5ea5815788eda7bb0743953e256a71f1d9128f26a454d974` |
| `mise run swift-native:test` | PASS 32 foundation/crypto tests, then PASS 35 after byte-host/logical-size slice; 0 failures |
| `mise run swift-native:test` | PASS 61 after lexer/numeric/encryption/meta slices; 0 failures (2026-11-05 16:31 CST) |
| `mise run swift-native:test` | PASS 92 after parser, temporal/value/key, pager and packed-page slices; 0 failures (2026-11-05 16:45 CST) |
| `mise run swift-native:parser-audit` | PASS 21,525 frozen SQL records; zero unexpected rejections (see Tools/PARSER_AUDIT.md) |
| `mise run swift-native:test` | PASS 113 after tree/disposition, temporal/numeric formatting and JSON foundations; 0 failures (2026-11-05 17:01 CST) |
| `mise run swift-native:test` | PASS 120 after catalog/golden and regex/SIMILAR slices; 0 failures (2026-11-05 17:08 CST) |
| `mise run swift-native:test` | PASS 122 after comparator/key invariants and regex admission; 0 failures (17:14 CST) |
| `mise run swift-native:test` | FAIL 127 tests / 30 JSONPath kernel probe failures; adapter and kernel issues preserved in `/tmp/jed-swift-jsonpath-first.log` (17:16 CST) |
| `mise run swift-native:test` | PASS 133 after JSONPath fixes, float kernels and GiST slice; 0 failures (17:20 CST); same 133 PASS after scalar-cast source compilation (17:22 CST) |
| `mise run swift-native:test` | PASS 135 after retained regex producers and encrypted GiST golden (17:24 CST) |
| `mise run swift-native:test` | FAIL 138 / two new Unicode decoder failures (17:30 CST); subsequent expression probe failures retained in `/tmp/jed-swift-expression-first.log` |
| `mise run swift-native:test` | PASS 146 after Unicode/interval corrections, GIN vectors/goldens and scalar-expression integration (17:35 CST) |
| `mise run swift-native:test` | FAIL 147 / 73 after full-corpus expression expansion; then PASS 147 after corrections (17:41 CST), including 642 shared expression records |
| `mise run swift-native:test` | FAIL 156 / 118 after function/container expansion, then 160 / 19 and 163 / two; PASS 163 (17:56 CST), including 2,202 frozen expression records |
| `mise run swift-native:codegen --check` | PASS frozen cast/type/expression corpus, Unicode and GIN fixtures; fingerprint `76da6b8479747b97050e7c7709a6ec4bd57bd2f322bd5d85aa781e1aa7547c28` |
| `mise run swift-native:codegen --check` | PASS all 749 frozen corpus files; fingerprint `37257dce1710682ff656ee83474597acd4adc91e8f040d8155e9d16e8b63ff50` |
| `mise run swift-native:codegen --check` | PASS all 112 frozen file fixtures plus regex vectors; fingerprint `802cfd6bda26761f7dc3c91d811278ced3bd0a4e368c3e11f3d6b21e917ba77f` |
| `mise run unit:go -- -run 'Test(Validated\|CommitCRC64\|CompleteManifest\|IncompleteEqualTxid\|Serialization)' -count=1` | PASS 8 selected recovery tests |
| `mise run unit:rust -- --lib validated_cow_tests` | PASS 7 selected recovery tests |
| `mise run unit:ts` | PASS 1,276 tests after sandbox Unix-socket access; initial sandbox-only EPERM failures resolved |
| `mise run durability-review …` (temporary wrapper, removed) | PASS 27 targeted TS recovery tests, TS typecheck, selected Rust/Go/TS formatting checks |
| `mise run verify` after shared recovery correction | PASS all default spec checks |
| Focused Go/Rust/TS PMap tests; `mise run tree-regression` | PASS shared empty-unary-subtree regression, plus delete-merge-separator-overflow simulation; exact transcripts and images agree |
| `mise run typecheck:ts`, selected formatting and `mise run verify` after shared deletion correction | PASS |
| `JED_CONFORMANCE_FILTER=gin_jsonb_ mise run conformance:{go,rust,ts}` (three commands) | PASS six memory + six disk suites per core; initial sandbox Unix-socket denials resolved by scoped reruns |
| `mise run corpus:check spec/conformance/suites/query/gin_jsonb_root_string.test` | PASS, byte-identical oracle output; separate forced-index PG18 probe confirmed the documented bug |
| Selected formatting, independent review, `mise run verify` after GIN correction | PASS |
| `git diff --check` | PASS before first integrated foundation test |

The 35 tests include all 11 shared LZ4 and 41 decimal key vectors, arithmetic
invariants, UTF-8/byte corruption and host boundaries, resource precedence/floors,
memory/Unix byte-device behavior, digest/AES known answers, the three shared XAES
cases, two subkeys, one KEK, one shared Argon2 vector, wrapped-key ciphertexts,
and an upstream multi-lane Argon2 vector. They do not establish SQL/file parity.

The 61-test run additionally proves lexer/script source boundaries, numeric guarded
kernel/account invariants, exact key-block/rewrap bytes, encrypted per-page golden
authentication/resealing, manifest chain/torn-content fallback and host-I/O failure
propagation. An initial integration run caught a stale v49 check in the new Swift
encrypted envelope; corrected to v50 without changing fixtures. New meta tests
initially expected a high-water bound without a file length and compared an unsealed
meta CRC with an authenticated one; corrected those test assumptions to the spec.
No shared expected results were changed.

The 92-test run additionally exercises all frozen key vectors/ordered-slot groups,
shared temporal/TZ vectors, parser source/nesting boundaries, byte-host pager write
order, torn writes and stabilization, relaxed cumulative manifests and failed-cap
read revocation, packed regions and malformed offsets, deterministic free/manifest
allocation, and overflow slab/chain validation. Initial integration found a fixed
zone abbreviation missing `:00` and an adapter assuming bytea vectors used `value`
instead of `hex`; both corrected against unchanged shared fixtures. Intermediate
build failures while parser files were still being written were not test failures;
subsequent complete sources compiled and all 92 tests passed.

The 113-test run additionally covers packed-tree snapshot/cursor boundaries, per-delete
near-capacity structure, golden rebuilds, cache faults/concurrent invalidation, kept/fresh
overflow bytes and costs, numeric template arithmetic and JSON internal boundaries.
Initial disposition test inputs did not fit after one compression as assumed; changed
the input from 80 to 70 bytes to exercise the intended boundary. An audit corrected
Swift's encrypted compression quantum to C (chain payload capacity is separately C−16).
No shared expected results changed. The 120-test run verifies catalog page bytes,
continued-entry corruption, nominal cycles and malformed FK/type references. A test
initially expected the malformed-row-count image to fail at catalog open; that
fixture explicitly defers count drift until a write/integrity check. The test now
checks that boundary and separately rejects a zero root with nonzero cardinality.
This is catalog verification, not complete database/file conformance.

Local-only commits so far: `91aef346` (approved scope/design), `f9c09043`
(verified foundations), `d2a56c3b` (syntax/numeric), `e83ff6d1`
(recursive codecs/temporal), `a0cb024b` (pager/packed primitives), and `196feb8a`
(shared recovery correction). The foundation commit's staged whitespace check reported two existing
whitespace defects in pinned, unmodified Argon2 upstream LICENSE/encoding.c. Those
vendor bytes remain unchanged to preserve the recorded provenance hashes.

Additional local-only commits: `b9e99410` (frozen syntax audit/tree fixtures),
`a645434f` (packed B+trees/disposition), `a87abe2c` (formatting/JSON foundations),
`16569ff1` (catalog codecs and frozen images), `7b4883b6` (shared deletion correction).
Further local-only checkpoints: `111932fa` (float/regex/type foundations),
`2e13d1b8` (shared GIN root-string correction).
Further experimental checkpoints: `4c14d61c` (GiST), `ba7859bd` (JSONPath and full
frozen corpus), `35579e82` (GIN and loaded Unicode runtime), all local-only.
`e2503626` records the initial typed expression/scalar-cast slice, also local-only.

Initial SwiftPM compile failed while SQLDecimal was not yet present, then four
missing `try` markers in decimal test autoclosures were corrected. No expected
results changed. Swift 6.4's default `swiftbuild` stalled when the sandbox denied
CFSocket creation; interrupted experiment processes were identified and stopped.
Opt-in tasks now select its supported `--build-system native` option, which builds
successfully and emits an upstream deprecation warning. No sandbox IPC dependency
is introduced into the engine. SwiftPM also warns about the direct swift-asn1
constraint, retained intentionally to pin CryptoExtras' required transitive package.

## Current integration checkpoint

Checkpoint `b7324c9b` saves the 250-test integration below, local-only and unpushed.
The Linux page-service slice is checkpointed separately at `40c9c6a4`, also local-only.
Work after it remains active. The latest complete integrated run (20:50 CST) ran
**287 tests in 118.181 seconds**: all **286 tests outside the expanded whole-file
SQL test passed**. That SQL test ran **187/749 files, 5,038 records (3,100 statements /
1,938 queries), 7,800 cells, 482 cost / 874 name / 11 type assertions; 91 failures**.
All records in every admitted file execute; no failing file or expectation was
removed. Remaining failures include writable CTEs, ON CONFLICT, ALTER column
rewrites, introspection/temp domains, EXPLAIN and two multi-table join costs.
The 20:43 SQL-only run had 92 failures; the subsequent DISTINCT ON aggregate-key
identity correction removed one. Earlier 178-file and 166-file runs had 124 and
117 failures respectively. No frozen expected outputs changed.

The selected ordinary and ordered-set/hypothetical window files, basic/nonrecursive
CTEs, recursive structure/SEARCH/CYCLE, all 11 subquery files, composite casts,
JSON record/populate, both JSON_TABLE files, JSON Unicode errors, DISTINCT ON and
main CTAS file pass. CTAS/SELECT INTO remains incomplete for temporary domains and
writable CTEs. Real columnar projection/group lanes, bounded top-k and eager-filter
lifetimes now satisfy the selected query-memory cases apart from EXPLAIN gaps.
Subqueries use isolated lexical scopes, statement-shared initplan results,
depth-aware outer references and explicit Q1 result lifetimes. Independent review
corrected scalar-subquery collation provenance and replaced recursive ordinary
expression initplan walking with an explicit stack.

The row-valued corpus pins IN-subquery as a flat node visiting every result
element; grammar.md §26's older OR-chain description is stale (literal IN-lists
still use an OR chain). Recursive UNION hash admission follows the frozen core
batch schedule (sum payloads before the cost guard); prose suggesting a per-key
guard needs a separate shared-spec audit.

Linux automatic owner/client activation is now connected. Native tests cover 27
frozen frames, two owner records, 14 malformed vectors, streamed deltas, Unix
framing/close wakeup, encrypted foreign commits, request multiplexing, retained
invalidations, role-epoch revocation, real-process election/promotion, read-only
handoff, lost-ack resolution and writer timeout/cancellation during election.
Independent review identified unsynchronized core publication and admission waits
that could outlive writer deadlines; both were corrected before this run. A first
full run was stopped after diagnosing the process-test helper's fixed-fill pipe
read; it now uses one polled native read. The focused automatic/role/file-host gate
then passed all 16 tests before the completed full run. Post-ack storage maintenance
and the shared process/simulation adapters remain outstanding.

The current snapshot fingerprint is
`7a1bcf53fefb01e43484e760a68d9f61faabbc0c15c35d2b1da39c4c90db8043`.
Latest command: `stdbuf -oL -eL mise run swift-native:test`, log
`/tmp/jed-swift-native-full-187-fixed.log`. The adapter reports every mismatch;
XCTest records the first 40 plus one summary failure while printing the remaining
51. The earlier 43 selected non-SQL tests (including CTAS privileges/parameters,
foreign keys and host boundaries) also passed. Scope rows remain unverified until
the full applicable acceptance evidence is available.
A final independent accounting review found JSON_TABLE admitted Q1 rows while
later column expressions were still evaluating. Production now completes first,
then generated-row charges run, then the relation admits Q1, as memory.md §5.1
requires. The follow-up `mise run swift-native:test --filter
'SQLCorpusBoundary|CreateTableAsBoundary'` completed at 20:55 CST: both CTAS boundary
tests and two corpus-adapter boundary tests passed; the 187-file SQL run retained
91 failures with unchanged counts. Log: `/tmp/jed-swift-native-sql-187-json-admission.log`.
Review also identified the same delayed-admission requirement for other bounded
SRFs and a missing general statement privilege-preflight phase (a denied-source
CTAS must reject before an invalid target name); these are outstanding corrections.


Verified integration (2026-11-05 19:24 CST): **250 tests passed, zero failures**
with `mise run swift-native:test` (82.999 seconds). The public
Database/Session/Rows path now executes DDL, INSERT/UPDATE/DELETE, ordered and bounded
base scans, parameter admission, transactional sequence functions, serial/identity
defaults, initial grouped aggregates, set operations, VALUES queries, derived
relations and authored nested-loop joins, SELECT DISTINCT, and initial stored-view
reads/inserts. This is
still a partial SQL engine: costed join search/hash/INL/pushdown, complete expression
and relation sources, complete view DML/dependencies, windows, constraints, prepared plans, attachments,
temporary domains, spill integration and process page service remain incomplete.

An earlier `mise run swift-native:test` at 19:04 CST ran 242 tests: two assertions failed in
one new catalog environment test because collecting an in-memory statistics summary
duplicated its continuation arrays. The collector fix is now published and its
targeted and full tests now pass. That run's original 28 SQL files passed (618 records,
721 cells, 60 cost and 45 name assertions); the expression adapter passed its
expanded scalar/temporal selection. File-host tests cover exclusive protocol-v2
ownership, atomic no-clobber creation, encrypted opening, descriptor/lock cleanup,
fork and special-file refusal, collation grading, and retained directory durability.
Automatic owner/client page service remains explicitly unavailable.

The latest full run passes **56/749 complete SQL files**, 1,334 records (814
statements and 520 queries), 1,458 cells, 152 cost, 192 name, and two type assertions.
All records in every admitted file execute; 693 files remain outside this gate.
The final failure was all-NULL set-operation metadata: `ResultColumn.type` is now
optional so unresolved `unknown` remains observable through the host API.
Independent review also corrected definer-view INSERT privileges leaking into an
unrelated SELECT source, view DEFAULT VALUES mapping, and identity defaults.
All five new maintenance tests pass, including encrypted rekey round trips and
pre-replacement failure isolation. Independent crypto review found no blocking
maintenance defect; native replacement/reopen failure injection remains outstanding.

An earlier `mise run swift-native:test --filter SQLCorpusBoundaryTests` at 19:13 CST ran
46 complete admitted files / 1,099 records / 1,084 cells, with 92 cost, 100 name,
and one type assertion. Seven failures remained: five consequences of unconnected
view operations and two consequences of unconnected SELECT DISTINCT. No failing
record was removed. At that point 703 frozen corpus files remained outside the gate.
The 45-file run before compound/derived integration had 15 failures; a duplicated
aggregate collector entry (resolver revisiting one source call during literal
adoption) was fixed, along with the missing query paths. Subsequent commands and
results are recorded above; these intermediate failures are retained as history.

The Linux host uses a first-party CJedHost shim containing only the OS atomic
no-replace rename call; it adds no dependency or native engine algorithm. Swift
retains all storage, SQL, type and codec policy. Scratch/rebuild and host maintenance
work are opt-in and remain separately tracked in Tools/STORAGE_LIFECYCLE_PLAN.md.

Previous storage checkpoint (2026-11-05 18:20 CST): **197 tests passed**. It includes
3,235 frozen expression records / 3,793 cells / 294 cost checks / 13 type checks,
with 2,547 records explicitly outside the expression adapter's gate. JSON/SQLJSON
initially exposed 14 failures, fixed against unchanged expectations. A speculative
JSON resolver path multiplied work on nested arithmetic; conservative type-candidate
admission restored the suite's roughly 65-second runtime. This remains an expression
slice, not full database conformance.

The same run verifies seven storage-domain tests, six coordinator tests, streamed
export of 85 canonical images plus normalized metadata for two torn-meta recovery
images (all body pages exact), independent cursor pins, reader-watermark reclamation,
Q4 preflight, and memory stage/adopt failure isolation. Export retains bounded packed
leaf/page payloads and chain references rather than collecting the whole image.
Two initial Q4 failures were test defects (a throwing XCTUnwrap recorded an expected
error, and a proposed growth did not exceed the existing high-water mark). Two export
assertions initially expected repaired meta CRCs to retain deliberate fixture damage.
The corrected boundaries pass without changing storage policy or shared fixtures.

The separate handwritten Unicode compiler rebuilds shared compiler vectors and
production collation/casing/bundle artifacts byte-exactly. Its command-line smoke
check, `mise run swift-native:unicode -- bundle --preset non-cjk --spec-dir spec
--out /tmp/jed-swift-native-unicode-smoke.jucd`, produced 340,621 bytes identical to
the shared fixture. Current snapshot fingerprint is
`7024bdb84a3dc257a81d965161303df5ae427f0870a4c5750dcc99432d6d7f83`.
Environment sources, host-kernel boundaries and lazy per-block clock/UUID services
are included in the 197-test run. New Database/Session cursor tests and expanded
clock/seed/variables expression probes added afterwards pass in the 250-test run.
`mise run swift-native:codegen --check` also passes at the current fingerprint.

## Next steps

1. Active delegated slices: Linux protocol-v2 page service (`host_inventory`),
   columnar/memory/join planning and corpus admission (`crypto_audit`), views and
   FK actions/ALTER/constraint DDL (`scope_inventory`). Root owns query/session integration
   and verification. Keep source publication coordinated with integrated checks.
   The experiment is not frozen.
2. Extend the whole-file corpus gate and connected execution paths. Audit cost
   charge granularity, view dependencies, costed join planning, resource accounting,
   and rejected/unconnected SQL surfaces. Do not substitute decoded dictionary
   storage or engine-owned cursor collection.
3. Continue vertical slices across expressions, SQL, pager/storage and host APIs,
   verifying against shared data/corpus at every slice and retaining failing counts.
4. Complete full verification and independent reviews, then measured optimization
   and final verification/benchmark reports. Freeze only once acceptance is met.
