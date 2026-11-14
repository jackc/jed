# Java experiment progress

## Current checkpoint

The independent Java core has passed full frozen correctness acceptance on
Linux x86-64. All 42 gates pass on source
`c06506cef70fb89b0369552d396ce79b46d2868310eb7825e991c590579d1118`:
Java 25, ordinary JDK 28 EA24 and the explicit value-class variant each pass all
three SQL inventories, the host suite, 16,000 threaded snapshot checks, 496
mixed-core process cases, 85 shared fault traces and the review regressions.
Independent review verified gate completeness, actor/log hashes and exact
transcripts. The default repository CI passes all 18 steps. Detailed counts
and replayable receipts are in [VERIFICATION.md](VERIFICATION.md).

Profiling removed repeated leaf decoding, per-probe planning and throwaway leaf
layout buffers. Allocation diagnostics and their limitations are recorded in
[BENCHMARKS.md](BENCHMARKS.md). Controlled five-actor comparisons are the remaining
work before closing the experiment. Earlier interrupted checks have been stopped;
they are excluded from accepted comparative timings.

Toolchains and the approved dependency are installed; no manual dependency
installation is needed. The experiment stays opt-in. Production delivery,
publication, other platforms and supported-core promotion remain separate.
The entries below preserve implementation history; their outstanding-work
statements describe those earlier checkpoints.

## 2026-11-14 — authorization and foundations

The maintainer approved the complete [experiment charter](../../spec/design/java-native-experiment.md),
including local installation of the pinned JDKs and scoped crypto dependency.
Baseline: `3e5962f9affc06c768df1688765162a65a893848`, format v53.

Initial work: reproducible opt-in tooling and inventory; SQL value/type/error and
exact-numeric foundations; byte devices and Linux host operations; parser and
execution foundations. Parallel implementation and independent review are approved.

Status: implementation in progress; this is not a completed core. The pinned
Temurin 25.0.5+8, OpenJDK 28 EA24 and Bouncy Castle 1.86 artifacts are installed
locally. A preview value-class compiler probe succeeded. No Valhalla engine
comparison or performance claim has been made.

Implemented slices now include recursive array/composite/range values, JSON,
exact numerics and timezone behavior; joins, lateral and recursive queries,
subqueries, grouped aggregates and windows; primary-key interval scans; stored
DEFAULT and CHECK expressions; basic ALTER TABLE and CTAS; bounded row spools,
sorting and secure scratch. Persistent storage includes validated COW, overflow
chains, encryption and credential changes. Page-service owner/client integration
is being verified. These descriptions record implementation progress, not full
coverage of each subsystem.

Targeted encrypted runs pass all CHECK cases and scalar/composite default cases.
Memory runs cover selected complete relational/window files, with exact costs.
Shared host checks now include 45 catalog fixtures and 28 recovery fixtures.

The sixth complete memory run attempted all 766 files: **489 passed, 275 failed,
and two were inapplicable under shared directives**, with 13,242 records in passing
files. This improves the original 31-file result but remains far from acceptance.
An intervening run was interrupted by repeated operand resolution; that bug is
fixed and all 17 deep-operand cases pass. See [VERIFICATION.md](VERIFICATION.md)
for exact source identities. Later targeted changes are not included in the total.

Later targeted work adds persisted views and sequence metadata, serial and identity
columns, transactional sequence/session state, and identity ALTER operations.
All 83 serial/identity/typed-sequence records pass in memory and encrypted files;
all 160 identity ALTER records pass in memory. Complete regex and many SQL/JSON,
JSONpath, window and aggregate files also pass in targeted runs. Mixed Go/Java
page-service lifecycle checks have passed; the full process acceptance is pending.
A fourth broad run was interrupted at excessive replanning of a deep view chain;
it does not replace the latest complete inventory above.

Major outstanding work includes remaining SQL/catalog objects (including index paths,
advanced writable views and foreign keys), exact resource/cost/planner coverage, complete JSONpath,
page-service lifecycle, cross-core/process/fault acceptance, and matched
performance measurements. Unsupported functionality must fail explicitly rather
than silently pass or disappear from inventories.

Ordered-index persistence now includes independent COW roots, exact live-page
accounting, bounded reverse cursors, and canonical index catalogs/leaf payloads.
The complete host suite passes with 50 catalog fixtures. Twenty-two focused
mixed Go/Java process cases pass, including encrypted index mutation and lifecycle
recovery/handoff. Exact artifact identities and the limits of these checks are in
[VERIFICATION.md](VERIFICATION.md). GIN/GiST and full process acceptance remain
outstanding; GIN posting-run storage is the next slice.

Ordered UNIQUE constraints now own canonical indexes, and ordinary index creation,
expression indexes, indexed type changes and basic/default UPSERT pass focused
encrypted runs. UPDATE stages primary-key replacements before final uniqueness
validation. Partial-index inference has a bounded implication prover. Foreign-key
metadata and mutation closure are in progress; the MATCH FULL suite passes in memory.
The latest broad inventory predates these UPSERT and foreign-key changes.

GIN now has canonical posting runs, array/JSON term extraction, sorted builds,
mutation maintenance, encrypted persistence, and candidate scans including
skip-scan costs, OR/IN and JSONpath term trees. Shared checks cover 59 catalogs and
13 posting-run histories; targeted query files pass in memory and encrypted
storage. Remaining GIN planner/resource coverage and GiST still require work.

Subsequent slices add GiST persistence and scans, exclusion constraints and
exclusion UPSERT, plus writable views with defaults, chains, old/new RETURNING,
check options and schema renames. Focused encrypted runs pass the basic view-write
100 records, view UPSERT 63, check-option 124, ALTER VIEW 52, view-default 76,
exclusion 23, B-tree exclusion 40, partial exclusion 43, and exclusion UPSERT
45 records. Foreign-key keyed/action suites pass 162 encrypted records. The
ALTER TYPE/primary-key suite passes all 57 encrypted records, including exact
default conversion and validating-scan costs. Remaining ALTER constraints and
schema interactions are still being checked. A sixth full memory inventory is
running against a newly captured immutable source snapshot.

- GiST storage checkpoint: canonical range/scalar/tuple bounding trees, exact
  page5/6 fixture reserialization, plaintext/encrypted commit/reopen and old-pin
  lifecycle are connected. Two complete standalone query suites pass encrypted
  (38 records); 22 mixed-process cases pass with GiST added alongside ordered
  and GIN indexes. Collation reference entries and column flags round-trip
  against three shared images. Exclusion parser/catalog and final-state probe
  helpers are implemented; SQL integration and remaining planner/resource
  acceptance continue. Exact immutable actor evidence is in VERIFICATION.md.

Storage ceilings now cover main memory/file databases through
`Database.maxStorageBytes()`, `setMaxStorageBytes(long)`, `storageBytes()` and
`FileOptions.Builder.maxStorageBytes(long)`. Nonpositive ceilings are unlimited;
file admission uses exact live pages and memory admission uses the packed-store
high-water, with forced reclamation and the specified repair exemption. Failed
explicit commits now end the transaction. The 35-record file corpus and focused
host checks pass; attachment/temp domain integration is the next storage slice.

The Java mutation engine now executes MERGE with a frozen matching snapshot,
ordered actions, second-affect detection, simultaneous key changes, unique and
exclusion checks, deferred foreign keys, and source/target/OLD/NEW RETURNING.
Writable-view translation includes MERGE predicates, defaults and check options.
The first disk verification passed 195 records across the basic/error/constraint/
joined-source/hash-value suites; encrypted view/alias/parameter verification
passed 42 more. The hash-value suite does not establish the hash execution path:
the first implementation uses the correlated target scan, and matching-form /
EXPLAIN work remains in progress. Privilege preflight and public session controls
now pass the three shared privilege suites (103 encrypted records); wider catalog
and view authorization checks remain in progress.

Writable WITH now retains referential checks until the statement's final pending
state while executing each substatement's referential actions in order. The two
writable-CTE foreign-key/exclusion suites and cross-type/keyed foreign-key suites
passed 195 encrypted records together. Query CTE reference planning is restricted
to read-query references, as required by the canonical pushdown rule.

Unchanged-key UPDATE replacements now preserve the physical leaf structure while
checking uniqueness against a separate logical end state. This fixes the exact
live-page storage limit after index construction. These are focused results;
full frozen-baseline inventories and acceptance remain outstanding.

Host attachment registries, independent domain pagers and N-root publication are
connected. The full memory storage-limit corpus passes 48 records. Temporary
storage can migrate to an unlinked scratch file before publication, with
independent authenticated encryption for encrypted handles and bounded page
caches. Host checks cover retained readers and atomic admission failure; focused
mixed Go/Java attachment writer ordering passes in both path orderings. The full
shared process/fault/concurrency inventories remain outstanding, alongside
compaction, backup, data-key rotation, stream transport and relaxed durability.

Storage follow-up now implements canonical streaming compaction, both backup
forms through an internal test seam, and data-key rotation/conversion through
`Database.rekey`. Rekey rebuilds ordered trees in target-layout scratch storage;
`FileOptions.protectedScratch(true)` admits encryption from a plaintext handle,
while plaintext scratch is rejected. Encrypted handles retain automatic protected
scratch. The frozen encrypted-service scenario passes all four Java/Go pairings,
including client key invalidation after rotation. Full rekey fixture acceptance
is still being verified.

Canonical byte checks now cover 16 complete fresh plaintext images, ten large
allocation-history commits and fourteen small-page COW commits against frozen Go.
The latter exposed two corrected differences: reopened files must seed reclamation
accounting, and retained parent remaps survive reuse of the original page number.
Client publication now prepares its resident interior skeleton before commit ACK,
so it needs no extra owner read pin; all four client-churn pairings pass. The latest
focused page-service run also passes the 24 existing Java/Go lifecycle and index
mutation cases. Complete process acceptance remains outstanding; the third full
matrix is running after these corrections. Stream transport and relaxed durability
are still unimplemented, and fault/concurrency inventories are not yet complete.

The fifth complete frozen process matrix now passes **all 124 Java/Go pairings**
(31 scenarios, four pairings each). Stream clients use a reconnecting host duplex
factory, and the serving helper strips credentials before forwarding to an owner.
Relaxed commits now retain their durable floor, cumulative body identities and
floor-free allocation grant; local and foreign writers share the batch, with cap,
deadline, close, credential-change and ownership-release flush handling. The
ordinary host suite passes cumulative-manifest crash fallback and failed-flush
reader revocation checks. These process passes do not substitute for the saved
simulation inventory, which remains outstanding.

Rekey acceptance now includes byte-for-byte equality with the frozen encrypted
rekey index fixture, bounded protected scratch, conversion in both directions,
and unchanged original bytes after admission failures. The seven shared
concurrency schedules pass all 72 SQL records both sequentially and with a real
thread per session in memory mode, including blocking writer acquisition. Host
read handles pin a snapshot and survive statement errors. Malformed foreign-key
type pairs and a persisted row-count mismatch now pass their shared rejection
cases. Wider storage modes and full fault simulation remain in progress.
