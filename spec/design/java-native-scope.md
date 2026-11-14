# Java experiment scope and host matrix

This matrix describes the authorized experiment, not supported-core promotion.
Its frozen baseline is `3e5962f9affc06c768df1688765162a65a893848`, format v53.
The [charter](java-native-experiment.md) controls scope; immutable run identities
and limits belong in [VERIFICATION.md](../../impl/java-native/VERIFICATION.md).

| Area | Required scope | Current acceptance |
| --- | --- | --- |
| SQL, types, built-ins, errors, costs, plans | Entire frozen implemented surface | All nine complete SQL inventories pass on one frozen source |
| SQL inventory | All 766 files, with canonical mode exclusions | Per variant: 764 memory, 679 plain and 678 encrypted applicable files pass |
| Persisted storage | Exact v53 pages, validated COW, reclamation, encryption | Golden fixtures, interchange, process and complete recovery gates pass on all three variants |
| Shared processes | Protocol-v2 ownership, clients, reader pins, transport | Per variant: all 31 scenarios across 16 Java/Go/Rust/TypeScript pairings pass (496 cases) |
| Concurrency | Shared ordered schedules and unordered threaded invariants | Seven schedules pass sequential/threaded; per variant, 12,000 Java snapshot checks plus 4,000 Go checks pass |
| Faults and recovery | All 19 saved traces plus generated profiles | Per variant: 19 saved + 66 generated pass (76 exact peer comparisons, 9 intrinsic checks); four additional review traces pass |
| Java host APIs | Owned rows, prepared statements, cancellation, host functions, resource limits | Complete host suites pass on all three variants |
| Ordinary JDK 28 and value-class variant | Same semantic engine and applicable corpus | Both pass all 14 acceptance gates, matching the stable variant |
| Performance | Matched Go/Rust workloads, startup, warmup, steady state, memory, concurrent reads and durable writes | Completed 24 workloads × five actors × three repeats and 72 separate Java memory probes; exact results/costs match. Shared-host load and bounded warmup limit timings; converged steady state is not established |
| Default repository tasks | Java absent from normal setup/tests/CI | Complete rerun passes all 18 aggregate CI steps, including 20 browser tests. Java remains opt-in |
| PostgreSQL oracle | Applicable SQL regressions under declared profile | Profile checked; individual probes and their limitations recorded in verification |

| Host boundary | Implementation / constraint |
| --- | --- |
| Runtime | Pinned Temurin 25.0.5+8; no stable preview requirement |
| Platform | Linux x86-64; other platforms unverified and native file coordination fails closed |
| Memory storage | Packed page/tree seam, same logical SQL and resource contracts |
| Local files | JDK file I/O; narrow Linux FFM for `flock`, directory durability and atomic no-replace publication |
| Multiple processes | Stable lock bundle and owner page service; no PID files or stale-lock stealing |
| Remote transport | Host-supplied duplex `PageTransport` and `PageRelay`; core opens no network listener |
| Scheduling | Blocking public API; ordinary or virtual threads; no required executor or async wrapper |
| Crypto | JDK digests, AES and HKDF; approved BC 1.86 only for Argon2id and its necessary helpers |
| Packaging | Local JAR experiment; no publication, JDBC, ORM or reactive adapter |
| Preview | Explicit value objects on pinned JDK 28 EA; no preview semantic fork |

Guardrails are deterministic logical budgets, not hard JVM heap limits. Native
host operations and cryptographic provider internals do not make the handwritten
Java engine a wrapper around another core. Production delivery, other platforms,
GraalVM Native Image and promotion remain outside this authorization.
