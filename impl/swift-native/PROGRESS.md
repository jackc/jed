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
- No SQL parser/executor, database, pager/tree/commit or conformance adapter yet.
  No database-level encryption, recovery, process, simulation or interchange result.
- No performance baseline or Apple-platform verification yet.

## Verification ledger

2026-11-05, Linux x86-64, Swift 6.4.2, debug configuration:

| Command | Result |
|---|---|
| `mise run verify` | PASS: all default spec/fixture/catalog/codegen checks; 112 file goldens, 24 integrity images, 738 encoding vectors, corpus taxonomy and other existing checks |
| `mise run oracle:status` | PASS after sandbox socket access: existing PG18 `jed_oracle`, builtin C.UTF-8 and all profile pins agree; no cluster provisioning |
| `mise run swift-native:codegen --check` | PASS, 14 pinned source blobs, fingerprint `68920423d1c74f1aef9cda1b5238500020c0155564e1f1d665d2309d5de078b7` |
| `mise run swift-native:test` | PASS 32 foundation/crypto tests, then PASS 35 after byte-host/logical-size slice; 0 failures |
| `git diff --check` | PASS before first integrated foundation test |

The 35 tests include all 11 shared LZ4 and 41 decimal key vectors, arithmetic
invariants, UTF-8/byte corruption and host boundaries, resource precedence/floors,
memory/Unix byte-device behavior, digest/AES known answers, the three shared XAES
cases, two subkeys, one KEK, one shared Argon2 vector, wrapped-key ciphertexts,
and an upstream multi-lane Argon2 vector. They do not establish SQL/file parity.

Initial SwiftPM compile failed while SQLDecimal was not yet present, then four
missing `try` markers in decimal test autoclosures were corrected. No expected
results changed. Swift 6.4's default `swiftbuild` stalled when the sandbox denied
CFSocket creation; interrupted experiment processes were identified and stopped.
Opt-in tasks now select its supported `--build-system native` option, which builds
successfully and emits an upstream deprecation warning. No sandbox IPC dependency
is introduced into the engine. SwiftPM also warns about the direct swift-asn1
constraint, retained intentionally to pin CryptoExtras' required transitive package.

## Next steps

1. Active delegated slices: SQL lexer/script splitter (`host_inventory`), full
   numeric functions/guarded transcendental kernels (`scope_inventory`), key blocks
   and authenticated page codec (`crypto_audit`). Root owns integration/storage.
2. Build parser/resolver/evaluator and shared-corpus adapter over the established
   types, plus the pager/packed B+tree/validated-COW transaction path. Do not
   substitute decoded dictionary storage or engine-owned cursor collection.
3. Continue vertical slices across expressions, SQL, pager/storage and host APIs,
   verifying against shared data/corpus at every slice and retaining failing counts.
4. Complete full verification and independent reviews, then measured optimization
   and final verification/benchmark reports. Freeze only once acceptance is met.
