# Parity update (2026-11-12)

The maintainer requested this refresh of the independent Ruby experiment against
supported-core source revision `b1a6affc`. Its previous semantic baseline was
`56f49192`; the [October report](PARITY_UPDATE.md) and all historical performance
measurements remain unchanged. Ruby stays opt-in and experimental. Future changes
again require an explicit request; this update does not promote the core.

## Changes

- Format v51 fixed-width constrained decimals and v52 compact leaves: narrow
  directories, fixed keys, primary-key column elision, bare string bodies, key
  prefixes, integer offsets, string dictionaries, packed integer arrays, and
  multi-way splits. Incremental edits, compaction, integrity checks and encrypted
  files use the same layouts. Oversized delete rebalances retain their original
  siblings when redistribution would overflow the parent.
- Whole-row relation references, INSERT/SELECT parameter inference, parameterized
  LIMIT/OFFSET, timestamp type spellings and contextual array/range literals.
- Current numeric, float, interval, timezone, range, array, JSON, JSON_TABLE,
  regex, window, aggregate-scope, privilege and constraint behavior. Saved views
  retain table-function definitions. Deep legal IN lists and operand resolution
  no longer exhaust the Ruby stack or repeatedly resolve the same expression.
- Current cost accounting and query-memory thresholds, including projected scans
  without LIMIT/ORDER BY, window frame filters, JSON structural keys and compact
  pending records. The disk lane preserves grouped outer references and fraction
  array shapes, loads whole-row operands, and implements the bounded group
  table's measured handover to spill state. Mechanical catalog/schedule data was
  regenerated explicitly.
- `Database#serialize(name = 'main')`, `JedNative.deserialize(bytes, **options)`
  and `Database.deserialize`: committed snapshots, owned input bytes, preserved
  page size/version, attachment exports and the shared encryption boundaries.
- Transaction closures refuse early termination, failed closures report `25P02`,
  database close refuses an open writer, and SQL/host text rejects invalid UTF-8
  and NUL. Ruby binary strings remain accepted for `bytea` parameters.
- The opt-in host-test task runs each file in a fresh bounded process, keeping
  test-only overrides and retained VM stacks within that test's resource limits.

## Verification

Verification uses interpreter Ruby 4.0.7 and the current shared corpus, including
its pinned PostgreSQL-derived expectations and costs. No shared SQL expectations
were changed to make Ruby pass, and this update adds no dependency or performance
claim.

| Audit | Result |
|---|---:|
| Memory SQL/concurrency corpus | 22,019 / 22,019 |
| Disk corpus, plaintext | 19,800 / 19,800 |
| Disk corpus, encrypted | 19,765 / 19,765 |
| Encrypted cost-bearing outputs, exact transcript comparison with Go | 2,761 / 2,761 |
| Host/storage test files | 76 / 76 |
| Ruby/Go/Rust process scenarios | 279 / 279 |
| Shared readable byte fixtures | 107 |
| Shared byte-exact image encodes | 71, plus the ordered multi-way-split fixture |
| Integrity checks | 50 shared fixture runs; 105 applicable clean goldens |

The source baseline is `b1a6affca7725e0b07a4f734eddb1c569ef76bcf`.
Each corpus report records its initial and final Ruby implementation digest
(sorted `lib/**/*.rb`, SHA-256). Host tests pass in 76 isolated processes.
The full memory, host and process audits used digest
`9c3c8cb79d5c8d14b69c2bd8aacd8066bc6e1a02fae7c7cc0cc5b6581f5027ed`.
The final file audits began and ended at
`894854edf01e5b31754e8ebb3962e5bdc78df7a080a2568cbd7ae120297b04d5`,
which additionally fixes expanded-star sort bindings and avoids repeated schema
resolution when naming whole-row fields in reopened views. The latter revision
also passes all 51 whole-row memory records and all 22 deep-view disk records.
The memory audit started at 594 failures before the refresh. Its two skipped
files are the corpus's declared memory exclusions, not unsupported Ruby features.
Plaintext and encrypted file runs skip 86 and 87 files respectively, matching the
supported-core ledger. All three audits report zero unsupported files, timeouts
and memory-limit terminations. The Go encrypted audit itself passed all 676
applicable files; its complete transcript is identical, including repeated SQL
records and their ordering. All 72 changed/new Ruby files pass syntax checks, and
`git diff --check` is clean.
The PostgreSQL oracle was not rerun; this refresh uses existing shared expectations.

Reproduction (all tasks remain explicitly opt-in):

```sh
JED_NATIVE_QUERY_API=query JED_NATIVE_FILE_TIMEOUT=300 mise run ruby-native:conformance
JED_NATIVE_QUERY_API=query JED_NATIVE_STORAGE=disk JED_NATIVE_LOCKING=exclusive \
  JED_NATIVE_FILE_TIMEOUT=600 mise run ruby-native:conformance
JED_NATIVE_QUERY_API=query JED_NATIVE_STORAGE=disk JED_NATIVE_LOCKING=exclusive \
  JED_NATIVE_ENCRYPTED=1 JED_NATIVE_FILE_TIMEOUT=600 \
  JED_CONFORMANCE_TRANSCRIPT=/tmp/ruby-encrypted.tsv mise run ruby-native:conformance
mise run ruby-native:test
mise run ruby-native:process:build
mise run ruby-native:process:rust-build
JED_PROCESS_CORES=ruby-native,go,rust mise run ruby-native:process
```

Disk corpus runs use exclusive locking; the process corpus and host tests exercise
shared-owner/client operation. The disk and encrypted variants honor the shared
skip ledger for session state, concurrency, fixtures and variant-specific limits.

## Scope and remaining limitations

The [October omissions](PARITY_UPDATE.md#omissions-and-divergences) remain:
no page-service stream client or `jed pages` server; relaxed remote clients commit
synchronously; integrity checks materialize decoded rows/index entries, omit
catalog re-encoding checks and skip timezone-pinned indexes; rekey materializes
trees; opaque stored defaults cannot be rewritten on sequence rename. Ruby has no
simulator or cancellation API. TypeScript worker/async delivery and Go-only format
prototypes are outside this experiment. Supported-core performance optimizations
are not a requirement for identical SQL, cost or file-format behavior; existing
Ruby benchmark reports still describe their original revisions.
