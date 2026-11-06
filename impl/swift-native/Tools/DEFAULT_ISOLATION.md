# Default-task and oracle audit

Audit baseline: `362e0798ac3a8bd04bd2da556044b6ec73bf468a`.
The original read-only integration audit on 2026-11-06 did not execute default
CI itself. Subsequent verification passed all 17 default checks and all 18 CI
steps; [VERIFICATION.md](../VERIFICATION.md) records those commands, logs and
the resolved intermediate failures. The final freeze changes documentation only.

## Default selection remains unchanged

The following tracked files/directories are byte-unchanged from the baseline:
`mise.toml`, `scripts/gate.rb`, `mise-tasks/verify`,
`mise-tasks/conformance/`, `scripts/sim.rb`, `.github/`, and `web/`.

- `test` and `ci` still dispatch only their original task lists. Neither
  installs Swift, resolves its package dependencies, or invokes its tests.
- Default conformance and encrypted/query-memory walks select Rust, Go and
  TypeScript. The default page-service pairing map is Rust→Go, Go→TS, TS→Rust.
- Default simulation selects `go,rust,ts`.
- Default process conformance selects `rust,go,node`. The shared driver gained
  explicit actor, corpus, fixture, serving-role and artifact overrides; without
  those environment variables its previous selections and behavior remain.
- Swift tasks are isolated under `mise-tasks/swift-native/`. The added
  `tree-regression` task is opt-in and selects only the supported cores.
- No website entry, CI dependency, default benchmark entry, or default core
  list was added for Swift. Existing website comments that mention a future
  Swift entry are unchanged baseline comments.

The explicit Swift service corpus adds one owner token, guarded by
`JED_SWIFT_PROCESS_ACTOR`, to these supported test adapters only:
`impl/go/cmd/conformance/{main.go,service.go}`,
`impl/rust/src/bin/conformance.rs` and `conformance/service.rs`, and
`impl/ts/src/bin/{conformance.ts,conformance_service.ts}`. The token validates an
absolute executable and asserts an actual page-client role. No default owner
selection changes, and the role assertion is inactive outside that explicit
experimental token. `swift-native:service` owns the new six-direction matrix;
its complete membership and four fixture-local exceptions are documented in
`SERVICE_CORPUS_PLAN.md`.

The process wrapper validates exact core tokens, requires Swift, rejects
duplicates and empty/unknown tokens, verifies the 31-file frozen inventory, and
rejects a filter selecting no files before any build or actor invocation. These
three negative invocations each failed as required, through mise:

```text
JED_PROCESS_CORES=rust mise run swift-native:process --no-build
JED_PROCESS_CORES=swift-native,swift-native mise run swift-native:process --no-build
JED_PROCESS_CORES=swift-native JED_PROCESS_FILTER=not-a-frozen-scenario-778 mise run swift-native:process --no-build
```

The opt-in process task additionally accepts `--pages-owner rust` to capture the
existing canonical Rust serving actor. Its default remains `swift-native`; the
shared driver and its Rust default are unchanged. The selected pages executable
and Swift actor are copied once and checked against before/after source hashes.
This serving selection does not replace the selected SQL actor.

The wrapper passed Ruby syntax validation and `git diff --check`. No SQL or
process acceptance is inferred from those negative checks.

## Verification commands and boundaries

`mise run verify` is the exact toolchain-light default check: its 17 entries
validate shared encoding/PRNG/corpus/page-service/file fixtures, function and
cost/estimator data, coercion capture, and supported-core generated-file drift.
It requires no Swift installation or SwiftPM build. The completed final run
passes all 17 checks in `/tmp/jed-swift-default-verify-final.log`; the subsequent
default CI also passes its verification step after the shared corrections.

`mise run ci` remains the full default merge gate. It includes default
conformance, unit/binding/CLI/migration tests, formatting/lint/typechecks,
process and simulation checks, browser tests, and metamorphic tools. Its broader
workload and required local services were coordinated separately from this
audit. The final canonical retry passes 18/18 steps in 137.9 seconds at
`/tmp/jed-swift-native-default-ci-gist-canonical-retry.log`, without adding Swift
to its dependencies. No browser/reference dependencies were provisioned by this
audit.

Relevant PostgreSQL checks use this checkout's mise environment without
overriding `PGHOST` or `PGPORT`:

```text
mise run oracle:status
mise run corpus:check spec/conformance/suites/query/gin_jsonb_root_string.test
mise run corpus:check spec/conformance/suites/ddl/reindex.test
mise run corpus:check spec/conformance/suites/query/analyze_oracle.test
```

All are already recorded as passing in the progress ledger. The GIN correction
also has a scoped PostgreSQL 18 forced-index probe documenting its index bug;
ordinary predicate/sequential evaluation supplies the expected SQL answer.
The new `statistics_kmv_boundary.test` concerns deterministic `jed_statistics`
facts, so PostgreSQL sampling estimates are not its oracle. Tree deletion,
recovery selection, page-fit, poisoned-owner tenure, bootstrap stabilization,
and cursor charge/checkpoint phases require the shared byte, simulation or
host probes recorded in `BASELINE_FINDINGS.md`; they have no PostgreSQL host
API equivalent. No blanket oracle pass is claimed for the entire frozen corpus.

Temporary supported-core cursor probes and their task wrappers were removed
after execution. Exact setup, inner commands and observations remain in
`BASELINE_FINDINGS.md`; their source/log copies are under
`/tmp/jed-cursor-cost-probe/`, outside the repository. Unrelated `tasks/` remains
untouched.
