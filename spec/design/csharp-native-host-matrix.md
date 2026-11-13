# C# experiment host and tool gates

Baseline: `b1a6affca7725e0b07a4f734eddb1c569ef76bcf`. Initial platform:
Linux x86-64. **Missing** means no acceptance evidence; it is never a skip waiver.

| Gate | Required proof | Status |
|---|---|---|
| Tooling isolation | Opt-in tasks, exact SDK, normal tasks need no .NET | Initial setup/build/codegen/test tasks verified |
| Shared corpus adapter | Values, precise types, names, errors, costs, real cursor drains; reject unknown directives | Scalar subset verified with negative adapter checks; full directive surface missing |
| Public API | Database/session/prepared/rows lifetime, binding, cancellation, host functions | Scalar async cursor/lifetime/cancellation subset verified; database/prepared/binding/extensions missing |
| Encoding/format | Shared key and value vectors, golden images, exact metadata | Missing |
| File I/O | Partial reads/writes, durable barrier, poisoning, recovery, replace and directory durability | Missing |
| Memory/storage accounts | Q1-Q4, cursor checkpoints, transaction retention, spill rejection behavior | Missing |
| Crypto | Primitive vectors, page codec, credentials, rekey and sealed scratch | Missing; dependency audit open |
| Interchange | C# writes/reads plain and encrypted images with Rust/Go/TS in both directions | Missing |
| Protocol-v2 | Same crash-clean lock domains; election, client reads/writes, promotion and owner watermark | Missing |
| Concurrency | Shared schedules, parallel stress, mixed-core process corpus | Missing |
| Simulation | Saved regressions and generated traces, images and transcript comparison | Missing |
| PostgreSQL oracle | Checkout cluster and declared profile; no answer changes to fit C# | Supplement verified; four existing planning-time divergences ledgered |
| NativeAOT | Published host executes conformance; no dynamic engine code | Initial slice verified; full-engine gate remains |
| Performance | JIT and AOT, cold/warm, GC/allocation, durability, matched answers, fixed peer actors | Unmeasured |

Use existing Ruby harness infrastructure where practical. Engine entry points may
not call a subprocess. Tooling may invoke independent peers for comparison only.
No test-framework or benchmark package is assumed approved; a dependency-free
console driver is sufficient for initial host tests and corpus transport.
