# C# experiment host and tool gates

Baseline: `b1a6affca7725e0b07a4f734eddb1c569ef76bcf`. Initial platform:
Linux x86-64. **Missing** means no acceptance evidence; it is never a skip waiver.

| Gate | Required proof | Status |
|---|---|---|
| Tooling isolation | Opt-in tasks, exact SDK, normal tasks need no .NET | Verified opt-in .NET 11 tasks; fixed baseline extraction and source capture |
| Shared corpus adapter | Values, precise types, names, errors, costs, real cursor drains; reject unknown directives | All applicable memory/plain/encrypted files pass all four compilation variants at capture34; mode exclusions and precise counts recorded in `VERIFICATION.md` |
| Public API | Database/session/prepared/rows lifetime, binding, cancellation, host functions | Database/session/prepared lifetimes, native binding/results, async rows/mapping, immutable environment, host registration/pins, cancellation and capabilities pass default/runtime-async JIT/NativeAOT host checks, including streaming batch replay and exceptional writer-pin admission cleanup |
| Encoding/format | Shared key and value vectors, golden images, exact metadata | Full scalar/container vectors, catalogs and plaintext/encrypted image invariants verified; broad peer interchange remains a separate gate |
| File I/O | Partial reads/writes, durable barrier, poisoning, recovery, replace and directory durability | Full host suite verifies awaited devices, validated COW, poisoning, geometric zeroed growth, Linux barriers and replacement protocols |
| Memory/storage accounts | Q1-Q4, cursor checkpoints, transaction retention, spill rejection behavior | Shared resource files and host invariants exercised; latest full backend inventory remains authoritative for omissions |
| Crypto | Primitive vectors, page codec, credentials, rekey and sealed scratch | Scoped dependency approved; shared vectors, encrypted recovery, credentials/rekey and scratch verified; scoped implementation review and all four host variants pass; the final stable fault sweep passes |
| Interchange | C# writes/reads plain and encrypted images with Rust/Go/TS in both directions | Plain/encrypted C#↔Go/Rust/Node reads, writes, backups and reopen verified by all 496 mixed process cases |
| Protocol-v2 | Same crash-clean lock domains; election, client reads/writes, promotion and owner watermark | Real Linux process scenarios and injected production-protocol traces verified; all final service/attachment fault traces pass |
| Concurrency | Shared schedules, parallel stress, mixed-core process corpus | All seven shared concurrency files and 31 shared process scenarios ×16 C#/Go/Rust/Node pairings pass; threaded stress passes C# memory/plain/encrypted hosts plus independent Go (16,000 checks, identical checksum) |
| Simulation | Saved regressions and generated traces, images and transcript comparison | All 85 stable traces pass: 19 saved plus 22 generated profiles ×3 seeds; 76 exact peer comparisons and nine intrinsic checks |
| PostgreSQL oracle | Checkout cluster and declared profile; no answer changes to fit C# | Supplement verified; four existing planning-time divergences ledgered |
| NativeAOT | Published host executes conformance; no dynamic engine code | Default and runtime-async NativeAOT pass full host plus all three SQL inventories at capture34, including final-review engine fixes |
| Performance | JIT and AOT, cold/warm, GC/allocation, durability, matched answers, fixed peer actors | Nine workloads ×3 repeats ×4 variants pass matched-answer checks; paired latency/startup/allocation/GC results and tmpfs limits recorded in BENCHMARKS.md |

The experimental baseline is complete and frozen. Engine34, source
`7124c0e6a67e3fb3955ef6df359faaa94d8d375702b887b72cc439a851a51594`, passes
all 12 storage/compiler SQL inventories, all four full host executables, the
496-case mixed-core process matrix and 16,000 threaded snapshot checks. All 763
distinct SQL files pass in at least one storage mode. The final stable simulation
sweep passes all 85 traces; diagnostic measurements cover all four compilation
variants with three repeats each. [VERIFICATION.md](../../impl/csharp-native/VERIFICATION.md)
owns counts, logs, artifacts and freeze proof. [BENCHMARKS.md](../../impl/csharp-native/BENCHMARKS.md)
records substantial current overhead and the tmpfs measurement limits. Earlier
evidence below retains its original source identity.

Use existing Ruby harness infrastructure where practical. Engine entry points may
not call a subprocess. Tooling may invoke independent peers for comparison only.
No test-framework or benchmark package is assumed approved; a dependency-free
console driver is sufficient for initial host tests and corpus transport.


The complete mixed process matrix passes **496/496 cases** (31 canonical scenarios,
all 16 pairings of C#, pinned Go, Rust and Node), including final image and backup
byte comparisons. Run `mise run csharp-native:process --cores csharp-native,go,rust,node`.
The recorded `--no-build` run used actor SHA-256
`7c0474be1fd5acf26fc93ece5635b0312c3a24becb1914a45f034e87438fe4e8`
and engine SHA-256
`a68bda470eb3fd161a36747f4c030dee55d7701177889013a9bfadd732e1aa90`;
log `/tmp/jed-csharp-process-mixed-all.log`. Peer sources are extracted from the
baseline above; builds run offline, and the shared process driver changes only
actor command paths. It retains every scenario and all cross-pair byte comparisons.
