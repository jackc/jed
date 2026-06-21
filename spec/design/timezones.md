# Time zones — design PROPOSAL

> ⚠️ **PROPOSAL — NOT A SETTLED SPECIFICATION.** Unlike the other docs in `spec/design/` (which
> record *ratified* decisions the cores implement in lockstep), this is an **unratified design
> proposal** captured from an in-progress discussion so the idea is not lost. **Nothing in it is
> decided, contracted, or built.** It binds no core, it is not part of the conformance contract,
> and every part of it is open to revision or rejection. The only *settled* time-zone facts today
> are the ones this doc builds on, all already in-tree: `timestamptz` is stored as a UTC `i64`
> instant (the clock-seam micros — [entropy.md](entropy.md) §1; the "i64 instant rule" for keys,
> [encoding.md](encoding.md) §2), the `TimeZone` / `time_zone` session slot exists and defaults to
> `UTC` ([session.md §6.2](session.md)), and the clock functions `now()` / `current_timestamp` /
> `clock_timestamp()` produce `timestamptz` through the clock seam. The *conversion* machinery
> below — `AT TIME ZONE`, `timestamptz`↔`date`/`text` casts in a zone, `date_trunc(…, zone)`, and
> the IANA tz database that powers them — is the **deferred follow-on** ([session.md §6.2](session.md)
> calls it exactly that). Read this as a sketch of how that follow-on *could* be built, not a
> description of how jed works or has agreed to work. The open decisions are in §10; **none of them
> has been made.**
>
> **The two questions this doc answers.** (1) *Where does the time-zone data come from, and how big
> is it?* — proposal: **vendor a pinned IANA tzdata version into each core** (≈100 KB–500 KB), never
> the host OS, for the same determinism reason jed hand-rolled LZ4 rather than linking a library.
> (2) *Does jed inherit the collation index-corruption hazard?* — **for the base type, no** (UTC
> storage makes `timestamptz` ordering tz-independent, so plain indexes are structurally immune); **for
> a functional index that bakes a zone conversion into its key, yes** — and that case is the *same
> problem* [compatibility.md](compatibility.md) already unifies (stored bytes produced by a versioned
> computation), so it registers into that doc's manifest rather than getting a bespoke mechanism. The
> structural version layer is [../fileformat/format.md](../fileformat/format.md); the cross-core
> identity requirement is [determinism.md](determinism.md) (CLAUDE.md §2/§8); the host/legibility
> stance is CLAUDE.md §13; the sibling versioned-reference-data instance is [collation.md](collation.md).
>
> **Status: UNRATIFIED PROPOSAL — nothing here is decided or implemented.**

---

## 1. Scope

Time-zone support adds three things on top of the already-settled `timestamptz` instant type:

1. **An IANA time-zone database** the engine can consult to map a UTC instant ↔ a local wall-clock
   reading for a named zone (`America/New_York`, `Europe/Paris`, the fixed `UTC`/`Etc/*` zones).
2. **Conversion operations** that use it — `AT TIME ZONE`, `timestamptz` → `text` rendering in the
   session zone, `timestamptz`↔`timestamp`/`date` casts, `date_trunc(unit, ts, zone)`, the relevant
   `EXTRACT`/`make_timestamptz` surface (§9).
3. **A session zone** — the `TimeZone` GUC, *already present* ([session.md §6.2](session.md),
   default `UTC`, capability `session.timezone`, corpus directive `# timezone:`), which selects the
   zone for I/O and for the two-argument `STABLE` forms of the above.

What it deliberately does **not** add: any change to how `timestamptz` is *stored* or *compared*.
That is the whole basis of §2.

## 2. The representation — `timestamptz` is UTC, and that buys structural immunity

`timestamptz` is a UTC `i64` instant (microseconds; the clock-seam representation,
[entropy.md §1](entropy.md)). The session `TimeZone` affects **input parsing** and **output
rendering** only — never the stored bytes, never the comparison. This is PostgreSQL's model exactly
([CLAUDE.md §1](../../CLAUDE.md): match PG): a `timestamptz` column stores instants, and "in zone Z"
is an I/O-time interpretation, not a stored property.

The consequence is the single most important fact in this doc, and it is a sharp contrast with
collation:

> **`timestamptz` ordering is integer comparison of UTC micros — completely independent of the tz
> database.** So a plain B-tree index, `ORDER BY ts`, `UNIQUE(ts)`, or `ts` as a primary-key member
> is **structurally immune** to the tz-data-version corruption hazard that §5 describes. A tzdata
> update cannot reorder stored instants, because no tz rule participates in their order.

Collation has no such escape: the comparison function for `text` *is* the versioned thing, so **every**
collated index is exposed when the library reorders. Here the base type's comparison is tz-free, so
**none** of the plain timestamp indexes are exposed. The exposure is confined entirely to keys that are
*derived by applying a zone* — §5.

This also means **time-zone support adds nothing to the on-disk format** — no `format_version` bump.
`timestamptz` is already a UTC `i64`; the tz database lives in the engine, not in the file (with one
caveat for derived keys, §6).

## 3. The time-zone database — vendor a pinned version, never the OS

### 3.1 How big is it?

| Form | Size | What it is |
|---|---|---|
| **Compact source** (`tzdata.zi`, single-file) | **~107 KB** | The authored rules as text. What PostgreSQL vendors in `src/timezone/data/`. |
| **Compiled binary tree** (TZif files, `/usr/share/zoneinfo`) | **~2 MB** | One [RFC 8536](https://datatracker.ietf.org/doc/html/rfc8536) TZif file per zone (`America/New_York` ≈ 3.5 KB), ~454 files. |
| **Embedded-in-binary** (e.g. Go's `time/tzdata`) | **~450 KB** | A compressed zip of the compiled tree, linked in. |

Scale: ~341 zones + ~257 links ≈ 600 named entries. The data changes a few times a year (a new
release like `2025b` whenever a jurisdiction changes its rules) — which makes the *version* a
first-class, trackable artifact (§6).

So "bake it into the binary" costs **~100 KB–500 KB** depending on form — trivial next to an engine.

### 3.2 Why vendor it, not read the host OS

The determinism requirement ([CLAUDE.md §8/§10](../../CLAUDE.md), [determinism.md](determinism.md))
forbids reading the host's `/usr/share/zoneinfo`: two containers with different distro tzdata versions
would compute *different* conversions for the same instant, and the three cores would diverge. **This
is the ICU-collation trap of [types.md §11](types.md) all over again** — and jed already rejects it
once, for collation. So:

> **Proposal: pin one IANA tzdata version, vendor it into every core, and treat the vendored version
> as part of jed's build.** It changes only when jed ships a new core — a discrete, version-stamped,
> cross-core-identical event — never silently under a host upgrade.

This is now the **same stance collation takes.** [collation.md §2/§3](collation.md) pivoted to
**vendor the compiled collation tables into each core and reference them (by name + version) from the
file**, never storing the data in the file — exactly tzdata's vendor-pinned, reference-in-file model
here. The two are the same Tier-2 "versioned reference data" pattern
([compatibility.md §5](compatibility.md)); collation is no longer the baked outlier it once was.

### 3.3 Per-core reader — TZif is standardized, so this is the *easy* case

A pleasant asymmetry with the LZ4 decision ([large-values.md §6](large-values.md), CLAUDE.md §9):

- **LZ4 encoders are not standardized** → a per-core library would produce different bytes → jed
  hand-rolled a byte-pinned codec.
- **TZif is standardized (RFC 8536)** — like AEAD in [encryption.md](encryption.md). Independent
  per-core readers of the *same* TZif bytes agree **by construction**.

So the proposal is to **vendor the *compiled* TZif bytes as a shared, pinned spec fixture**
(`spec/tz/<version>/…`, with `(zone, instant) → (offset, abbrev, dst)` golden vectors like every other
cross-core fixture), and have each core implement a small RFC 8536 reader. No core runs `zic` — which
sidesteps the one real divergence risk (different `zic` versions emit slightly different TZif v1/v2/v3
framing). PostgreSQL vendors its *own* `zic` + reader for the same cross-platform-consistency motive
([its `src/timezone/README`](../../references/postgres/src/timezone/README)); pinning the compiled
bytes is the same idea, one step further, and a better fit for jed's "data over code" discipline
([CLAUDE.md §5](../../CLAUDE.md)).

## 4. Where the tz database is consulted — the read/write split

Mirroring [compatibility.md §4.2](compatibility.md), almost every tz dependency is needed to *write or
derive*, not to *read a stored value*:

| Use | tz data needed to **read** a stored value? | Notes |
|---|---|---|
| `timestamptz` stored value / comparison / plain index | **No** | UTC `i64`; tz-free (§2). The common case. |
| Parse a `timestamptz` literal in the session zone | — (write path) | One-time at INSERT; result is frozen UTC. |
| Render a `timestamptz` as `text` in the session zone | display only | Affects output formatting, never stored bytes or order. |
| `tz`-dependent **expression DEFAULT** ([constraints.md §2](constraints.md)) | **No** | Evaluated once at INSERT, frozen as a stored value; a plain index over that value stays consistent. **Not exposed.** |
| `tz`-dependent **functional index key** (future) | **No** for the base heap-scan; **yes** for index-accelerated lookup | The exposed case — §5. |
| `tz`-dependent **STORED generated column** (future) | **No** — value on disk | Exposed only on recompute/maintenance. |
| `tz`-dependent **VIRTUAL generated column / view** (future) | **Yes** — computed on read | Read-required; the hard wall ([compatibility.md §11](compatibility.md)). |

The takeaway: a stored instant is always readable on any core regardless of tz-version skew (its bytes
encode no zone). What can go stale is a **key or value derived by applying a zone** and then *stored* —
and only when that derivation is later expected to match a fresh evaluation under a different tzdata
version.

## 5. The functional-index stability hazard

This is the question that motivated the doc: *does a functional index on, e.g.,
`(ts AT TIME ZONE 'America/New_York')::date` have the collation index-corruption problem?*

**Yes — that specific shape is the same corruption class.** The index key is a value *derived from the
row by applying tz rules*. A B-tree relies on the invariant `stored_key == f(row)`. A tzdata update can
change `f` for a fixed UTC instant:

- **Future-dated rows** are the common exposure — a government announces it is dropping DST next year;
  re-deriving the same stored instant under the new data yields a different local date.
- **Historical corrections** too — tz releases routinely fix past offsets (especially pre-1970).

When that happens, stored keys no longer match a fresh derivation → index scans miss rows, indexed vs.
heap-scan plans disagree, and a `UNIQUE` functional index can admit logical duplicates. Same *failure*
as collation, via a slightly different *mechanism*: collation flips **the comparison between two stored
keys** (the comparator is the versioned thing); a tz functional index keeps a stable comparator (date
compare) but **stales the key derived from the row** (the derivation is the versioned thing). Detection
and remedy are identical — version-stamp and rebuild.

**This is not a new mechanism.** It is exactly the unification in
[compatibility.md §3](compatibility.md): *stored bytes whose correctness depends on a versioned
computation.* Collation tables and the IANA tzdata version are two instances of one Tier-2
"versioned reference data" dependency ([compatibility.md §5](compatibility.md)). So tz registers into
**that** doc's manifest and reuses **its** machinery — no bespoke "tz-version tracking" subsystem.

Two reasons jed's exposure is **strictly smaller than PostgreSQL's**:

1. **jed cannot build such an index today.** [indexes.md](indexes.md) §1 is "plain column keys only —
   expression keys rejected," and there is no read-recomputed generated column. The hazard is *latent*:
   it appears only the day expression indexes (or recomputed generated columns) land *together with* tz
   functions.
2. **The trigger is jed's, not the OS's.** Because §3 vendors a pinned version, the data shifts only on
   a jed release — discrete, version-stamped, identical across cores — not silently under a host
   glibc/ICU-style upgrade.

### PG's stance (the cautionary detail)

PostgreSQL marks `timezone('zone', ts)` (`AT TIME ZONE 'const'`) and 3-arg
`date_trunc(unit, ts, 'zone')` **IMMUTABLE** *specifically so they can be indexed* — a deliberate
practical fudge, since they genuinely depend on mutable tz data. (The bare cast `ts::date` is only
**STABLE** — it reads the session zone — so you cannot index it directly; you are pushed to the
immutable-but-actually-tz-dependent `AT TIME ZONE 'const'` form, which is the exposed one.) And PG built
collation *versioning* (`pg_collation.collversion`, mismatch warnings, `REINDEX`) but has **no**
equivalent tz-version tracking for these expression indexes — so on PG this breakage is *less*
detectable than the collation one. The admin simply has to know to `REINDEX` after a relevant tzdata
update. jed should do better (§6), not inherit the silence.

## 6. Mitigation — pin one tz version per file, version-stamp, degrade legibly

The remedy is the collation playbook ([compatibility.md §7–§10](compatibility.md), [collation.md](collation.md)),
applied verbatim:

- **One tzdata version pinned per file.** If/when a tz-dependent key can be *stored*, the file records
  the tzdata version that produced it (a manifest capability entry of kind ≈`reference-data`,
  [compatibility.md §6](compatibility.md)). All tz-derived keys in a file are that version, so two
  indexes can never silently disagree.
- **Capability handshake on open.** The engine's vendored tzdata version is checked against the file's.
  Match → full read-write. Mismatch on a *write-time* dependency (a functional index, a STORED generated
  column) → **reduced (read-only) via heap-scan**: the base table reads correctly (values are tz-free,
  §2), the suspect index is not used for acceleration and not maintained until a migration rebuilds it.
  This is [compatibility.md §8](compatibility.md)'s degradation, nothing new.
- **Read-required tz dependency** (a regular view or VIRTUAL generated column over a tz function whose
  semantics are absent) → **refused, legibly** with an `XX002`-class error that names the missing
  capability — the hard wall of [compatibility.md §11](compatibility.md).
- **Changing a file's tz version is a deliberate migration** (rebuild tz-derived indexes), never an
  accident of which binary opened it.

Because the verdict is a pure function of `(manifest, vendored version)`, it is **cross-core identical**
— the same §8/[determinism.md](determinism.md) byte-identity requirement that governs everything else.

## 7. The immutability stance — a decision to make deliberately, not inherit

jed should choose, rather than copy PG's fudge by default:

- **Option A — follow PG, but better.** Allow `AT TIME ZONE 'const'` / `date_trunc(…, 'zone')` in
  index / PK / generated-key positions, marked immutable, **plus** the §6 version-stamping PG lacks.
  More useful; strictly safer than PG because the gap becomes detectable and the data is vendored +
  pinned (so the verdict is deterministic).
- **Option B — jed-strict.** Classify tz-dependent functions as **non-immutable** and refuse them as
  index / PK / stored-generated keys entirely, sidestepping the hazard. More in character with jed's
  strictness; costs users the (occasionally useful) tz-bucketed index.

The proposal **leans A-with-tracking** — vendored + pinned tz data is exactly what makes the tracking
clean and deterministic, so jed can offer the feature *and* close the hole PG leaves open — but B is a
legitimate call and is recorded as open (§10).

## 8. Session zone and abbreviations

- **`TimeZone` / `time_zone`** already exists ([session.md §6.2](session.md)): default `UTC`,
  capability `session.timezone`, corpus directive `# timezone: <zone>`. It selects the zone for
  rendering and for the two-argument `STABLE` forms (`ts::date`, `date_trunc(unit, ts)`). Setting it is
  pure session state ([session.md](session.md) §6) — no storage effect, fully deterministic given the
  directive.
- **Abbreviations** (`EST`, `CST`, …) are ambiguous and PG keeps a separate curated table
  (`pg_timezone_abbrevs`, the `tznames/` files). Proposal: **defer** abbreviation *input* (accept only
  IANA zone names + fixed `±HH:MM` offsets initially), and decide later whether to vendor a curated
  abbreviation set. Abbreviations in *output* render from the active zone's TZif data (unambiguous).
  Open (§10).
- **`SET TIME ZONE` grammar + `pg_timezone_names`-style introspection** are follow-ons, gated behind the
  conversion slice.

## 9. Function / operator surface (future, deferred)

Already built (clock seam, [entropy.md](entropy.md)): `now()` / `current_timestamp` (STABLE),
`clock_timestamp()` (VOLATILE) → `timestamptz`.

Deferred, to land with the conversion slice (each oracle-checked against PG, [CLAUDE.md §7](../../CLAUDE.md)):
`AT TIME ZONE` (both directions), `timestamptz`↔`timestamp`/`date`/`text` casts in a zone,
`date_trunc(unit, ts[, zone])`, `EXTRACT`/`date_part`, `make_timestamptz`, `to_char`/`to_timestamp` (a
larger, later surface). All are **pure given the tz seam** (CLAUDE.md §13) — they read only the vendored
tz data + the instant, never host state — so they stay inside the untrusted-query safety guarantee. The
mutability label of each (IMMUTABLE vs STABLE) is the §7 decision.

## 10. Status & open decisions

**Status: unratified proposal.** The settled ground it builds on (UTC-`i64` `timestamptz`, the clock
seam, the `TimeZone` slot) is real and in-tree; the tz database, the conversions, and everything in §5–§7
are **not built and not decided**.

**Open — need a deliberate call before/at adoption:**

1. **Vendor compiled TZif bytes vs vendor `.zi` + a pinned compile step** (§3.3) — proposal leans
   compiled bytes (fewer moving parts, no per-core `zic`).
2. **Immutability stance** (§7) — Option A (allow + version-track) vs Option B (jed-strict refuse).
   Proposal leans A.
3. **Manifest registration of the tzdata version** (§6) — confirm tz reuses the
   [compatibility.md](compatibility.md) manifest as a Tier-2 reference-data capability, sharing the
   `XX002`-class legible-refusal / heap-scan-degradation machinery rather than a bespoke mechanism.
4. **tz-update / version policy** — cadence for bumping the vendored version, and whether the version is
   part of the conformance contract (proposal: yes — it is a §8 divergence hotspot, "which tz version
   produced this key").
5. **Abbreviation handling** (§8) — defer abbreviation input; decide later whether to vendor a curated
   abbreviation table.
6. **`AT TIME ZONE` with an unknown / absent zone name** — error code and behavior (proposal:
   conservative error, never a silent substitution — mirrors [compatibility.md §8](compatibility.md)'s
   "never silently substitute an ordering the user did not ask for").

**Registers into:** [compatibility.md](compatibility.md) (the manifest + graded verdict; tz is a Tier-2
versioned-reference-data instance alongside [collation.md](collation.md)), [indexes.md](indexes.md)
(if/when functional indexes land), [constraints.md](constraints.md) (tz-dependent DEFAULTs / generated
columns), [session.md](session.md) (the `TimeZone` slice), [encoding.md](encoding.md) (the i64-instant
key rule, unchanged).
