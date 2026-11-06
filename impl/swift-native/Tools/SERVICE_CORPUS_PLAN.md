# Native service SQL acceptance

Status: adapter implementation and independent review complete; cross-role SQL
acceptance must be established by the retained task reports. This document does
not claim a passing matrix.

`mise run swift-native:service` selects these six explicit **client → owner**
pairs: Swift→Rust, Swift→Go, Swift→TypeScript, Rust→Swift, Go→Swift and
TypeScript→Swift. Every client executes SQL with its own native engine. The
external owner exposes the existing `owner-idle` control protocol and ordinary
page-service transport; no SQL is delegated to the owner.

The task checks every frozen corpus file against the source baseline
`362e0798ac3a8bd04bd2da556044b6ec73bf468a` and verifies identical live inputs before
using supported runners. The original sequential disk eligibility rules apply:
concurrency schedules and authored `#skip: disk` files are reported exclusions.
Rows, errors, names, types and cost remain the unchanged corpus assertions.
A per-record ledger, completed-file list and exact role totals independently
reject missing files, missing records, early termination and local fallback.
The ledger does not set a query-memory budget or relax a cost expectation.

The frozen service adapters have four fixture-local exceptions:

- `collation/collation_upgrade.test`
- `collation/skew.test`
- `collation/skew_full.test`
- `storage/unparsable_definitions.test`

Their fixture replacement bypasses the per-record temporary-file reopen in the
supported runners. Swift preserves that canonical behavior and reports their
records separately. All other reopened service records must prove the current
handle is a real page client. No fixture record is silently skipped or relabeled
as service execution. Postbaseline shared files are listed and their role counts
are separated from the frozen comparison.

The Swift helper uses native `ProcessPageTransport`, bounded replies, deadlines
and explicit owner release before temporary-file deletion. The supported
conformance adapters admit the `swift-native` owner token only with an explicit
absolute executable `JED_SWIFT_PROCESS_ACTOR`; default task selections and owner
lists remain unchanged. The shared owner-idle command cannot pass credentials,
so encrypted service mode is rejected explicitly. The separate encrypted SQL
matrix covers the canonical encrypted disk mode.

The task captures immutable Swift XCTest/resource bundles, the Swift process
owner, and supported Go/Rust clients and owners. The Node executable, live TS
sources, lock addon and installed crypto dependency files are hash checked before
and after every run; a concurrent input change fails the comparison. Reports
retain actor hashes, source hashes, commands, logs, record ledgers, exact
exclusions and all failures. A whole-run deadline terminates the runner's process
group if a protocol peer stalls.

Useful invocations:

```text
mise run swift-native:service --check-adapter
mise run swift-native:service --no-swift-build --filter query/point_lookup.test
mise run swift-native:service --no-build
```

`--no-swift-build` builds fresh supported peer adapters while capturing existing
Swift binaries, allowing the parent coordinator to avoid concurrent SwiftPM.
`--no-build` requires all existing actors and builds nothing. `--pairs` and
`--filter` are explicit diagnostic subsets; an empty/unknown/duplicate pairing
or empty selection fails before a build. Only the complete six-pair unfiltered
run is labeled the full matrix, and success still requires zero failures.

The adapter self-check and selected Ruby/Go/Rust/TS syntax/format checks passed.
Three negative mise invocations (unsupported pair, duplicate pair, empty filter
selection) returned failure before actor invocation. These checks validate the
harness; they do not establish engine acceptance.

The first diagnostic completed all six directions on unchanged
`query/point_lookup.test`: **102/102 records, zero failures**, with 17 actual
page-client records per direction. The retained XCTest actor is
`02d0b80e6a83938b44641601838dda4c0342a4521175bf4a23952f03e0cae999`;
resource digest
`5834cf5c6274f1f8bd23b191fa18ac4e2a78e19654a6a9283277c0b7368eedb6`.
Evidence: `/tmp/jed-swift-service-corpus-20261106-4062030-42nfv4`, log
`/tmp/jed-swift-native-service-six-pair-first.log`. This is a diagnostic subset,
not full V06 acceptance.
