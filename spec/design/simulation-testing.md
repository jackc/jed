# Deterministic simulation testing — design

> **Status: S1–S10 landed in all three cores** (§14; S5, the shared-mode multi-process slice, was
> retired with shared mode and succeeded by S8, an owner and its page clients), including spill faults, temp-table spill, file
> attachments, `create`, the bounded-growth invariant, the `corrupt` mode, batched durability (S9), trailing truncation (S10), and backups (page-service.md §9). A seeded simulator that runs long, randomized
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
  goroutines and threads are the page service's (the owner's listener and connections, a client's
  socket) and Go's context-cancellation watcher. Neither runs on a simulated single-process trace; a
  multi-process trace (S8) settles the page service's connections after every step, so they add no
  ordering of their own.
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
spill and temp-table spill, `max_storage_bytes`, `create`, compaction, file attachments, and
at-rest corruption, and multiple processes under the page service: an owner and its page clients,
promotion, lost connections, and lost commit acknowledgements (§14, S8).

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
# profile: mixed
# seed: 81723
# page_size: 256
# cache_bytes: 4096
# clock: 1700000000000000
open w write
on w CREATE TABLE t (id i32 PRIMARY KEY, v text)
on w INSERT INTO t SELECT g, repeat('x', g * 37) FROM generate_series(1, 200) g
commit w
open r read
open w2 write
on w2 UPDATE t SET v = v || 'y' WHERE id % 3 = 0
fault power_loss survive=500 tear=100
commit w2
check
```

| Step | Meaning |
|---|---|
| `open <sid> read\|write`, `on <sid> <sql>`, `commit`, `rollback`, `close` | As in the concurrency format. The SQL's outcome is recorded, not asserted. A step naming a session that no longer exists is recorded as skipped. |
| `fault <kind> <params>` | Arms a fault (§6.3) for the **next** step only. |
| `crash process` / `crash power survive=‰ tear=‰` | An immediate crash between steps. |
| `reopen` | Ends every session (rolling back a writer), closes cleanly, and opens again. |
| `compact [<db>]` | Host compaction of `main` or a file attachment (api.md §2.6), through the simulated filesystem. |
| `backup page_identical\|compacted [p<N>]` | A backup of `main` (page-service.md §9) by process `p` (default: the lowest running), into a new file `<main>.backup<K>` through the simulated filesystem, while every other session keeps its pin and writer gate. Its oracle is §8.3's *Backups*. |
| `attach <name>` | Attaches the file database `<name>.jed` read-write (attached-databases.md §4), first creating it through the simulated filesystem if it does not exist. Every later process re-attaches every attachment whose file exists. In a service trace it attaches the file in every running process under the page service, main's owner first (S11). Attaching takes the writer gate, so the generator emits it only while no writer is open; a trace that attaches while one is (a shrinking artifact) is invalid, a violation of its own, as a second write-open is, rather than a wait that would hang a threaded core's single-threaded runner. |
| `corrupt flips=N [reseal=1]` | Bit rot in the main file (§6.3). Ends every session cleanly, flips bits, opens and reads, then repairs. |
| `check` | Runs the integrity checkers (§8.3). |
| `clock +<micros>` | Advances simulated time. |
| `spawn <p>`, `exit <p>`, `kill <p>`, `disconnect <p>` | Multi-process traces only (S8): start process `p` (owner when none runs, else a page client), close it cleanly (an owner drains its clients for its shutdown grace), crash it alone, or end its connection to the owner. `open <sid> <mode> p<N>` opens a session in `p` (default `p0`). |

**Every faulted step and every crash ends the process.** After the step, the runner abandons the
`Database` and its sessions and opens the disk as a new process would; that recovery and its oracle
checks are part of the step. There is no separate `recover` step.

**Header knobs** are `seed`, `page_size`, `cache_bytes` (0 or absent: the engine default), `clock`
(the simulated clock's start, in microseconds), `sector` (the tear granularity, default 64 bytes),
`synchronous` (`on`, the default, or `off`: every process batches its barriers, [validated-cow.md](validated-cow.md)
§4, and runs the cap flush inline so the barrier lands in the step that reached the cap), and the
informational `profile`. Four handle settings are applied at every open: `work_mem` and
`temp_buffers` (a trace session's; spill goes to the simulated spill host, §6.1, whose bytes are not
durable state), `max_query_memory_bytes` (a trace session's; the oracles' own reads are unlimited),
and `max_storage_bytes` (the database's file form, memory.md §8.7). Under either limit a commit can
fail for size reasons the generator cannot predict, so it treats every commit as uncertain. The most important is a **small page size**
(256 or 512). It produces deep trees, multi-page overflow chains, and manifest overflow with very
little data. At 256-byte pages the inline manifest holds `floor((256 − 64) / 12) = 16` entries, so
any commit dirtying more than 16 pages exercises the overflow chain. A small database also keeps the
per-commit digest (§8.1) cheap.

A crash ends every session of the crashed `Database`. The generator never references them afterward.

## 6. The simulated disk

### 6.1 State

The disk is a small **simulated filesystem** (`simFS`): named files, a directory, and the per-step
fault state. The directory has a kernel view and a durable view; creates, renames, and removes join
an ordered list of pending changes until a directory sync makes them durable. The database file is
`sim.jed`; each file attachment is `<name>.jed`; `create` and host compaction add their sibling
`.jedtmp` temp files. The filesystem is also the database's **file host** (`filehost.{go,rs,ts}`):
`create`'s existence check, temp file, rename, and directory sync, compaction's, and the open of a
file attachment all go through it in every core, so a crash can land between any two of those steps.
The runner creates `sim.jed` through it before the first step.

Spill scratch files go through a second seam, the **spill host** (`spillhost.go`, Rust's
`spillhost.rs`, TypeScript's `SpillSink`): sort runs, blocking-operator spools and hash partitions,
and a spilled temp domain's scratch block store. The simulated spill host keeps scratch files in
memory, outside the filesystem: their bytes are never durable state and vanish with the process. Its
operations (create, read, write) are what `spill_error` fails.

Each file implements `BlockStore` over three pieces of state:

- **durable**: the bytes that survive a power loss unconditionally;
- **pending**: for each page offset, the ordered list of versions written since that offset was last
  covered by a barrier;
- **cache**: what reads return, meaning durable overlaid with the newest pending version (the OS
  page cache's read-your-writes).

`write_at` appends a pending version and updates the cache. A successful `sync` makes every pending
write durable. A growing `set_size` does too: it is a full `fsync` by contract (hosts.md §2.1).
A shrinking `set_size` (trailing truncation, format.md) cuts the cache, the durable bytes, and the
pending writes past the new length at once: the most destructive outcome, as if the truncation
reached the disk before anything still pending. `read_at` serves the cache.

### 6.2 Content-keyed fault decisions

Fault outcomes are decided by **what** was written and **where**, never by the position of a write
in the operation log:

```
mix(z)  = z ^= z >> 30; z *= 0xBF58476D1CE4E5B9; z ^= z >> 27; z *= 0x94D049BB133111EB; z ^ z >> 31
decide(seed, lane, step, offset, version):
  h = seed
  for w in [lane, step, offset, version]: h = mix((h ^ w) + 0x9E3779B97F4A7C15)
  return h                                          (all arithmetic mod 2^64)
```

`mix` is splitmix64's finalizer, the mixer `spec/encoding/prng.toml` already pins. `step` is the
trace step index (0-based, counting only step lines), `offset` the byte offset, and `version` the
write's index in that offset's pending list. A permille test is `decide(…) % 1000 < p`. The `lane`
keeps independent decisions uncorrelated, and its values are part of the cross-core contract:

| Lane | Decision |
|---|---|
| 1 | power loss: does anything land at this offset? |
| 2 | power loss: which pending version lands (`% versions`) |
| 3 | power loss: is the landing version torn? |
| 4 | power loss: the sector's bytes, new or old (`& 1`), keyed by the sector's own offset |
| 5 | `io_error op=write`: does this write fail? |
| 6 | `process_crash`: was this write issued before the process died? |
| 7 | `io_error op=sync`: does this pending write land or vanish (`% 2`)? |
| 8 | a session's entropy-seam seed (`decide(seed, 8, step, 0, 0)` for the session opened at `step`) |
| 9 | `read_error`: do reads of this offset fail during the step? |
| 10 | power loss: how many pending directory changes survive (`% (changes + 1)`, an ordered prefix) |
| 11 | `spill_error`: does this spill-host operation fail? (`offset` is the operation's ordinal in the step) |
| 12 | `corrupt`: the page a flip lands in (`2 + h % (page_count − 2)`; `offset` is the flip's ordinal) |
| 13 | `corrupt`: the bit of that page it flips (`h % (page_size × 8)`; `offset` is the flip's ordinal) |

A file other than `sim.jed` adds `(FNV-1a 32(name) + 1) << 40` to every offset it decides on, so two
files' decisions stay independent while the database file's are unchanged by the others' existence.

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
| `power_loss survive=‰ tear=‰ [nth=N]` | The process runs until the step's `N`th barrier (default 1; file syncs and directory syncs both count), which never completes. Then, for each pending offset, `decide` chooses "nothing lands" (durable stays) or one pending version, **any** version, not only the newest, since an older one may have been written back first. With probability `tear`, the chosen version is mixed sector by sector with the durable bytes. Cache is discarded. The directory keeps its durable entries plus an ordered prefix of the pending changes (lane 10), as a journaling filesystem would. | Recovery selects a generation allowed by §8.1–8.2, never `XX001`. | Image + transcript |
| `process_crash issue=‰ [nth=N]` | Each write of the step is independently issued or not (§6.2). The live process sees all of its own writes; at its death, at the step's `N`th barrier, the never-issued ones vanish from the cache. The kernel directory survives. Issued writes stay **pending**, not durable, so a later power loss can still drop them. | The next opener sees the cache. Before its first write, a writable opener must stabilize an adopted unacknowledged generation (validated-cow §3). A later power loss checks that it did. | Image + transcript |
| `io_error op=write p=‰` | The first write whose `decide` falls under `p` fails and lands nothing. | The commit fails `58030` and the handle is poisoned; recovery yields the prior generation. | Transcript |
| `io_error op=sync nth=N` | The step's `N`th `sync` fails with Linux "fsyncgate" semantics: each pending write independently became durable or was dropped from the cache, and the error is reported once. | As above, but recovery may yield the in-flight generation if all of it landed. | Transcript |
| `read_error p=‰` | Every read of an offset whose lane-9 `decide` falls under `p` fails during the step. The runner's own oracle reads (digests, the live checker) are exempt — an early version applied the fault to them too and reported false violations. | The statement fails `58030`, never `XX001` or wrong rows. | Per-core: which reads happen depends on buffer-pool policy |
| `enospc` | The step's first growing `set_size` fails. | The commit fails `58030`. | Per-core: when the file grows depends on preallocation policy |
| `spill_error p=‰` | Each spill-host operation of the step fails when its lane-11 `decide` falls under `p`. | The statement fails `58030`; durable state is untouched. | Per-core: which operations spill, and how, is each core's own codec |
| `corrupt flips=n [reseal=1]` | A standalone step, not an armed fault. Every process ends cleanly; `n` bits flip in durable body pages of `sim.jed` below its page count (lanes 12–13), skipping a page with a pending write; durable bytes and cache both change. A new process opens the file and reads every row through the digest. Then the bits are repaired and the database must reopen exactly as it was. | Without `reseal`, every flip breaks its page's CRC: open and read each yield `XX001` or the right rows — the current version, or the previous one when a flip lands in the newest generation's dirty pages and recovery falls back (validated-cow.md §1). **Never wrong rows.** With `reseal`, the CRC is recomputed, so only the decoders' structural checks stand between the flip and the engine (storage.md §6): any engine error or any rows, but **never a panic or a non-engine error**. | Per-core only, intrinsic oracle |

A trace containing a per-core fault or a `corrupt` step runs every oracle on every core but is not
compared across cores (`scripts/sim.rb`), since one core may read, grow, or spill where another does
not.

A fault that never reaches its barrier (an unchanged commit, a statement inside a transaction) fires
at the end of the step. A directory sync with no pending change is not a fault point and does
nothing: it cannot lose anything, and counting it would renumber every later barrier. `set_size`
is never a fault point: preallocation is a host-policy detail,
and a model that killed the process there would make the fault's reach depend on it. A process
that died fails every later call it makes.

**A process crash must not drop writes while the process still runs.** The first version of this
model decided "never issued" at write time, so the live process read its own unwritten pages back
as stale and reported `XX001`: a modeling artifact, not an engine bug. Unissued writes now vanish
only at death.

### 6.4 What the model assumes

The simulator is only as adversarial as this list is pessimistic. Each assumption is deliberate and
documented so a reader knows what a green sweep does **not** prove:

- A successful barrier makes every prior write durable. There is no lying `fsync`, which matches the
  host contract jed already states.
- Writes to distinct offsets are independent. There are no misdirected writes (a page landing at
  the wrong offset).
- A torn page is a per-sector mix of old and new bytes at the declared sector size. The default of
  64 bytes keeps tearing possible at the smallest page size (256) and is more adversarial than real
  512-byte or 4 KiB atomic units.
- A process crash preserves the kernel cache and directory. A power loss preserves only durable bytes
  plus whatever subset the model chooses.
- Syncing a file never makes its directory entry durable; only a directory sync does (POSIX). Pending
  directory changes survive a power loss as an ordered prefix, as on a journaling filesystem.
- Reads after an error-free write return that write.

## 7. Scheduling, clock, and entropy

The **generator** is the scheduler. It interleaves sessions in the trace using the same rules as the
stress harness's seeded-sequential interleaver ([concurrency-testing.md](concurrency-testing.md) §6):
a write-open is emitted only while the gate is free in its model, readers open and close freely, and
a reader may stay pinned across many commits. That reader-pinned-across-reuse shape is the one the
watermark exists to protect, and the profiles weight it heavily (§9). The runner executes steps in
order on one thread. That is sound by §2: every real interleaving consistent with the trace's order
produces the same observations.

Each session gets a `SeededRandomSource` seeded with lane 8 of the decision function (§6.2) and a
clock that starts at the `clock` header and advances only on `clock` steps. Nothing in a run reads
wall time or OS entropy.

## 8. Oracles

### 8.1 The generation history

The runner keeps a history of **logical digests** per database — `main` and each file attachment,
each with its own version counter: for each version, the digests of every generation attempted at
it. (A version can have several: after recovery to version 5, a new commit produces a different
version 6.)

- Immediately before each commit, it computes the digest of the in-flight version **inside the write
  session**, which sees its own writes.
- After an acknowledged commit, a fresh read session's digest of the new version must equal that
  in-flight digest: the commit published exactly what its transaction saw. The digest is recorded.
- After a failed commit that wrote at least one page **of a database's file**, the in-flight digest is
  recorded as attempted at that database's next version, since a crash may still recover it.
- A commit that leaves a database's version unchanged must leave its digest unchanged too.

A digest is computed through SQL so every core can produce it identically:
`SELECT * FROM` each of `jed_tables`, `jed_columns`, `jed_indexes`, `jed_constraints`, and
`jed_sequences` ([introspection.md](introspection.md)), then `SELECT * FROM` every table in name
order. Each result's rendered rows are sorted and folded, with the query text, into FNV-1a 64. An
attachment's digest qualifies every relation with its name (`a1.jed_tables`, `a1."t3"`). A freshly
created attachment is version 1 with every relation empty, so its digest is known before it opens:
the fold of each catalog query text followed by an empty row list.

A session pins every attached database's root when it opens (attached-databases.md §5), so the
isolation check compares a closing reader's digest of **each** database it pinned with that
database's history.

**After every recovery, the recovered version must be in the allowed set (§8.2) and its digest must
be one recorded for that version.** This catches torn or mixed snapshots, lost index
updates, wrong row counts, and recovery of a generation that never existed, without a hand-written
model of SQL semantics. The engine's own fault-free behavior is the reference. That is acceptable
here because the target is durability: a semantics bug shared by the faulted and unfaulted paths
belongs to the corpus.

### 8.2 The durability floor

- The **floor** is the newest version known durable: one whose commit or compaction was
  **acknowledged** under `synchronous = on`, or the version published when a step began if the
  database file's own barrier completed in that step with no directory change pending (a
  stabilization sync, for example, makes an adopted generation durable). A compaction's temp file
  syncing proves nothing about the database file, and neither does a file sync while the file's
  directory entry is still pending.
- The **allowed set** after a crash is every version from the floor through the newest version whose
  commit was **attempted**. Under a relaxed `synchronous` mode, the floor is the last explicit
  durability point.
- **Under `synchronous = off`** a barrier proves less. A file growth is a full fsync (hosts.md §2.1),
  so a pending version can become durable by accident and still be overwritten by the next commit of
  its batch, which writes the same slot. So the step-start rule above applies only when the engine
  held nothing pending as the step began, and the acknowledged-commit rule only to a page client's
  commit, which keeps its own barrier. The floor otherwise moves when the engine itself holds nothing
  pending at a step's end (no pending set, no adopted generation awaiting stabilization, no failed
  barrier) and the file's directory entry is durable: it then treats its published version as
  flushed, and the runner holds it to that — after checking the claim against the disk. (A process
  that died between compaction's rename and its directory sync leaves the new file readable with
  nothing pending; until the next opener's first commit syncs the directory (validated-cow.md §3), a
  power loss may still bring the older file back.) A meta-slot write still pending there is a violation of its own
  ("the engine treats version V as flushed, but no barrier covered its meta"). Under the page service
  the owner's state is the one consulted, and after an owner dies a process may recover any version
  from the floor up, since the simulated process crash may drop writes of the step that acknowledged
  a commit (§6.3) — a more pessimistic model than the in-order issue that makes a real process crash
  lose no acknowledged relaxed commit.
- **Under either mode**, a page-service owner that dies may take every version above the floor with
  it, so until a process observes the database again it may see any version from the floor up. A
  generation an owner adopted without stabilizing it (a read-only adoption needs no sync,
  validated-cow.md §3) is above the floor, and a failed stabilization sync before the owner's next
  write may drop part of it from the cache (`io_error op=sync`, §6.3).
- **Recovering below the floor is a durability violation.** Recovering above the latest attempted
  version, or failing with `XX001` outside corruption mode, is a corruption violation.
- Each database has its own floor. An attachment's floor is **0** from the moment `attach` starts
  creating its file until `create` returns: until then a crash may lose the file, and the runner
  forgets the attachment. Once `create` returns (it syncs the directory), the floor is 1 and the file
  must survive every crash.

### 8.3 Structural invariants

Two per-core, test-only checkers run at every `check`; the image checker also runs after every
recovery, and the live checker after every commit.

The **image checker** opens a copy of the disk the way a new process would, so meta selection and
manifest validation (validated-cow.md §1) run first, and then walks the selected generation:

- the image holds the version the runner expects;
- the live page count at meta offset 56 equals the pages reachable from the catalog root, and every
  reachable page lies in `[2, page_count)`;
- the free list has no duplicates and is disjoint from the reachable set and from the selected
  generation's manifest dependencies;
- each table's storage keys ascend strictly and equal the primary-key encoding of their rows;
- each table's persisted row count equals its leaf total;
- each column B-tree index holds exactly the entries derived from its table's rows.
- no overflow page is named by two stored values: a rewritten record may keep an unchanged value's
  chain ([large-values.md](large-values.md) §15), but a chain belongs to one record per snapshot.

The **live checker** checks the running process instead: every page its committed in-memory trees
reference (tree nodes, and the overflow chains reached through resident rows as well as faulted
leaves) lies below the high-water and off the in-memory free list the next commit allocates from,
and no overflow page is reached through two committed rows.
It exists because the image checker asks the engine's own reachability walk what is live and
re-derives everything from disk, so it cannot see a resident node whose references went stale. The
stale-root bug in §14 passed the image checker and was caught only when its corruption surfaced; the
live checker flags it at the commit that creates it.

Separator bounds and chain decoding are implied by the digest, which reads every row, but are not
checked structurally.

**Bounded growth** is checked in a reader-aware form after every commit that publishes a new version
of a database. The engine reclaims only when no reader is pinned behind the new version
(transactions.md §8), so a workload that holds a reader legitimately grows until the readers close;
only a **quiet** commit — no other session open, a single process, no armed fault — is held to a
bound. A file commit reclaims in-commit once its pages off the free list (`page_count − free`) pass
`max(compact_min_pages, compact_growth × L)`, where `L` is the live count at its last reclamation
(memory.md §8.4, format.md *Reclamation*). A commit only adds pages off the list, so the count it
began with is a lower bound on the one the engine tests. The
runner cannot see `L`, so it tracks an upper bound per process lifetime: at an open (or recovery,
compaction, attach), the page count minus the free list, which is what the engine seeds `L` with
(format.md *Reclamation*); after each commit, the maximum of that and its live pages plus the pages
it wrote. Then:

- a quiet commit that **began** with more pages off the list than that threshold must have reclaimed: every page below its
  high-water is a meta page, live, on the free list, or written by the commit itself (its manifest
  and free-list pages) — `page_count − 2 − live − free ≤ written`;
- the next commit, if quiet too, reuses that free list before it grows the file: its high-water grows
  by at most `max(0, written − free)`.

`written` counts the distinct pages of the database's file (other than the two meta pages) the step
wrote; `free` is the in-memory free list the next commit allocates from. A commit that begins inside a
relaxed batch is exempt from both assertions: it reuses only pages free at the floor and keeps the
batch's pages off the free list by design (validated-cow.md §4). Only its bounds carry forward.

**Trailing truncation** (format.md *Trailing truncation*) is checked exactly after every quiet
commit, since a quiet commit always passes the reuse gate. Let `P₀` be the high-water before the
commit, `k` the start of the free list's trailing run below it (`k = P₀` when page `P₀ − 1` is not
free), and `w` one more than the highest page index the step wrote. The commit's high-water must be
exactly `max(k, w)`: the tree pages take the lowest free pages, so whatever of the run they leave is
truncated, and every page drawn after that comes from the high-water at `k`. This one equation
also pins ordinary growth (with no trailing run it reads `max(P₀, w)`). After a commit that lowered
the high-water to `P`, the file must be at most `P + clamp(P, floor, cap)` pages long (pager.md
§7), so the shrink happened and kept no more than one growth step.

Under `synchronous = off` (validated-cow.md §4) the same equation holds for a commit that begins
with nothing pending. A commit that begins with a non-empty pending set never lowers the high-water,
so its high-water must be exactly `max(P₀, w)`. Its shrink is deferred, so the length bound is checked
instead when the engine next holds nothing pending (after the batch cap's flush or a clean close),
against the newest generation's high-water. And after every step the file must be at least as long
as the durable floor's `page_count`: while a batch is pending the floor is the one generation recovery
is sure to find, and a shrink that cut its pages would leave it unusable. A clean close (a `reopen`) flushes the
batch, so when it leaves every byte of the file and its directory entry durable, the floor first
becomes the published version before the file is held to it. A generation a recovered process
adopted and never had to stabilize (it wrote nothing) may still be pending, and a database whose
owner died may have lost what the runner last saw published; either keeps its floor.

**Backups** (page-service.md §9) are checked at the step that takes them, as a new process would
open the file: a failed backup leaves no destination; a successful one opens, its contents are a
generation the history records at the version it copied (its own `txid` for the page-identical
form, one below for the compacted one), that version is no older than the newest committed, and the
image checker accepts it. Its transcript line carries the image hash, so the cores' backups of one
history are byte-identical, and `--check-images` decodes it with `spec/fileformat/verify.rb` too. A
fault on a backup step may cut its writes (the temp file's sync, the directory's) or, in a service
trace, end the connection its page reads use. Whether a completed backup survives a later power
loss is not checked: like `create`, it is durable only once its directory barrier completes.

**Two more violations** need no checker: any statement or commit that reports `XX001` (corruption
must never surface outside corruption mode), and **snapshot isolation** — when a reader closes, its
digest must be one recorded for the version it pinned, however many commits, reclamations, and page
reuses happened in between.

### 8.4 Cross-core agreement

When a trace runs on several cores, the driver requires identical transcripts. A transcript is one
JSON object per line, keys in exactly this order, no whitespace, hashes as 16 lowercase hex digits:

```
{"i":N,"op":"open","sid":S,"version":V}                      V: the session's base version
{"i":N,"op":"on","sid":S,"rows":R,"affected":A,"hash":H,"cost":C}   A: -1 when not a DML count
{"i":N,"op":"on","sid":S,"error":"23505"}
{"i":N,"op":"commit","sid":S,"version":V}   or   …,"error":CODE}
{"i":N,"op":"rollback","sid":S}   {"i":N,"op":"close","sid":S}
{"i":N,"op":OP,"sid":S,"skip":true}                          the session no longer exists
{"i":N,"op":"fault","kind":K}   {"i":N,"op":"clock"}
{"i":N,"op":"check","version":V,"digest":D,"live":L,"pages":P,"free":F,"image":I}
{"i":N,"op":"reopen","version":V,"digest":D}
{"i":N,"op":"crash","fired":X,"recovered":V,"digest":D,"live":L,"pages":P,"free":F,"image":I}
{"i":N,"op":"compact","version":V}   {"i":N,"op":"compact","db":"a1","version":V}   or   …,"error":CODE}
{"i":N,"op":"attach","db":"a1","version":V}   or   {"i":N,"op":"attach","db":"a1","error":CODE}
{"i":N,"op":"corrupt","flipped":F,"open":"ok"|CODE,"read":"ok"|CODE|"-"}
```

While file attachments are tracked, `open`, `commit` (success), `check`, `reopen`, and `crash` lines
gain a trailing `"dbs"` object keyed by attachment name in byte order: `{"a1":V}` (the version each
session pinned, or each database's version after the commit) on `open` and `commit`, and on the others
an object of the same fields the line carries for `main` (`{"a1":{"version":V,"digest":D,…}}`, with
`recovered` in place of `version` on `crash`). A trace without attachments keeps exactly the shape
above. With `JED_SIM_IMAGES`, an attachment's recovered image is written beside main's as
`<trace>.<core>.step<N>.<name>.img`, and the independent checker decodes it too.

`hash` folds the statement's rendered rows, sorted (row order is not what this harness tests), with
FNV-1a 64; `fired` is `barrier`, `write`, `sync`, `read`, `enospc`, `spill`, `end` (the fault reached
no fault point and fired as the step ended), or `none` (a standalone `crash` step). The
`live`/`pages`/`free` counts come from the meta page and persisted free list, which are byte-exact
across cores. `image` is the FNV-1a 64 of the **logical image** — the first `pages × page_size`
bytes, ignoring preallocated tails — of the disk at a `check`, or of the post-crash disk recovery
opened. Equal transcripts therefore mean **byte-identical crash images** across cores, which the
content-keyed fault decisions (§6.2) make possible for the faults marked "image" in §6.3.

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

`scripts/sim_gen.rb` keeps a model of the schema, open sessions, gate holder, and pinned readers.
With that model it emits mostly valid statements, plus a configurable small fraction of invalid ones
(`42P01`, `22012`, `23502`), which abort the write block so commit-of-an-aborted-block runs too.
Inserts take fresh ids and unique indexes cover only id-derived columns, so constraint errors occur
only when intended. After a faulted commit the model cannot know whether the commit landed, so every
table the transaction changed becomes *maybe*: its next use re-establishes it idempotently
(`CREATE TABLE IF NOT EXISTS`, `ADD COLUMN IF NOT EXISTS`, `CREATE INDEX IF NOT EXISTS`).

With attachments, the model keys an attachment's tables by qualified name (`a1.t3`) and tracks each
attachment as attached or *maybe* (its `attach` step crashed, so its file may not exist). A write
transaction writes one durable database: its first statement that names a table fixes it, so no
transaction trips the one-durable-writer `0A000`. An attachment takes no `DROP` (the grammar takes no
qualifier there), no `ON CONFLICT`, and no sequence (attached-databases.md §8), and `attach` waits for
no writer, since attaching takes the writer gate. Temp tables live in the write session only and die
with it. A spill fault arms only a statement that can spill under the drawn knobs: a sorting or
grouping read under a small `work_mem`, a temp-table statement under a small `temp_buffers`.

Fault parameters are drawn from per-profile permille ranges, except that a quarter of the draws take
the extreme (every write lands or is issued; nothing torn), because a complete in-flight generation
is what a random subset rarely produces. `chain` is the fault chance for the first commit after a
crash, whose barrier stabilizes the recovered generation.

Weights live in profiles (`spec/conformance/sim/profiles/*.toml`, merged over the generator's
defaults, which `mixed.toml` restates):

| Profile | Emphasis |
|---|---|
| `mixed` | The baseline: every action, value shape, and fault kind |
| `reuse-under-pins` | Readers held across many small commits, so reclamation and reuse run around pinned snapshots |
| `large-values` | Overflow chains, LZ4-compressed records, `UPDATE`s that grow and shrink values, and `update_narrow` `UPDATE`s that assign only a fixed-width column, so the rewrite keeps every chain (large-values.md §15); half the traces relaxed |
| `big-commits` | Transactions dirtying hundreds of pages: manifest overflow, free-list/manifest planning retries |
| `ddl-churn` | `CREATE`/`DROP`/`ALTER` of tables and indexes, `REINDEX`, sequences, catalog-chain growth |
| `crash-storm` | Repeated process and power crashes |
| `stabilization` | Process crashes that leave complete unsynced generations, then a faulted next commit |
| `resource` | Tiny caches, spill, storage and query-memory limits, `read_error`, `enospc` |
| `compaction` | Host compaction with crashes at the temp-file and directory barriers (`nth`) |
| `spill` | Sort, aggregate, and temp-table spill under small `work_mem` and `temp_buffers`, with `spill_error` |
| `attachments` | File attachments created under crashes, per-database recovery, writes to one durable database per transaction; half the traces relaxed |
| `growth` | Short-lived readers between long runs of quiet commits, for the bounded-growth check (§8.3) |
| `corruption` | `corrupt` steps, with and without `reseal` |
| `service` | An owner and page clients: spawns, exits, kills, lost connections, client commits, promotion (S8); half the traces relaxed |
| `relaxed` | `synchronous = off`: batched commits, cap flushes, crashes and faulted commits mid-batch and at the flush (S9) |
| `truncation` | Bulk inserts, heavy deletes and drops, then long quiet runs, so commits lower the high-water and shorten the file (S10); half the traces relaxed |
| `service-attachments` | Owners and page clients of main and of up to two file attachments: attach in every process, client commits of attachments, per-database promotion and recovery (S11); half the traces relaxed |

A profile changes what is emitted, never what is checked: every oracle runs on every trace.

## 10. Shrinking and the regression corpus

A failing trace shrinks with ddmin applied to **steps** (`scripts/sim.rb shrink`). Removing an
`open` leaves that session's later steps skipped, which the runner tolerates. The failure predicate
is "the same core fails with the same violation class": the message with its step location, numbers,
and hashes masked. The 307-step trace that found the stale-root bug shrank to 11 steps in 198 runs
and about a second. The minimized trace is committed under `spec/conformance/sim/regress/` with a
comment naming the bug.

Regression traces carry **no pinned transcript**. They must pass every oracle on every core and agree
across cores. A pinned transcript would break on unrelated changes, such as a new SQLSTATE, and the
oracles are what the test means.

## 11. Placement, tasks, and CI

- **Generator and driver:** `scripts/sim_gen.rb` and `scripts/sim.rb`, Ruby per CLAUDE.md §10.
- **Runners:** one per core, test-only, reading many traces per process so startup cost is
  amortized. A runner needs to open an engine over a caller-supplied `BlockStore`. Go runs it as an
  internal test compiled once (`go test -c`), as the existing crash tests do:
  `impl/go/sim_{disk,runner,check}_test.go`. Rust's runner is `impl/rust/src/sim/`, a library module
  behind the non-default `sim` cargo feature (run by `src/bin/sim.rs`), so it reaches crate-internal
  state through a few feature-gated accessors. TypeScript's is `impl/ts/src/sim/` with
  `src/bin/sim.ts`, imported by nothing the library or browser build loads. A runner reads
  `JED_SIM_IN` (a trace, a directory, or a list file) and writes transcripts under `JED_SIM_OUT`.
- **Seams:** the file host (`create`, compaction, attachment opens) and the spill host (spill and
  temp-table spill scratch files) are internal per-core interfaces whose production form is the OS;
  only the simulator substitutes them. They change no behavior, bytes, or public API.
- **Tasks:**
  - `mise run sim:sweep [seeds] [profile] [cores]` runs a seed sweep. `mise run ci` runs a fixed
    eight-seed sweep of every profile on all three cores (about 15 seconds); longer sweeps are
    bench-family, like `stress`.
  - `mise run sim:regress [cores]` runs the regression corpus and the independent image checker on
    every crash image it produces; it is in `mise run ci`.
  - `mise run sim:replay <trace> [cores]` and `mise run sim:shrink <trace> [core]`.
  - `mise run sim:plant [seeds]` re-runs the planted-bug check (§14), an analysis tool like mutation
    testing.
- **Failure artifacts:** results land in `bench/results/sim/<stamp>/`: every trace, each core's
  transcript, the crash image a failed recovery opened, a panic's stack, and, for a commit-contents
  violation, the digest text before and after.

## 12. Relationship to existing harnesses

| Existing | Status alongside the simulator |
|---|---|
| Per-core exhaustive crash tests | **Keep.** They enumerate every subset of small commits, an exhaustiveness random search cannot promise. The simulator covers the long histories they cannot express. |
| `spec/fileformat/verify.rb` | **Extended in slice S6** with `--check-image`, an independent fourth voice for §8.3 and for which generation recovery should select. |
| Concurrency Layers 1–3 | **Keep.** The race-detector modes test real threads. The simulator tests faults over logical interleavings. |
| Process corpus (Layer 4) | **Keep, release-blocking.** Real kernel lock semantics and real process death cannot be simulated. S8 explores the page service's state machine randomly; Layer 4 stays the authority for real sockets, kernel locks, and process death. |
| Durability benchmarks | **Keep.** Only real hardware says whether the barrier contract holds on a real device (§6.4). |

## 13. The obligation to grow it

Like the NoREC obligation (CLAUDE.md §10), the simulator explores only what its generator emits and
checks only what its checker knows. **When a change adds a page type, a commit path, a host
operation, or a storage setting, add its steps and faults to the generator and its invariants to the
checker in the same change.** A new seam a fault could target (a spill sink, a file factory, a lock
primitive) gets a simulated implementation.

## 14. Slices

1. **S1 — Foundation, Go runner. Landed.** The trace format, `sim_gen.rb` with sessions, DML/DDL,
   large values, and small pages, the `SimDisk` with `power_loss`, `process_crash`, and `io_error`,
   the clock and entropy injection, the history, floor, isolation, and corruption oracles, the image
   and live integrity checkers, self-determinism (§8.5), shrinking, and the mise tasks.
   **Exit criterion: the simulator must catch bugs planted on purpose. Met.** `scripts/sim_plant.rb`
   removes one protection at a time from the Go core and sweeps the profile built to reach it
   (60 seeds each; the digest plant sweeps all profiles):

   | Planted bug | Profile | Traces that caught it |
   |---|---|---|
   | skip recovered-generation stabilization | `stabilization` | 36 / 60 |
   | let the free list include the commit's own manifest pages | `big-commits` | 26 / 60 |
   | skip the dependency digest check | all | 10 / 420 |
   | ignore pinned readers when reclaiming and reusing pages | `reuse-under-pins` | 60 / 60 |

   Two lessons came from making this pass. The reclaim and reuse gates are **redundant defenses**:
   disabling either alone is masked by the other (a free list that cannot be reused just grows), so
   the plant removes both. And a plant no trace catches usually means a workload gap, not an oracle
   gap: stabilization was caught in 4 of 600 general traces until the `stabilization` profile and
   the `chain` knob targeted it.

   **Engine bugs found.** The first sweeps found two real bugs, both now fixed with corpus entries
   and regression traces:
   - *DROP TABLE kept the table's ANALYZE facts* (Go and TS; Rust was correct), so the next catalog
     write crashed on the missing table. Found on the first sweep.
   - *A resident root leaf kept references to overflow chains its own commit had freed*, in **all
     three cores**, so the cross-core comparison could not see it. After a reopen, or after a
     multi-leaf table shrank back to one leaf, a commit rewrote the leaf's spilled values into fresh
     chains; the next commit that reused the freed pages corrupted the table. No fault was needed —
     the simulator's reopen and long histories reached it.
2. **S2 — Rust and TS runners, cross-core tier. Landed.** Both runners reproduce Go's transcripts
   byte for byte on every profile; a TS sweep of 700 traces also produced 22,643 crash images
   byte-identical to Go's. The decision function is pinned in `prng.toml` ([[sim_decide]]) and
   recomputed by `prng_verify.rb`. `sim:regress` and a fixed sweep run in `mise run ci`. Each port
   also caught the planted stabilization and watermark bugs in its own engine.
3. **S3 — Resource knobs. Landed.** The `resource` profile: tiny caches force buffer-pool eviction,
   small `work_mem` spills the generator's sorting and grouping reads, small `temp_buffers` spills
   session temp tables, and storage and query-memory limits fail commits and statements
   mid-history. `read_error` and `enospc` are per-core faults (§6.3). Every core's spill goes through
   a spill-host seam (Go's `spillhost.go` and Rust's `spillhost.rs` mirror TypeScript's `SpillSink`):
   sort runs, spools, hash partitions, and a spilled temp domain's scratch store. The simulated host
   keeps scratch files in memory, and `spill_error` fails their operations; the `spill` profile
   arms it on statements that do spill.
4. **S4 — Simulated filesystem. Landed.** Every core's compaction, `create`, and file-attachment open
   go through a file-host seam (`filehost.{go,rs,ts}`); the simulated filesystem implements it, with
   named files, ordered directory changes, and a directory barrier where a rename becomes durable
   only after its directory sync. The runner creates the database through it, the `compaction`
   profile crashes at either of compaction's barriers (`nth`), and the `attachments` profile creates
   file attachments under crashes at `create`'s barriers, writes them one durable database per
   transaction, compacts them, and checks every database's recovery against its own history and
   floor (§8.1–8.2).

   **Engine bug found.** A process that died between a compaction's rename and its directory sync
   left the new file in place with an unsynced entry; the next opener adopted it and committed with
   file syncs only, and a power loss reverted the rename and lost those acknowledged commits — 8 of
   80 `compaction` seeds. The first durable commit after each open now syncs the directory
   (validated-cow.md §3). The same sweep also corrected the floor rule (§8.2), which had trusted a
   compaction's temp-file sync.

5. **S5 — Simulated lock host and multiple processes. Retired with shared mode.** S5 simulated
   shared mode: processes joining, leaving, and dying around one file under the shared lock protocol
   (the `shared` profile, `# processes: shared` traces, and the `ignore-co-residents` plant). The page
   service replaced shared mode ([locking.md](locking.md) §1), so the profile and plant were removed
   and S8 replaced the multi-process half with an owner and its page clients.
6. **S6 — Independent checker. Landed.** `verify.rb --check-image` decodes a crash image as a fourth
   voice: it selects the generation recovery must select and checks the live count and free list.
   The driver compares its decision with every recovery a core reports (`sim:regress` always,
   sweeps with `--check-images`); in testing it agreed on every one of tens of thousands of images.
7. **S7 — Bounded growth and corruption. Landed.** The reader-aware bounded-growth check (§8.3), with
   the `growth` profile built to reach it, and the `corrupt` step (§6.3), with the `corruption`
   profile. Each is held to a plant (`scripts/sim_plant.rb`, 60 seeds of its profile):

   | Planted bug | Profile | Traces that caught it |
   |---|---|---|
   | a closed reader never releases its pin | `growth` | 59 / 60 |
   | a commit never reuses the free list | `growth` | 60 / 60 |
   | an attachment's commit ignores pinned readers | `attachments` | 50 / 60 |
   | a page whose checksum does not match is decoded anyway | `corruption` | 43 / 60 |

   No engine bug surfaced in any core: a flipped checksum surfaced only as `XX001` or not at all, and
   the CRC-valid malformed pages `reseal` produces failed closed (the decoder hardening that landed
   just before, storage.md §6). One gap remains: the generated reads never spill an `ORDER BY`
   sorter run (only spools, hash partitions, and temp stores spill), so `spill_error` does not yet
   reach the `sort`/`merge` scratch files; the unit and forced-spill corpus runs still cover them.

8. **S8 — An owner and its page clients. Landed.** A trace with `# processes: service` opens every
   process under the page service ([page-service.md](page-service.md)) through the simulated lock
   host, which also holds the page service's owner record and endpoints, and a simulated **network**
   of in-memory stream connections. The first process is the owner; later ones are page clients,
   whose pages and commits pass through the owner's real connection code.
   - **Determinism.** The owner's and clients' connection goroutines and threads still run, so after
     every step the runner waits until the network is **quiescent**: every byte written has been read,
     every reader is blocked waiting for more or has closed its end after handling end of stream, and
     every pending connection has been accepted. Within a step, a write returns only once the peer
     has read it and waits for more, so each connection's frames and their effects happen in one order.
     A lost connection marks end of stream on both ends rather than closing either, so both readers
     handle it before the network settles. So an asynchronous frame (`end_read`, `abort`), an end
     of stream, or a maintenance commit that follows an acknowledgement is always complete before the
     next step. A client never re-elects in the background: it re-elects at its next transaction
     begin, in trace order. Owner timers are off (the idle timeouts are 0), the shutdown grace is
     1000 ms of simulated time, and every wait (an election, a re-election, a drain) polls through the
     lock host, advancing the simulated clock. Incarnation ids are random but never reach a
     transcript. Each core writes each frame with one write, so frame counts agree across cores.
   - **Faults.** Only the owner touches the file, so a disk fault (`process_crash`, `io_error`)
     armed on any step lands in the owner's I/O, and the faulted step kills the **owner**, whichever
     process's step drove the I/O; a power loss kills every process. `kill p` closes the process's
     listener and connections, as the kernel closes a dead process's sockets, and releases its locks.
     `disconnect p` ends a client's connection; the armed `fault disconnect nth=N` ends the connection
     carrying the step's `N`th frame (in either direction) instead of delivering it, which lands
     before or after a commit's point of no return, so a client resolves a lost acknowledgement by
     re-election: committed, not committed (`08006`), or unknown (`08007`). A disconnect kills no
     process.
   - **Oracles.** The storage oracles are unchanged. A read transaction open across a change of its
     process's role may fail `08006` (README.md §6.5) — it must never see other contents. An owner's
     **maintenance commit** after a client's commit (page-service.md §6) joins the history at the next
     versions with the same contents; when the owner dies later in the step that acknowledged a client's
     commit, the commit is checked by the recovery that follows (nothing can be read back through the
     dead owner), and a half-written maintenance commit at the next version is an allowed outcome. The **commit-role rule**: a page client's commit never rebuilds
     the free list, so the free list it persists is a subset of its base's. Each commit's transcript
     line names its role. The live checker runs on the owner (a client reads pages only under a
     pinned transaction), and the bounded-growth check stays single-process.
   - In the first 20 `service` traces on Go: 360 commits, 98 kills, 55 connection drops, 22 owners
     killed by a fault mid-step, 16 readers that lost their snapshot with `08006`, and four commits
     whose outcome stayed unknown (`08007`), every one cross-checked by the oracles. The
     `client-reclaims` plant (a client's commit reclaims and reuses as if it owned the file) is caught
     in 10 of 40 traces. The regression trace `service-owner-and-clients.sim` keeps one such history.

9. **S9 — Batched durability. Landed.** The `synchronous` header knob, the `relaxed` profile, and
   relaxed halves of `service` and `attachments`. The runners open every process relaxed with the cap
   flush inline; the oracles gain the engine-aligned floor rules of §8.2 (including the check that an
   engine claiming a flushed version really put a barrier behind its meta) and the batch exemption of
   §8.3. Two plants (`scripts/sim_plant.rb`): a pending commit that writes slot `txid & 1`
   (`relaxed-parity-slot`) and one that reuses pages freed during its batch (`relaxed-reuse`). The third rule, the
   cumulative manifest, is hard to reach here: a batch often outgrows the pages its floor had free, so
   the file grows and each growth's full fsync makes the batch durable early. Only one or two in a hundred `relaxed` traces catch
   a plant that drops it, so each core carries a deterministic unit test instead — a crash that lands
   only a batch's second commit, whose meta must be rejected.

10. **S10 — Trailing truncation. Landed.** The exact high-water check after every quiet commit and the
    file-length bound after a commit that lowered it (§8.3), the shrinking `set_size` of the simulated
    disk (§6.1), and the `truncation` profile, whose deletes, drops, and long quiet runs make
    reclamation list the dead top of a file and the next commit cut it off. The service model's
    maintenance commit now also follows a client commit that leaves the free list ending at the
    high-water. Each protection is held to a plant (60 seeds of its profile):

    | Planted bug | Profile | Traces that caught it |
    |---|---|---|
    | a commit never truncates | `truncation` | 58 / 60 |
    | truncation also takes the commit's own orphans, still the fallback's pages | `truncation` | 58 / 60 |

    A plant that truncated pages despite the reuse gate was dropped: in one process every reader pins a
    version at or after the free list's generation (a rebuild waits for older readers), so it changed
    nothing a trace could observe. No engine bug surfaced.

    Rebased onto batched durability (S9), half the `truncation` traces run relaxed, and the oracles add
    the relaxed rules of §8.3: no lowering inside a pending batch, the deferred shrink checked at the
    flush, and the file never shorter than the durable floor's high-water. Two more plants:

    | Planted bug | Profile | Traces that caught it |
    |---|---|---|
    | a relaxed commit shrinks the file before its batch's flush | `truncation` | 8 / 60 |
    | the flush that ends a batch never shrinks a file the batch lowered | `truncation` | 25 / 60 |

11. **S11 — File attachments under the page service. Landed.** The `service-attachments` profile puts
    `attach` in `# processes: service` traces ([spec/pageservice/README.md](../pageservice/README.md)
    §6.6). An `attach` step attaches the file in every running process: main's owner first, so it owns
    the file and the others are page clients of it; a spawned process attaches every attachment whose
    file exists. Each database's bundle locks, owner record, and endpoint are its own in the simulated
    lock host.
    - **Faults kill every owner.** The simulated filesystem has one death point per step: once a fault
      fires, every later I/O of the step fails, whichever process issues it. Only owners touch files, so
      a faulted step kills every process that owns main or an attachment (all of them, as a single
      process's crash, when that is every process). Ownership usually coincides — main's owner attaches
      first, and each begin re-elects main and every attachment together — but a faulted attach or a
      promotion can split it, and the rule holds either way. Cross-owner gate order is the process
      corpus's (`attachment_gate_order`).
    - **Oracles.** Every database is observed like main under S8: what a process sees of an attachment
      may be newer than the runner last saw, never older, and is a recorded generation. A client's
      attachment commit obeys the commit-role rule and may be followed by the owner's maintenance
      commit, which joins that attachment's history. A process that is not attached to a database (a
      faulted attach killed the attacher first) is skipped by the checks of that database until the
      next `attach`.
    - The regression trace `service-attachments-owner-and-clients.sim` keeps one history with client
      commits of attachments, an attachment maintenance commit, and owner kills.
    - The sweeps surfaced one S8 oracle gap, independent of attachments: an armed `disconnect` can
      land in a commit's read-back itself, which then fails `08006` like any read open across a lost
      connection; the read-back now retries once with a fresh read, which re-elects.
   - **Batched durability and trailing truncation** (S9, S10). An attachment follows its handle's
     `synchronous` setting, as a process's main does, and half the traces run relaxed. Each database is
     its own file domain: its owner flushes its own batch before granting its gate to a client, and
     only its owner's own commits truncate it. The floor rules apply per database — a page client's
     commit of an attachment sets that attachment's floor, and a dead process loses the batch of each
     database it owned — and the durability checks read each database's pending state from its own
     owner.

## 15. Open questions

- **Generator throughput.** A Ruby generator keeps the trace language-neutral and authored once. In
  S1 the runners, not the generator, dominate sweep time, so no accelerated generator is needed.
- **The Rust hook's shape.** Is a `sim` feature flag the right boundary, or should "open over a
  host-supplied `BlockStore`" become a real host-API capability? hosts.md already treats hosts as
  pluggable. That decision belongs to the API, not this harness.
- **CI budget.** How many regression traces and how long a sweep slice `mise run ci` can afford.
- **Sector size defaults.** 64-byte tears are the default because they keep tearing possible at
  256-byte pages. Whether to also sweep 512-byte or 4 KiB units, which match real atomic-write
  sizes, is a profile choice.
