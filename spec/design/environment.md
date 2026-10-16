# Environment — one explicit home for the engine's outside inputs (design PROPOSAL)

> ⚠️ **PROPOSAL — NOT A SETTLED SPECIFICATION.** Nothing here is built. The current behavior
> remains in force: process-global loaded collation and timezone data ([collation.md](collation.md)
> §4.2, [timezones.md](timezones.md) §3.3), the `ExtensionRegistry` passed in open/create options
> ([api.md](api.md) §12), and the random and clock sources set on the database handle (api.md §10,
> [entropy.md](entropy.md) §5/§6). Agreed so far: the environment holds every input from outside
> the engine except storage (§2), and it is produced by a builder and immutable once built (§3).
> Remaining open decisions are in §10.

## 1. The problem

**Process-wide state.** Each core keeps three kinds:

1. **Constant lookup tables** built once from compiled-in, generated data (Rust
   `OP_COST_OVERRIDES` in `executor/kernels.rs`; Go's `costs` and estimator tables). They never
   change after initialization. **Not a problem; out of scope.**
2. **The same-file open registry** (Rust `OPEN_PATHS` in `coordinator.rs`, Go `processOpenPaths`).
   OS file locks behave inconsistently within one process (POSIX `fcntl` locks belong to the
   process; `flock` and Windows locks to the open file), so only a process-wide registry can notice
   a second open of the same file in the same process ([locking.md](locking.md) §3, `55006`). It
   is process-wide because the resource it models is. SQLite keeps the same kind of global in
   `os_unix.c`. **Justified; out of scope.**
3. **Loaded Unicode and timezone data**: the collation set and the property/casing table (Rust
   `collation.rs`, `LOADED` and `LOADED_PROPERTY`) and the zone set (`timezone.rs`, `LOADED_TZ`),
   with Go package-level and TS module-level equivalents. Filled by `load_unicode_data` /
   `load_time_zone_data` (Rust `jed::…`, Go `jed.LoadUnicodeData` / `jed.LoadTimeZoneData`, TS
   `loadUnicodeData` / `loadTimeZoneData`) and by the handle methods that forward to them.

The recorded reason for (3) is that a host must be able to load data **before `open`**, because
opening a file that references a collation resolves it against the loaded set, and "`open` mints
the handle, so the data cannot live on the handle" (collation.md §4.2). That shows the data cannot
live on the *database handle*; it does not show it must be *process-wide*. Being process-wide costs:

- **Libraries interfere.** Loading is first-wins. If two libraries in one program use jed and the
  first loads one bundle, the second's different bundle is silently ignored for every name the
  first already provided, and the second library's databases are judged against the first's data.
- **No replacement or unloading** without restarting the process.
- **Tests cannot isolate.** The loaded set leaks between tests in one process.
- **A hidden input.** Which files open, and each collation's version verdict (collation.md §12),
  depend on what unrelated code in the process loaded. Results stay deterministic *given* the
  loaded set, but the set itself is ambient.
- **Multiple linked versions.** A tool linking several jed releases into one binary (for example,
  one that upgrades files from older format versions) must load data into each release's globals
  separately.

**Scattered inputs.** The engine's other outside inputs each arrive a different way: host
functions as an `ExtensionRegistry` in the open/create options, frozen for the handle's lifetime;
entropy and time as sources set and cleared on the handle at any time. A host that opens many
databases supplies each of them separately, per database, and nothing ensures it supplies the same
ones.

## 2. The line

**The environment holds every input the engine takes from outside itself, except storage.** The
database handle and its sessions keep policy and resource settings.

| Input | Today | Proposed |
|---|---|---|
| Unicode data (collations, casing) | process-global | environment |
| Timezone data | process-global | environment |
| Host functions | `ExtensionRegistry`, per handle | environment |
| Host types (not built) | — | environment |
| Random source | handle setter | environment |
| Clock source | handle setter | environment |
| Storage (`BlockStore`, path) | per database | per database (it *is* the database) |
| `max_cost`, `work_mem`, memory and storage limits, cache sizes, `synchronous`, locking mode | handle / session | unchanged |
| Session envelope (privileges, `allow_ddl`, budgets) | session | unchanged |

Two observations support this line:

- **The first four are what a file depends on.** A file records collations, timezone data, and
  host functions by name and version, and checks them against what the host supplies (collation.md
  §12; [index-dependencies.md](index-dependencies.md); [extensibility.md](extensibility.md) §11).
  A host type's codec will be needed just to decode a file. Together they are everything a
  file can depend on from outside, so "can this host open this file?" becomes "does this
  environment satisfy the file's recorded dependencies?"
- **Entropy and time are outside inputs too.** CLAUDE.md §10 names the entropy/clock seam as one of
  the engine's "host supplies it" boundaries, alongside storage and cost. Host functions, already in
  the environment, can read the clock or the network themselves, so "data that interprets a file"
  and "services" are not a clean split. "From outside the engine" is.

The settings that stay on the handle are not outside inputs; they are the host's policy for one
database (how much work, memory, durability) and do not change what a value means.

## 3. The `Environment`

```
// Rust (Go and TS mirror the shape idiomatically)
let env: jed::Environment = jed::Environment::builder()
    .load_unicode_data(&bundle_bytes)?
    .load_time_zone_data(&tz_bytes)?
    .register_function(geo_hash)?               // was ExtensionRegistry
    .random_source(jed::os_random_source())      // the default; shown for completeness
    .clock_source(jed::wall_clock())             // the default
    .build();

let db = jed::Database::open_with(path, OpenOptions { environment: env.clone(), .. })?;
let mem = jed::Database::create(CreateOptions { environment: env.clone(), .. })?;
```

- **A builder produces it; an `Environment` is immutable.** All loads, registrations, and source
  choices happen on an `EnvironmentBuilder`; `build()` returns an `Environment` with no mutating
  methods at all, so "frozen" is a fact of the type, not a runtime check, and needs no error code.
  Validation (a malformed bundle is `XX001`, a conflicting function registration `42723`) happens
  as each builder call is made, as it does today. A database's outside inputs therefore never
  change while it is open. This matches how the `ExtensionRegistry` already behaves, and replaces
  today's additive loading, under which a load after `open` could turn a `42704` into success in
  the middle of a session. Hosts load at startup in practice. Go and TS follow the same shape with
  their idioms (a builder struct with a `Build()` method; a builder class with `build()`); the
  exact spelling is the implementing slice's.
- **Changing inputs means a new environment and reopening** the databases that should see them.
- **Shared by handle, not by process.** A host that wants one set for everything creates one
  environment and passes it to every `open`/`create`. A library, a test, or an upgrade tool creates
  its own and stays isolated.
- **One environment per handle.** Attached databases ([attached-databases.md](attached-databases.md))
  and temp domains use their handle's environment, so a spanning query compares under one loaded
  set and reads one clock.
- **Cheap to derive.** An environment's contents are immutable, so environments can share them.
  `env.to_builder()` returns a builder pre-filled with the environment's contents, by reference, not
  copy; changing only the clock and calling `build()` yields a second environment that shares
  everything else. This is how a host gives one database different inputs: a separate environment,
  never a handle-level override with rules about which one wins. Whether a derived builder may only
  *add* (more bundles, more functions) and *replace sources*, or may also remove, is open (§10).
- **Shared ownership and thread safety.** The environment is reference-counted (Rust `Arc` inside,
  Go pointer, TS object); a handle keeps it alive. Frozen, it needs no locking for reads.

## 4. Entropy and time

**The environment fixes *which* source is installed, not the source's internal state.** A source
is an object; the host may keep a reference to it and control it. This keeps the immutable-environment
guarantee (the set of outside inputs never changes while a database is open) while letting tests
control time and randomness.

| Source | Default? | Control |
|---|---|---|
| OS random source | yes | none (fresh OS entropy per value) |
| Seeded random source | | reseedable: `reseed(u64)` restarts the splitmix64 stream (entropy.md §2) |
| Wall clock | yes | none |
| Manual clock | | `set(micros)`, `advance(step)`; replaces `fixed_clock` |
| Advancing clock | | `reset(start, step)`; one increment per read, as today |

- **The conformance harness** installs a seeded source and a manual or advancing clock in the
  environment of the database it runs records against, and implements `# seed:`, `# clock:`, and
  `# clock_advance:` by reseeding or setting them before a record and resetting afterwards. Record
  results are unchanged.
- **Shared stateful sources interleave.** Every database and session using one environment draws
  from its random source in call order, so concurrent use makes each database's draws depend on
  timing. This is not new: today the handle's seeded source is already shared across sessions that
  can run concurrently. The fix is the counter-keyed generator of
  [determinism.md](determinism.md) §7 (each value derived from a seed and a row key), deferred with
  parallelism. A host needing independent streams meanwhile uses separate environments.
- **Determinism** is stated as "deterministic given the environment" instead of "given the handle's
  seam inputs" (CLAUDE.md §10, determinism.md §5).
- **Per-statement behavior is unchanged**: `now()` reads the clock once per statement,
  `clock_timestamp()` per call, and `uuidv7` keeps its per-statement counter (entropy.md §5).

## 5. Host functions and host types

- **The `ExtensionRegistry` is absorbed.** Registration moves onto the environment with unchanged
  semantics (exact signatures, strictness, batch kernels, component identity and semantic version,
  cost declaration — api.md §12, extensibility.md §4.2/§7). It was already frozen per handle; it is
  now frozen per environment.
- **Host types**, when ratified (extensibility.md §4.3), register on the environment too. A file
  using one cannot be decoded without its codec, so it must be present at `open`, the same situation
  as a referenced collation.

## 6. The handle API

- `db.environment()` returns the handle's environment. `db.LoadedCollations()` /
  `db.LoadedTimeZones()` and function introspection read it.
- **Removed:** the handle-level loads (`db.LoadUnicodeData`, `db.LoadTimeZoneData`), the
  engine-level free functions (`jed::load_unicode_data` and the Go/TS equivalents), and the seam
  setters (`set_random_source` / `clear_random_source` / `set_clock_source` /
  `clear_clock_source`). Keeping any of them as a convenience over a hidden default environment
  would recreate the global. Pre-1.0 this is an acceptable break.
- `fixed_clock(i64)` becomes a manual clock set once; `seeded_random_source(u64)` and
  `advancing_clock(start, step)` keep their names and gain the controls of §4.

## 7. The default when no environment is given

**A fresh, empty environment per `open`/`create`**: the built-in collation `C` and ASCII casing,
`UTC` and fixed offsets, no host functions, OS randomness, and the wall clock. Opening a file that
references a collation then fails as it does today when nothing is loaded. A process-wide default
environment would be the global under another name, and every problem in §1 would return for any
host relying on it.

## 8. What does not change

- **File format, SQL, conformance results, cost.** Nothing persisted or observable through SQL
  changes. A file still records its dependencies by name and version.
- **Untrusted-query safety.** Loading, registration, and source installation stay host-API acts,
  never SQL (collation.md §11, api.md §12).
- **Policy and resource settings** stay where they are (§2).
- **Categories 1 and 2 of §1.**

## 9. Scope of the change

All three cores, and every in-repo consumer of the moved APIs: the conformance harnesses (the
`# seed:` / `# clock:` / `# clock_advance:` directives, §4), the simulation runners, the `bench/`
modules, the CLI, `migrate/`, the Node wrapper (`impl/node`), the Ruby gem, the browser bridge
(`web/src/lib/jed/`), and the `/web` API docs and examples. Per-core unit tests cover isolation
(two environments with different bundles and clocks), derivation (`to_builder()` shares contents
and changes only what it is told to), and lifetime (a database outlives the caller's reference to its environment).

Amended on adoption: collation.md §4.2/§11, timezones.md §3.3, entropy.md §5/§6, api.md §10/§12,
extensibility.md §7, determinism.md §5, and the CLAUDE.md §10 wording of the entropy/clock seam.

## 10. Open decisions

1. **The name.** `Environment` is a working name (cf. RocksDB's `Env`). Alternatives: `Runtime`,
   `Host`, `Context`.
2. **The controllable-source API** (§4): method names, and whether the harness needs anything
   beyond reseed/set/advance/reset.
3. **What a derived builder may change** (§3): add bundles and functions and replace sources
   only, or also remove. Removing is rarely needed and makes "derived from" a weaker statement about
   shared contents; add-and-replace is recommended.
