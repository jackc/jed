# Frozen simulation comparison evidence

Durable preoptimization evidence is preserved in the ignored ext4
[verification archive](../results/verification-preoptimization-20261106/manifest.json):
50 successful report/log/TSV files, 11,386,296 bytes, with original paths and
SHA-256 hashes in its manifest. Originals remain intact; binaries and simulation
images are excluded. This archive remains preoptimization evidence. Final
actor-specific verification and the measured comparison are recorded separately
in [VERIFICATION.md](../VERIFICATION.md) and [BENCHMARKS.md](../BENCHMARKS.md).

## Postoptimization saved comparison

For optimized source `cec8170bfc33f087fa98949ae661b1eaa5490218`, all four cores pass
all 18 saved regressions. All 54 transcript comparisons pass; 612 images are
emitted with no byte differences, and all 153 Swift images pass the independent
checker. There are no failures or per-core-fault comparison exemptions in this saved set.

- [Summary](../results/sim-regress-postoptimization-20261106/summary.json):
  SHA-256 `f1c4857be5e421a78dd1d7171c00e56d10611b234fa16d0a0153da35a8d33462`.
- [Manifest](../results/sim-regress-postoptimization-20261106/manifest.json):
  SHA-256 `8cd5321e91bab8b90a2aab90e8517ce6fa1dd3422e78b507f27d9a6587e5d226`.
- Swift actor: `b700af5d38441fb611eb6f463dfe19b94b53ad89cef0e00a8e18c7d05a420e12`.
- Log: `/tmp/jed-swift-native-sim-regress-packed.log`.

[VERIFICATION.md](../VERIFICATION.md) maps the other final affected-path gates;
[BENCHMARKS.md](../BENCHMARKS.md) records the completed measured comparison. The
prior generated/process evidence below retains its earlier actors; these finite
checks are not exhaustive fault coverage or a performance measurement.

## Preserved preoptimization acceptance (2026-11-06)

All 18 saved regressions pass in
all four cores, with 54 transcript comparisons and 153 independently checked
Swift images. The generated 22-profile × eight-seed sweep also passes:
**176 traces per core, 459 transcript comparisons, 7,049 independently checked
Swift images, zero failures**. Its 28,196 emitted images and 23 shared per-core-
fault comparison exemptions are retained in the report, not silently omitted.

An earlier generated summary is
`/tmp/jed-swift-sim-sweep-20261106-980401-9e3v3g/summary.json` and its manifest
records Swift actor `40c80ea64c8eb37d78cea74192d60d59c8ea95b73645b66c399c349462894709`.
The preoptimization saved release regression summary is
`/tmp/jed-swift-sim-sweep-20261106-2057112-ozakk7/summary.json`: 18 per core, 54 comparisons,
153 checked Swift images and zero failures. Earlier saved-regression log:
`/tmp/jed-swift-native-sim-regress-final.log`.
The refreshed process matrix passes 496 ordered pairs. These finite simulation
and process runs do not establish exhaustive fault coverage or performance.
The separate service SQL refresh passes all six directions and 116,628 frozen
records, with no failures:
`/tmp/jed-swift-service-corpus-20261106-1040687-2wheah/report.json`. Complete V04
accounting now passes all 241,412 measurements, zero failures and complete
membership at `/tmp/jed-swift-query-memory-20261106-1996511-bxwolu/report.json`. Earlier
376-difference reports and the failed Rust cache attempt remain preserved in
RESOURCE_ACCEPTANCE; the shared loader correction resolves those differences.
The fourth release refresh passes all 509 native tests and all frozen SQL
files in `/tmp/jed-swift-native-release-all749-eof-fourth.log`. Prior handoff/client
and old-pin assertions are resolved; their failed logs remain preserved.
Both Swift-client/Rust-pages stream scenarios pass in
`/tmp/jed-swift-native-process-rust-stream.log`, and canonical default CI retry
passes 18/18 in `/tmp/jed-swift-native-default-ci-gist-canonical-retry.log`.
The fresh full release mixed-process run passes all 496 pairings, and saved
release simulations pass as above. The final generated release sweep passes
all 176 traces per core, 459 comparisons and 7,049 independently checked Swift
images at `/tmp/jed-swift-sim-sweep-20261106-2104641-awof5y/summary.json`. Its 28,196 emitted
images and 23 explicit canonical fault exemptions remain recorded; failures
are empty. Performance was outstanding at this checkpoint; the completed comparison is recorded separately in BENCHMARKS.md.

## Historical comparison and corrections

The first completed four-core comparison used the baseline
`362e0798ac3a8bd04bd2da556044b6ec73bf468a`, input fingerprint
`369ad9def9a39d16e8812714f9100f1ad86df6822910021c32cfa19e4c2e7bfc`, and native
Swift actor `01abf2e84fbe8d376d2d09c61599999a7c3970f3553b910e73534066644a4e5b`.
Evidence remains in `/tmp/jed-swift-sim-sweep-20261106-3794104-m1ohb2`;
`manifest.json` records each actor/source digest and trace hash, and
`summary.json` retains every failure. The log is
`/tmp/jed-swift-native-sim-cross-core-regress-bytecheck.log`.

All four cores returned 18 passing local-oracle results. Of 54 cross-core
transcript comparisons, 21 disagreed (7 traces against each supported core).
There were 159 image differences after two initial recovery/history
divergences, for **180 total disagreements**. All 153 emitted Swift crash and
backup images passed the separately pinned Ruby byte-format checker. There
were no per-core fault comparison exemptions in this regression set.

| Trace | First difference and disposition |
| --- | --- |
| `analyze-then-drop-table` | Step 3 reports ANALYZE affected count -1 instead of 0. Simulator command-tag adapter omitted ANALYZE. |
| `relaxed-close-truncation-floor` | Step 53 has the same ANALYZE adapter error. |
| `service-attachments-owner-and-clients` | Steps 186/239 have the ANALYZE adapter error; step 251 also charges 74 instead of 50 for an ordered LIMIT read, with identical rows. Attachment routing erased PK order facts as well as disabling bound pushdown, so top-K visited all 34 rows instead of the first 10. |
| `service-backups` | Three reader closes after role loss report an error instead of canonical `lost:true`; two ANALYZE counts also differ. |
| `encryption-service-reconnect-keeps-held-key` | Step 4 unchanged commit fails 08006. Transaction cursors unnecessarily acquire independent remote reader pins, so oracle reads consume an armed disconnect before the commit. |
| `encryption-rekey-rename-not-durable` | Step 106 recovers version 8 rather than 7. Credential rewrap clears the Swift pager's recovery-stabilization state; all three supported cores preserve it for the next data commit. Later transcript/image differences follow that first divergence. |
| `service-owner-and-clients` | Step 73 yields 08006 rather than 08007 and kills an extra process. Swift releases poisoned owner presence during commit resolution, allowing premature client promotion. Supported owners stop serving while retaining ownership until handle close/death. Later differences follow the changed process/history sequence. |

The reviewed host corrections are published. A focused gate passed all 26
selected tests, including the three new recovery/lifetime boundaries and the
whole view-depth file (`/tmp/jed-swift-native-view-host-recovery.log`). The boundaries
assert stabilization before data writes after credential rewrap, failed-owner
presence retained until close, and transaction cursors borrowing their writer
pin without independent reader RPCs. The ANALYZE and lost-reader-close changes
are transcript adapters; attachment PK order admission is a planner correction.

Those focused checks did not turn the failed comparison into acceptance. A new
full comparison was required to verify all corrected histories and exact bytes.
The old evidence remains intact; later passes are recorded below and above.

The opt-in task is `mise run swift-native:sim:sweep`: by default it generates
all 22 frozen profiles for seeds 1 through 8 with each profile's canonical
length, and compares Swift, Go, Rust, and TypeScript. `--regress` selects the
18 frozen regressions instead. It verifies the shared generator, driver,
independent checker, and checker helpers against frozen hashes; snapshots the
native binaries; rejects missing/empty transcripts and missing images; and
records explicit comparison exemptions defined by the shared driver. It does
not participate in default tasks or CI.

## Corrected comparison checkpoint

The next complete four-core run used Swift actor
`2a49ef74c3f5fcba95c20adfeff0028c3bd3daf7638334e2fe86bd577e1352b0` and the same
frozen inputs. All four cores again passed all 18 local-oracle traces. Of 54
transcript comparisons, 51 agreed and three differed at the same step 145 of
`encryption-rekey-rename-not-durable`. All emitted image comparisons agreed;
all 153 Swift images passed the independent checker. Results are retained at
`/tmp/jed-swift-sim-sweep-20261106-3815108-91dgv0`, log
`/tmp/jed-swift-native-sim-cross-core-corrected.log`.

The remaining mismatch at that checkpoint was an adapter boundary: the crash image was saved before
open, but the Swift transcript hashed current bytes after open repaired divergent
credential blocks. Every emitted step-145 image is identical and hashes
`fc79d4b1473b0db6` below its 41 × 1024-byte logical high-water. The supported
runners check and hash that saved image. The reviewed Swift correction now passes
the saved bytes to both checks, while ordinary live checks keep reading current
bytes. Its regression consumes the unchanged shared trace and compares every
crash transcript hash with its emitted image. The refreshed full comparison
now passes; no old failed evidence has been overwritten.

## Generated 22-profile, eight-seed sweep (2026-11-06)

The first complete generated sweep selected **176 traces** (all 22 frozen
profiles, seeds 1 through 8) using actor
`ecedde3298fef53ec972d0d0d55237ae1eb0b4fee5b7e741a2156069caad8a8f`.
Swift completed **131 passes and 45 local failures**; Rust, Go and TypeScript
completed all 176. The driver performed 459 transcript comparisons and checked
5,344 Swift images independently. Its 634 failure entries comprise 45 local
violations, 171 transcript disagreements (57 traces against three cores), 417
image disagreements, and the runner's nonzero exit. Downstream disagreements
are retained, not counted as independent root causes.

Exact evidence is retained under
`/tmp/jed-swift-sim-sweep-20261106-19-rfibm0`; console log:
`/tmp/jed-swift-native-sim-profiles-all22-eight-seeds.log`.

Reviewed corrections from that evidence:

- Simulated directory barriers now follow the shared void host operation: a
  barrier can mark death without converting the directory call into an I/O
  error. Later fallible I/O still fails. Thirty-nine local backup violations
  shared this first failure mechanism.
- Durability-floor advancement precedes the truncation lower-bound assertion,
  matching the shared oracle. The former order rejected two successful flushes
  against their superseded floor.
- Bootstrap meta with no manifest entries requires no data-file stabilization
  barrier, because its whole image is independently durable. Directory
  stabilization remains separate. Incremental generations retain their
  pre-write barrier. The old unconditional barrier changed encrypted fault
  timing, relaxed commit behavior, and surviving bytes.
- The simulated temporary-file host operation replaces a stale inode with one
  canonical create event. This avoids stale temporary-file `58P02` and an extra
  deletion event that changed the directory-crash prefix. Native creation and
  replacement behavior is unchanged.
- Failed maintenance checks account for a page client's older resident view;
  credential changes compare that local view before and after. A failed
  replacement still cannot publish a newer generation.

The focused host/SQL gate passed 19 tests with these corrections, including all
selected simulator boundary and runner tests. The next full sweep ran under
`/tmp/jed-swift-sim-sweep-20261106-19-89lt3a`, actor
`56ea638a5fa57adb96153fd9cb737c4fcaa9e0bc93ad520cda77ad289ea6592a`.
Its completed result follows. At that point, investigations still included
attachment allocation-history differences and owner credential changes while a
remote client held a writer lease.

### Corrected host sweep: 176 traces, seeds 1–8

The unchanged 22-profile sweep under actor
`56ea638a5fa57adb96153fd9cb737c4fcaa9e0bc93ad520cda77ad289ea6592a`
completed in `/tmp/jed-swift-sim-sweep-20261106-19-89lt3a`.
Swift passed 173 traces and failed 3 local invariants; Go, Rust, and TypeScript
passed all 176 each. The driver compared 459 transcripts and independently
checked 6,884 Swift images (28,031 total emitted images). Its 112 failure entries
comprise 3 Swift local failures, 30 transcript disagreements, 78 cross-core image
disagreements, and the Swift runner's nonzero exit. These are related observations,
not 112 distinct defects. The documented per-core-fault exemptions are recorded
verbatim in `summary.json`; they are not acceptance of different SQL behavior.

All three local failures concern a newly created attachment disappearing after
an injected directory-barrier death (`attachments-2`,
`encryption-attachments-6`, `service-attachments-2`). The reviewed common host
create correction now closes the temporary device before publication and reopens
the published path after directory sync, so the first fallible operation reports
that death before creation returns and advances its durability floor. This patch
was published after the captured actor; this sweep does not verify it.

A read-only diagnostic of unchanged `attachments-5` prefixes, each followed by
one explicitly diagnostic `crash process` to emit an image, isolated the first
allocation mismatch to commit 450. Swift and Go are byte-identical through 443;
Swift then emits three extra overflow pages for a 544-byte value. The supported
cores re-fault a mixed resident root containing an inherited external reference
(Go `storage.go:660`, Rust `storage.rs:765`, TypeScript `storage.ts:792`), converting
its fresh neighboring values to stored references too. Swift retained those fresh
payload caches. Pure-fresh resident roots must continue rewriting their chains,
as `large-values.md` §15.2 specifies. The subsequent correction preserves that
distinction; it does not change the format or shared expected bytes. Diagnostic
inputs, provenance, images, and a temporary Go state probe are retained in
`/tmp/jed-sim-attachment-prefix`; the probe/task files were removed from the repo.

Other investigations at that checkpoint concerned owner credential changes
while a remote writer was idle, an encrypted attachment allocation mismatch and
a pinned-reader allocation mismatch. That run did not establish full acceptance.

The next reviewed host batch was published before a newly captured sweep:

- Mixed roots now re-fault after persistence through the ordinary page pool;
  remote clients use their already-sent sealed body rather than a post-ack RPC.
  A read failure preserves the known successful commit and poisons later writes.
  Memory staging performs the read before final cross-domain admission.
- Credential rewrap excludes local writers while allowing an idle remote writer
  grant. A streamed commit guard refuses key writes with `58030`; successful
  completion or exact-token abort clears the guard. This matches all three
  supported local-writer gates and pager checks.
- Rekey prepares its target key before refusing a page-client owner operation.
  `encryption-service-4` step 79 returned `55006` in every core, but Swift skipped
  the canonical 32-byte DEK, 32-byte salt and 12-byte nonce requests. Their
  80-byte seeded-stream consumption explains the later backup's exact nonce
  shift. Rejection still leaves the selected key and generation unchanged.

Independent reviews found no remaining blocker in these scoped changes. The
unchanged generated sweep, native host checks and mixed-process checks were
then required. Their later passes do not erase the historical 112-entry result.

The direct-file attachment mismatch is also isolated and corrected. Unchanged
`encryption-attachments-1` prefixes through 207 are byte-identical. At commit 214,
Swift consumed two persisted free pages while a root reader remained open; all
three supported direct-attachment paths preserve those pages until the common
reader registry drains (Go `execute.go:370`, Rust `shared.rs:2401`, TypeScript
`executor.ts:4189`). The corrected caller supplies a stricter watermark only for
`FileDatabaseHost` attachments; page-service domains keep their independent
watermarks. Independent review and the subsequent native physical reuse test
passed. Diagnostic prefixes and byte images are retained under
`/tmp/jed-sim-encrypted-allocation`.

The `reuse-under-pins-3` prefix through 270 shows the same mixed-root symptom:
all body pages before 239 match; Swift emits two extra overflow pages at 239–240
before rewriting the leaf, while Go retains the inherited chain and writes the
leaf directly. This is evidence for the already-corrected root residency path,
not a separate speculative storage change. The prefixes are retained under
`/tmp/jed-sim-reuse-prefix`.

The third integrated build passes all three root residency tests, both credential
writer tests and all six maintenance tests. Its native page-client socket tests
were refused by the sandbox (`listen` errno 1), so those remain unverified by
that run. Later approved native-host, full generated-simulation and mixed-process
refreshes pass, as recorded in the current acceptance section.

### Corrected allocation and Q3 sweep: complete pass

The next unchanged generated sweep completed successfully with actor
`b4c1180774d7f48c4df911248156aaea822ec0e03a988f1b4cd871be0ed2e34f`:
**176/176 traces in each of Swift, Go, Rust and TypeScript, 459 transcript
comparisons, 7,049 independent Swift image checks, zero failures**. It covers
all 22 frozen profiles at seeds 1–8. The task exited zero; outputs are retained
in `/tmp/jed-swift-sim-sweep-20261106-19-lawllp`, with console log
`/tmp/jed-swift-native-sim-profiles-allocation-q3-fixed.log`. The frozen input
fingerprint remains
`369ad9def9a39d16e8812714f9100f1ad86df6822910021c32cfa19e4c2e7bfc`.

This verifies the reviewed create-publication, mixed-root residency, credential
writer, rekey entropy, direct-attachment reuse and Q3 corrections against the
selected generated traces. Earlier failed runs remain above as diagnostic
history. It is a finite seed sweep, not an exhaustive fault-space proof. The
later H07 cursor-open and H16 host-batching changes were outside that captured
actor. The refreshed current sweep and 496-pair process matrix now pass after
those changes. All-mode resource accounting and service SQL keep their separate
acceptance obligations; no performance measurement is implied.
