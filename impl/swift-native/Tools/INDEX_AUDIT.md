# Native index foundations

The experimental Swift core reads the frozen baseline
`362e0798ac3a8bd04bd2da556044b6ec73bf468a`. These checks concern native index
storage and kernels; they do not establish complete SQL planning, execution,
concurrency, or database lifecycle conformance.

## GiST

`GistBounds.swift`, `GistTree.swift`, and `GistPages.swift` implement all admitted
fixed scalar and range opclasses, tuple bounds, the canonical fanout-four R-tree,
and page types 5/6. The tree rebuilds in canonical leaf-key order. This deliberately
matches the baseline's resident GiST tree and whole-tree commit rewrite; it is
not a residency assumption for table data or B+tree/GIN indexes.

Six boundary tests cover the shared plain GiST goldens and
`encrypted_index_table.jed`, reproducing the original page IDs, payloads, tags,
and ciphertext with the fixtures' nonce sequence. They also cover all opclasses,
canonical insertion order, exact query-bound rules, empty-range fallbacks,
logical empty-tree visits, multi-column ordering, preserved decimal bound
representation, malformed bounds/links/padding, and failed preparation leaving
allocation unchanged.

The independent review by the crypto/codec agent found no concrete format or
algorithm defect. Their requested encrypted golden coverage is included.

## GIN

`GinTerms.swift`, `GinQuery.swift`, `GinPostingRun.swift`, `GinIndex.swift`,
`GinGather.swift`, `GinTermCodec.swift`, and `TreeRange.swift` implement the frozen
opclasses and inverted-index machinery. GIN shares the packed B+tree and bounded
leaf pool. Posting runs use the 64-byte v47 front-coded format; singleton runs
may exceed that target. Build packing is greedy, edits use the specified split,
and deletion does not merge neighboring runs.

Six boundary tests exercise every frozen posting-run codec/packing/history
vector and JSON term vector, numeric/float/interval term canonicalization,
128-byte truncation, multi-column framing, candidate skip scanning, source-arm
cost accumulation, cancellation, candidate ownership/release, and atomic edits.
Nine plain/encrypted GIN files rebuild from their decoded postings to the exact
original page bytes. The encrypted case uses each original nonce and checks
bound pointers and ciphertext, rather than only checking decoded records.

Logical page spans come from skeleton metadata independently of cache misses.
`GinGatherResult.pageReads` is returned for the executor to add to the selected
access path's table visits and scan-block cost. `gin_entry` is accumulated in the
baseline order and charged after gathering. The boundary tests check candidate
sets, repeated-arm entry charges, and counter relationships. Full shared SQL
cost parity still requires the planner/executor adapter and its disk pass.

### Baseline accounting limit

The frozen memory schedule does not reserve Q1/Q2 bytes for GIN posting/candidate
key buffers. The Swift `GinCandidates` owner makes release explicit at consumption
or close and preserves that zero charge, including with a finite query-memory
budget. Individual posting runs and physical leaf reads remain bounded; a whole
posting list or candidate set can still grow. Batched GIN entry charging also
does not bound retention before the gather finishes. This is a recorded baseline
guardrail gap, not a whole-engine memory guarantee. A new charge or trip point
requires a separate shared-spec/cross-core revision.

### Root-string extraction correction

The independent JSONPath extractor is in `GinJSONPath.swift`. A strict equality
with no final path accessor needs both KEY and VALUE string terms: a root string
is indexed as KEY. The frozen baseline incorrectly extracted VALUE alone and
lost a matching root string after adding a GIN index. A scoped Go reproduction
and PostgreSQL 18 forced-GIN probe confirmed the defect; the direct predicate and
forced sequential scan return the row.

The shared correction was reviewed independently and committed locally as
`2e13d1b8`. Rust, Go, and TypeScript apply the same narrow extraction rule. The
shared regression checks both `@@` and `@?` before and after index creation;
the oracle ledger records the PostgreSQL forced-index defect without overriding
query expectations. No page encoding or golden changed. This correction is
separate from the immutable baseline corpus snapshot.

## Verification

The root agent's `mise run swift-native:test` run at 17:35 UTC on 2026-11-05
passed all 146 tests, including all six GiST and six GIN tests. The same run
included the JSONPath term-extraction boundary assertions. This agent did not
run a competing SwiftPM process.

`mise run swift-native:codegen --check` subsequently passed with all 749 frozen
corpus files (742 SQL and seven concurrency), all 112 file goldens, and the GIN,
Unicode, and other shared mechanical vectors. Its fingerprint is
`37257dce1710682ff656ee83474597acd4adc91e8f040d8155e9d16e8b63ff50`.

## Unicode review

Independent source review of `UnicodeData.swift` and `Collation.swift` found no
concrete runtime defect in count/extent/scalar validation, immutable additive
loads, root/delta merge, longest-contraction selection, non-ignorable three-level
weight emission with the original-byte tie break, ASCII fallback, or pinned
simple/full casing. The shared vectors and bundle identities passed in the
146-test run. This review does not cover the still-unimplemented build-time
compiler or SQL collation derivation/compatibility integration.

The prose tailoring example in `spec/collation/README.md` places the contraction
count after the singles, while the frozen artifacts and supported cores use the
same counts-first header as full tables. Swift correctly follows the byte
fixtures; its decoder records this discrepancy in a comment. No canonical spec
file was silently edited during the frozen-core review.
