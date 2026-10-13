# Deterministic simulation testing — design

> **Status: proposed — design only, nothing built.** A seeded simulator that runs long, randomized
> multi-session histories against a database whose storage is a fault-injecting simulated disk.
> Each run is a pure function of its seed: a failure replays exactly, shrinks to a small trace, and
> becomes a regression test. It targets the **storage, commit, and recovery state machine**
> ([validated-cow.md](validated-cow.md), [storage.md](storage.md) §7, [locking.md](locking.md)), not
> SQL semantics. It is a test harness only. It changes no engine behavior, file bytes, or public API.
> When this lands, add it to CLAUDE.md §7 beside the concurrency layers.

Prior art: FoundationDB's simulator, TigerBeetle's VOPR, and Antithesis's deterministic hypervisor.
Those systems had to build a deterministic runtime (simulated network, actors, and scheduler) or
virtualize the machine. jed needs neither. Its engine is single-threaded, its I/O, clock, and
randomness already sit behind host seams, and its results already depend only on logical order (§2).
The simulator is a harness around the existing seams, not a change to the runtime.

## 1. The gap this closes

The storage layer is tested by several harnesses. Each covers **one axis**:

| Harness | Exercises | Does not exercise |
|---|---|---|
| Per-core crash tests (`impl/go/crash_*_test.go`, `impl/rust/src/validated_cow_tests.rs`, `impl/ts/tests/validated_cow.test.ts`) | Every write subset or tear of **one** commit from a hand-built starting state | Long histories, free-list reuse after many commits, pinned readers, repeated crashes |
| Corpus + NoREC ([conformance.md](conformance.md)) | Query semantics, cross-core agreement | Faults of any kind |
| Concurrency Layers 1–3 ([concurrency-testing.md](concurrency-testing.md)) | Snapshot isolation, watermark, gate, real threads | Crashes, I/O errors, file-backed reuse under load |
| Process corpus, Layer 4 | Real kernel locks, real process death, cross-core files | Breadth: every scenario is hand-written |

Storage bugs tend to be **combinations**: a power loss during a commit that reuses free-list pages
freed after a pinned reader closed, in a transaction whose manifest overflowed, on a handle that
spilled a sort. Nobody writes that test by hand. Its preconditions are each tested; their
conjunction is not. A simulator explores conjunctions by construction, and because it is
deterministic, the one-in-a-million interleaving it finds reproduces on every run.

## 2. Why jed can do this cheaply

A simulation run must be a pure function of `(seed, trace, core)`. jed already supplies almost
every precondition:

- **Storage is a five-method seam.** Every durable byte passes through `BlockStore`
  (`read_at`/`write_at`/`sync`/`size`/`set_size`, [hosts.md](hosts.md) §2). All three cores already
  open a durable engine over a caller-supplied store in their crash tests.
- **Clock and randomness are seams.** `SeededRandomSource` and an injectable clock
  ([entropy.md](entropy.md)) make `now()`, `uuidv4()`, and `uuidv7()` deterministic. Their splitmix64
  stream is byte-pinned in `spec/encoding/prng.toml`.
- **Concurrency depends only on logical order.** Read results are a function of commit order and
  the version each reader pinned ([concurrency-testing.md](concurrency-testing.md) §2). A
  single-threaded scheduler choosing which session steps next reproduces every observable
  interleaving.
- **The engine has no background work on the paths simulated.** In production code, the only
  goroutines and threads are the shared-mode coordinator's probe and Go's context-cancellation
  watcher. Neither runs unless shared locking or a cancellable context is in use.
- **Allocation is deterministic.** The free-list and manifest planning algorithm is specified
  ([validated-cow.md](validated-cow.md) §2) and produces byte-identical files across cores, so the
  **set** of pages a commit writes, and their bytes, is identical in every core.
- **Results, plans, and cost are deterministic by contract** (CLAUDE.md §10). A transcript of a long
  history can be compared exactly across cores.

What is not covered: real OS threads, the real kernel lock primitives, and real device behavior.
Those stay with the race-detector modes, Layer 4, and the durability benchmarks (§12).

## 3. Scope and non-goals

**In scope:** the validated-COW commit and recovery protocol, free-list reuse and the reader
watermark, manifest overflow, live-page-count bookkeeping, unchanged-commit elision, poisoned
handles, recovered-generation stabilization, large-value overflow chains, buffer-pool eviction,
spill, temp storage, `max_storage_bytes`, and, in later slices, compaction, file attachments, and
the shared-process lock protocol.

**Non-goals:**
- **SQL semantics.** The PostgreSQL oracle, the cross-core corpus, and NoREC already cover them. The
  simulator issues SQL only to drive storage.
- **Performance.** Simulated time is not wall time.
- **Validating real devices.** The simulator checks the engine against a *model* of the disk (§6.4).
  Whether a real SSD honors that model is a separate question.
- **Replacing any existing harness** (§12).

## 4. Architecture

```
 seed ──► scripts/sim_gen.rb ──► trace (.sim, language-neutral data)
                                   │
            ┌──────────────────────┼──────────────────────┐
            ▼                      ▼                      ▼
     Go sim runner          Rust sim runner         TS sim runner        (thin, per core)
     ├ SimDisk (BlockStore, fault model §6)
     ├ seeded clock + entropy seams (§7)
     ├ step executor over Database sessions
     └ oracles: generation history, durability floor, integrity check (§8)
            │                      │                      │
            └──────────► transcript JSONL + crash images ◄┘
                                   │
                    scripts/sim.rb: compare cores, report, shrink (§10)
```

**The generator is shared data, authored once** (CLAUDE.md §5). A Ruby generator emits a trace. Each
core's runner only executes it and checks oracles, so the per-core code is the simulated disk, the
step executor, and the integrity checker. This mirrors the existing split: `norec_gen.rb` and
`rqg_gen.rb` generate, and the conformance harnesses execute.

## 5. The trace format

A trace reuses the concurrency vocabulary ([concurrency-testing.md](concurrency-testing.md) §4.1)
with no expected results. The generator cannot know them, and the oracles do not need them. A trace
is a header plus one step per line:

```
# format: simulation
# seed: 81723
# page_size: 256
# cache_bytes: 4096
# work_mem: 8192
open w write
on w CREATE TABLE t (id i32 PRIMARY KEY, v text)
on w INSERT INTO t SELECT g, repeat('x', g * 37) FROM generate_series(1, 200) g
commit w
open r read
open w2 write
on w2 UPDATE t SET v = v || 'y' WHERE id % 3 = 0
fault power_loss survive=500 tear=100
commit w2
recover
check
close r
```

| Step | Meaning |
|---|---|
| `open <sid> read\|write`, `on <sid> <sql>`, `commit`, `rollback`, `close` | As in the concurrency format. The SQL's outcome is recorded, not asserted. |
| `fault <kind> <params>` | Arms a fault (§6.3) for the **next** step only. |
| `crash process` / `crash power <params>` | An immediate crash between steps. |
| `recover` | Opens a new `Database` over the surviving disk image, the recovery path. |
| `reopen` | Clean `close` then open. |
| `check` | Runs the integrity checker (§8.3). |
| `clock +<micros>` | Advances simulated time. |
| `set <setting> <value>` | A handle setting (`cache_bytes`, `work_mem`, `max_storage_bytes`, …). |

**Header knobs** select the starting configuration. The most important is a **small page size**
(256 or 512). It produces deep trees, multi-page overflow chains, and manifest overflow with very
little data. At 256-byte pages the inline manifest holds `floor((256 − 64) / 12) = 16` entries, so
any commit dirtying more than 16 pages exercises the overflow chain. A small database also keeps the
per-commit digest (§8.1) cheap.

A crash ends every session of the crashed `Database`. The generator never references them afterward.

## 6. The simulated disk

### 6.1 State

`SimDisk` implements `BlockStore` over three pieces of state:

- **durable**: the bytes that survive a power loss unconditionally;
- **pending**: for each page offset, the ordered list of versions written since that offset was last
  covered by a barrier;
- **cache**: what reads return, meaning durable overlaid with the newest pending version (the OS
  page cache's read-your-writes).

`write_at` appends a pending version and updates the cache. `sync` and `set_size` succeed by moving
every pending write to durable. `set_size` is a full `fsync` by contract, so it is a barrier too
(hosts.md §2.1). `read_at` serves the cache.

### 6.2 Content-keyed fault decisions

Fault outcomes are decided by **what** was written and **where**, never by the position of a write
in the operation log:

```
decide(seed, lane, step, offset, version) = splitmix64_mix(seed, lane, step, offset, version)
```

Here `lane` separates independent decisions (survival, version choice, per-sector tear), `step` is
the trace step index, `offset` is the byte offset, and `version` is the version's index in that
offset's pending list. The exact mixing function is pinned by new vectors in
`spec/encoding/prng.toml` and checked by `prng_verify.rb`.

This is what makes crash images **comparable across cores** (§8.4). Within a commit, write order is
not a cross-core contract and may differ by core, but the set of `(offset, bytes)` pairs does not.
With order-independent decisions, three cores that wrote the same pages produce the **same
post-crash image byte for byte**, and must all recover the same generation from it.

The model is also order-independent in its assumptions. The validated-COW protocol claims nothing
about the order of unsynced writes ([storage.md](storage.md) §7), so a model that allows any subset
is sound: a failure under it is a real protocol violation, not a modeling artifact.

### 6.3 Fault kinds

| Kind | Model | Expected outcome | Cross-core tier |
|---|---|---|---|
| `power_loss survive=‰ tear=‰ sector=512` | For each pending offset, `decide` chooses "nothing lands" (durable stays) or one pending version, **any** version, not only the newest, since an older one may have been written back first. With probability `tear`, the chosen version is mixed sector by sector with the durable bytes. Cache is discarded. | Recovery selects a generation allowed by §8.1–8.2, never `XX001`. | Image + transcript |
| `process_crash` | Armed on a step: each write of that step is independently issued or not (§6.2). The `Database` is dropped without `close`. Issued writes stay in cache and remain **pending**, not durable, so a later power loss can still drop them. | The next opener sees the cache. Before its first write, a writable opener must stabilize an adopted unacknowledged generation (validated-cow §3). A later `crash power` checks that it did. | Image + transcript |
| `io_error op=write\|sync\|set_size p=‰` | The first matching call whose `decide` falls under `p` returns a host error. A failed `sync` follows Linux "fsyncgate" semantics: each pending write independently lands or vanishes, then the error is reported. | The statement fails `58030`. The handle is poisoned: no later commit continues from an assumed root. `recover` yields the prior or in-flight generation. | Transcript only |
| `enospc` | `set_size` growth fails. | As `io_error`. | Transcript only |
| `read_error p=‰` | `read_at` fails. | The statement fails `58030` or returns rows matching the history. Recovery never silently selects an older commit because a read failed (validated-cow §1). | Per-core only: which pages are read depends on buffer-pool policy, which is per-core |
| `corrupt flips=n` | Opt-in mode: flip bits in durable, non-pending pages. | `XX001` on access, or inert if the page is dead. **Never wrong rows.** | Per-core only, intrinsic oracle |

Writes issued after a fault fires never reach the disk.

### 6.4 What the model assumes

The simulator is only as adversarial as this list is pessimistic. Each assumption is deliberate and
documented so a reader knows what a green sweep does **not** prove:

- A successful barrier makes every prior write durable. There is no lying `fsync`, which matches the
  host contract jed already states.
- Writes to distinct offsets are independent. There are no misdirected writes (a page landing at
  the wrong offset).
- A torn page is a per-sector mix of old and new bytes at the declared sector size. The default of
  512 is more adversarial than 4 KiB atomic units.
- A process crash preserves the kernel cache. A power loss preserves only durable bytes plus
  whatever subset the model chooses.
- Reads after an error-free write return that write.

## 7. Scheduling, clock, and entropy

The **generator** is the scheduler. It interleaves sessions in the trace using the same rules as the
stress harness's seeded-sequential interleaver ([concurrency-testing.md](concurrency-testing.md) §6):
a write-open is emitted only while the gate is free in its model, readers open and close freely, and
a reader may stay pinned across many commits. That reader-pinned-across-reuse shape is the one the
watermark exists to protect, and the profiles weight it heavily (§9). The runner executes steps in
order on one thread. That is sound by §2: every real interleaving consistent with the trace's order
produces the same observations.

The runner installs a `SeededRandomSource` derived from the trace seed and a clock that starts at a
header-given instant and advances only on `clock` steps. Nothing in a run reads wall time or OS
entropy.

## 8. Oracles

### 8.1 The generation history

The runner keeps a history of **logical digests** keyed by version:

- After each acknowledged commit, it opens a read session with faults paused and computes the digest
  of the newly committed version.
- Immediately before each commit, it computes the digest of the in-flight version **inside the write
  session**, which sees its own writes.

A digest is computed through SQL so every core can produce it identically. It covers the catalog
(the `jed_tables`, `jed_columns`, `jed_indexes`, `jed_constraints`, and `jed_sequences` relations,
[introspection.md](introspection.md)), every table's rows in primary-key order (all columns for a keyless
table), and every sequence's state. The digest folds these with FNV-1a 64, the bench answer checksum.

**After every `recover`, the recovered version must be in the allowed set (§8.2) and its digest must
equal the recorded digest for that version.** This catches torn or mixed snapshots, lost index
updates, wrong row counts, and recovery of a generation that never existed, without a hand-written
model of SQL semantics. The engine's own fault-free behavior is the reference. That is acceptable
here because the target is durability: a semantics bug shared by the faulted and unfaulted paths
belongs to the corpus.

### 8.2 The durability floor

- The **floor** is the newest version whose commit was **acknowledged** under `synchronous = on`.
  The commit returned success, so its barrier completed. A completed stabilization sync also raises
  the floor to the adopted version.
- The **allowed set** after a crash is every version from the floor through the newest version whose
  commit was **attempted**. Under a relaxed `synchronous` mode, the floor is the last explicit
  durability point.
- **Recovering below the floor is a durability violation.** Recovering above the latest attempted
  version, or failing with `XX001` outside corruption mode, is a corruption violation.

### 8.3 Structural invariants

`check` steps, and every `recover`, run a per-core, test-only integrity checker. It walks the image
the engine actually selected and asserts:

- the selected meta slot validates: its checksum, manifest, and every listed dependency digest;
- every reachable page has a valid CRC and the page type its referrer expects;
- the B+tree shape invariants ([bplus-reshape.md](bplus-reshape.md)): ordered keys within and across
  leaves, separators bounding their children, and uniform leaf depth;
- each table's catalog `row_count` equals its leaf total, and `root == 0` exactly when it is zero;
- every secondary, unique, GIN, and GiST index holds exactly the entries derived from its table's
  rows, and a unique index has no duplicates;
- overflow chains decode, including LZ4, to their recorded lengths, and no page belongs to two chains;
- the live page count at meta offset 56 equals the number of pages reachable from the catalog root;
- the free list has no duplicates and is disjoint from the reachable set and from the selected
  generation's manifest dependencies, and every referenced page lies below the high-water;
- **bounded growth**: in exclusive mode over a long run, the high-water stays within a per-profile
  bound of the live page count, which catches leaked orphans. This is the process corpus's
  `max_page_count` idea applied continuously.

A violation reports the invariant, the version, and the page.

### 8.4 Cross-core agreement

When a trace runs on several cores, the driver requires identical transcripts. Per step, the
transcript records the outcome (`ok`, a row digest, or a SQLSTATE), the metered cost, the version
after commit, whether an armed fault fired, and the version `recover` selected. For faults marked
"image" in §6.3, the driver also requires a **byte-identical logical post-crash image**: pages below
the high-water, ignoring preallocated tails.

This tier carries the CLAUDE.md §2 cross-core comparison into histories nobody wrote by hand. Three
independently written recovery paths look at the same torn bytes and must make the same decision.
Cost appears in the transcript because it is a contract and depends on tree shape, which long
reuse-heavy histories exercise more than the corpus does.

### 8.5 Self-determinism

Every run executes twice in the same core. The two transcripts and the two full `BlockStore`
operation logs (every write's offset and bytes, every barrier) must match exactly. This makes the
simulator a **nondeterminism detector**. Go randomizes map iteration on every run, so a map-ordered
loop leaking into allocation order, record layout, or error choice fails here even if every result
looks right.

## 9. The generator and workload profiles

`scripts/sim_gen.rb` keeps a model of the schema, open sessions, gate holder, pinned readers, and
armed faults. With that model it emits mostly valid statements, plus a configurable small fraction
of invalid ones, so error paths (`23505`, `42P01`, `54P06`, serialization-time errors) run without
poisoning anything they should not. After a fault the model knows the consequences: it emits
`recover` after a crash, and after a poisoning I/O error it drops the dead handle's sessions.

Weights live in profiles (`spec/conformance/sim/profiles/*.toml`). The first set:

| Profile | Emphasis |
|---|---|
| `reuse-under-pins` | Long-pinned readers, many small commits, free-list churn, crashes during reuse |
| `large-values` | Overflow chains, LZ4-compressed records, `UPDATE`s that grow and shrink values |
| `big-commits` | Transactions dirtying hundreds of pages: manifest overflow, free-list/manifest planning retries |
| `ddl-churn` | `CREATE`/`DROP`/`ALTER` of tables and indexes, sequences, catalog-chain growth |
| `crash-storm` | Repeated process and power crashes, including crashes during recovery stabilization |
| `resource` | Tiny `cache_bytes`, `work_mem`, `temp_buffers`, and `max_storage_bytes` (added in slice S3) |

A profile changes what is emitted, never what is checked: every oracle runs on every trace.

## 10. Shrinking and the regression corpus

A failing trace shrinks with the ddmin approach of `scripts/reduce.rb`, applied to **steps**.
Dropping an `open` drops that session's later steps, and the failure predicate is "the same oracle
fails on the same core". Fault parameters shrink too, for example by lowering the survival rate
toward the minimal failing subset. The minimized trace is committed under
`spec/conformance/sim/regress/` with a one-line comment naming the bug.

Regression traces carry **no pinned transcript**. They must pass every oracle on every core and agree
across cores. A pinned transcript would break on unrelated changes, such as a new SQLSTATE, and the
oracles are what the test means.

## 11. Placement, tasks, and CI

- **Generator and driver:** `scripts/sim_gen.rb` and `scripts/sim.rb`, Ruby per CLAUDE.md §10.
- **Runners:** one per core, test-only, reading many traces per process so startup cost is
  amortized. A runner needs to open an engine over a caller-supplied `BlockStore`. Go runs it as an
  internal test (`go test -run TestSim -args …`), as the existing crash tests do. Rust exposes the
  hook behind a non-default `sim` cargo feature. TypeScript already exports the interface.
- **Tasks:**
  - `mise run sim:regress` runs the regression corpus on all cores. It is in `mise run ci`, gated by a
    `sim.trace` capability.
  - `mise run sim:sweep [--profile P] [--seeds N] [--cores go,rust,ts]` runs a long seed sweep
    **outside** `mise run ci`, like `stress`, and emits JSONL.
  - `mise run sim:replay <trace|seed>` reruns one case with verbose step logging.
- **Failure artifacts:** for each failure the driver writes the seed, profile, trace, per-core
  transcripts, and the pre- and post-recovery images to a scratch directory. These are everything
  needed to debug without rerunning the sweep.

## 12. Relationship to existing harnesses

| Existing | Status alongside the simulator |
|---|---|
| Per-core exhaustive crash tests | **Keep.** They enumerate every subset of small commits, an exhaustiveness random search cannot promise. The simulator covers the long histories they cannot express. |
| `spec/fileformat/verify.rb` | **Extended in slice S6** with `--check-image`, an independent fourth voice for §8.3 and for which generation recovery should select, run on regression-corpus crash images. |
| Concurrency Layers 1–3 | **Keep.** The race-detector modes test real threads. The simulator tests faults over logical interleavings. |
| Process corpus (Layer 4) | **Keep, release-blocking.** Real kernel lock semantics and real process death cannot be simulated. Slice S5 explores the same protocol state machine randomly. |
| Durability benchmarks | **Keep.** Only real hardware says whether the barrier contract holds on a real device (§6.4). |

## 13. The obligation to grow it

Like the NoREC obligation (CLAUDE.md §10), the simulator explores only what its generator emits and
checks only what its checker knows. **When a change adds a page type, a commit path, a host
operation, or a storage setting, add its steps and faults to the generator and its invariants to the
checker in the same change.** A new seam a fault could target (a spill sink, a file factory, a lock
primitive) gets a simulated implementation.

## 14. Slices

1. **S1 — Foundation, Go runner.** The trace format, `sim_gen.rb` with sessions, DML/DDL, large
   values, and small pages, the `SimDisk` with `power_loss`, `process_crash`, and `io_error`, the
   clock and entropy injection, the history and floor oracles (§8.1–8.2), the Go integrity checker,
   self-determinism (§8.5), and `sim:sweep --cores go`.
   **Exit criterion: the simulator must catch bugs planted on purpose.** On a scratch branch, plant
   each of these, as the mutation harness does: skip recovered-generation stabilization; let the free
   list include a manifest dependency; skip one dependency digest check; return pages freed under a
   pinned reader. Each must be caught within a fixed seed budget. A simulator that has not caught a
   planted bug has not shown it can catch a real one.
2. **S2 — Rust and TS runners, cross-core tier.** The content-keyed decision vectors in `prng.toml`,
   transcript and image comparison (§8.4), the regression corpus, and `sim:regress` in CI.
3. **S3 — Resource knobs.** Buffer-pool eviction under tiny `cache_bytes`, spill under tiny
   `work_mem`, temp spill, `max_storage_bytes`, and `max_query_memory_bytes`. Go and Rust spill
   straight to `os`/`std` temp files today, so this slice gives them the `SpillSink` seam TypeScript
   already has, letting `io_error` target spill. `read_error` (per-core) lands here.
4. **S4 — Simulated filesystem.** A `SimFs` beneath `SimDisk`: named files, `rename`, `unlink`, and a
   directory barrier, where a rename becomes durable only after its directory sync. This covers
   `compact` (`.jedtmp` plus rename), file attachments with the N-root commit, and `create`.
5. **S5 — Simulated lock host and multiple processes.** A lock-primitive seam beneath the coordinator
   (today `flock`/`LockFileEx` on concrete files), with a simulated implementation shared by several
   simulated processes over one `SimFs`. A simulated process crash releases its locks, as the kernel
   does. The presence probe and lock timeouts run on the simulated clock. This explores alone↔shared
   transitions, append-only co-resident commits, and foreign-generation adoption randomly.
   Layer 4 stays the authority for real kernel behavior.
6. **S6 — Independent checker.** `verify.rb --check-image`, run on regression crash images (§12).

## 15. Open questions

- **Generator throughput.** A Ruby generator keeps the trace language-neutral and authored once.
  If sweeps become generator-bound, an accelerated single-core generator is an option. It must emit
  the same trace format so its finds still replay everywhere. Decide after S1 measurements.
- **The Rust hook's shape.** Is a `sim` feature flag the right boundary, or should "open over a
  host-supplied `BlockStore`" become a real host-API capability? hosts.md already treats hosts as
  pluggable. That decision belongs to the API, not this harness.
- **CI budget.** How many regression traces and how long a sweep slice `mise run ci` can afford.
- **Sector size defaults.** 512-byte tears are the adversarial default. Whether to also sweep
  4 KiB, which matches SSD atomic-write units, is a profile choice.
