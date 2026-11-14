# Parity update (2026-11-13)

The maintainer requested this refresh against supported-core source revision
`32feb48fd961598c2d7e60f0de284ba619a74ab0`. The previous semantic baseline was
`b1a6affca7725e0b07a4f734eddb1c569ef76bcf`; the [previous report](PARITY_UPDATE_2026-11-12.md)
and historical performance measurements remain unchanged. Ruby remains an opt-in
experiment, outside the Rust/Go/TypeScript differential set and automatic future
feature maintenance.

## Changes

- Format v53: eight-byte manifest entries, CRC-32 overflow-chain identity,
  per-page transaction stamps, and the plaintext/encrypted remap layout.
  Recovery validates the stamp window against the older structural meta and
  rejects malformed remap regions before selecting a generation.
- Deferred B-tree interiors: retain unchanged separators, replace child-slot
  mappings, discard retired parents' mappings, and fold stale interiors and
  their ancestors when the exact prospective list exceeds capacity. The
  migration-page rule and catalog retention follow the shared contract.
  Reopen and foreign adoption discard the retained catalog cache.
- Encrypted interior reads resolve both page and authentication tag through
  the selected generation's remap. Published snapshots, integrity checks,
  page-identical backups, compaction and serialization preserve the relevant
  generation or write a fresh resolved image.
- File commits list newly orphaned pages immediately, with per-page retirement
  generations for pinned readers, held pages across relaxed durability batches,
  and the shared leak-triggered rebuild. Foreign commits retain the specified
  unknown auxiliary-page behavior. Clients discard unpersisted singleton free
  lists rather than later treating them as owner-granted pages.
- Durable trailing truncation retains one shared growth step and remains best
  effort after the barrier.
- The shared sibling-leaf redistribution rule now precedes two-way splitting,
  restoring current tree shapes and GIN scan costs. No SQL expectations or cost
  assertions were changed to make Ruby pass.
- Host tests cover v53 shared histories, repeated deferred updates, folds,
  reopening, backups, integrity, independently rejected stale stamps, malformed
  remap bounds, and stale held-page protection. Historical bootstrap-image and
  compaction tests now distinguish commit histories and transaction stamps.

## Verification

The inventory is 764 SQL files, 121 database fixtures, 77 opt-in host-test files
and 31 process scenarios. Verification uses interpreter Ruby 4.0.7 on Linux.

| Audit | Result |
|---|---:|
| Full memory SQL/concurrency corpus | 22,064 / 22,064 |
| Full plaintext disk corpus | 19,845 / 19,845 |
| Full encrypted disk corpus | 19,810 / 19,810 |
| Host/storage test files | 77 / 77 |
| Ruby/Go/Rust process scenarios | 279 / 279 |
| Final plaintext storage corpus | 164 / 164 |
| Final encrypted storage corpus | 164 / 164 |
| Shared readable byte fixtures | 112 |
| Shared byte-exact bootstrap encodes | 71, plus the ordered multi-way fixture |
| Integrity checks | 50 fixture runs; 110 applicable clean goldens |

All full corpus runs report zero unsupported files, timeouts and memory-limit
terminations. Their 2 / 86 / 87 skipped files are the shared memory/plaintext/
encrypted exclusions. The full runs started at implementation digest
`a57a88af1f274091879b5760b7b6b270c1735a04ebde20bd8fb140e72b173356`.
The storage-refresh source digest is
`44d3d239566ea28469a4233a352a2238585fce0dddb07c99df17f7f3d7db272c`;
it additionally corrects the owner maintenance pin boundary: a completed client
write releases its old-generation pin before maintenance, and a pinned reader
prevents a rebuild/maintenance commit. Final host, process, and both storage
corpus reruns verify that correction separately. Both storage reruns and the
process run begin and end at that digest. `git diff --check` is clean.
Digests are SHA-256 over sorted
`lib/**/*.rb` contents, as recorded by the opt-in runners.

No dependency was added, the PostgreSQL oracle was not rerun, and no benchmark or
performance claim is made. Existing shared PostgreSQL-derived values, errors and
costs remain the SQL oracle.

Reproduction:

```sh
JED_NATIVE_QUERY_API=query JED_NATIVE_FILE_TIMEOUT=300 \
  mise run ruby-native:conformance
JED_NATIVE_QUERY_API=query JED_NATIVE_STORAGE=disk JED_NATIVE_LOCKING=exclusive \
  JED_NATIVE_FILE_TIMEOUT=600 mise run ruby-native:conformance
JED_NATIVE_QUERY_API=query JED_NATIVE_STORAGE=disk JED_NATIVE_LOCKING=exclusive \
  JED_NATIVE_ENCRYPTED=1 JED_NATIVE_FILE_TIMEOUT=600 \
  mise run ruby-native:conformance
JED_NATIVE_TIMEOUT=300 mise run ruby-native:test
mise run ruby-native:process:build
mise run ruby-native:process:rust-build
JED_PROCESS_CORES=ruby-native,go,rust mise run ruby-native:process
```

Set `JED_NATIVE_REPORT` for a corpus JSON report and `JED_NATIVE_RUN_REPORT` for
the process report. Corpus reports include initial/final implementation digests;
the process report also records runtime, limits and complete output.

## Integration on 2026-11-14

Rebased onto local master `f35a470bb0d53d94f2f92e5a4e9d0a315d914b68`,
which adds two shared corpus files (766 total). Integration also brings Ruby's
numeric resolver up to that revision: `div`/`gcd`/`lcm` validate decimal-island
arguments through the shared coercion table, preserve promoted integer widths,
and report the specified NULL result types; `width_bucket` returns `i32`.
Exact-type subquery membership retains the eager aggregate input memory account,
while mixed-float membership keeps the existing lane.

The integration source digest is
`cf53f77e2ff817b86c767c0c833d1989c430c3c916bb85a33bfc11a545eef668`.
The expression corpus passes 3,352/3,352 records and the resource corpus passes
1,013/1,013 records (two shared skips), including both new files. Earlier storage
and process results above remain evidence for the storage-refresh digest.

## Remaining experimental limitations

The [previous omissions](PARITY_UPDATE_2026-11-12.md#scope-and-remaining-limitations)
remain: no page-service stream client or `jed pages` server, synchronous relaxed
remote commits, no simulator or cancellation API, materializing integrity/rekey
paths, omitted catalog re-encoding and timezone-pinned-index integrity checks,
and opaque stored-default limitations on sequence rename. TypeScript's worker
and async delivery APIs are not Ruby APIs. Ruby's reclamation enumerates resident
live-page sets; it does not claim the supported cores' incremental-walk performance.
The existing prepared-plan-cache and runtime-compilation experiment boundaries
also remain unchanged. Production delivery, promotion, and performance comparisons
are separate decisions.
