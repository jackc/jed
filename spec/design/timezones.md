# Time zones — design

> Time-zone support for `timestamptz`: a host-loaded IANA time-zone database the engine consults to
> map a UTC instant ↔ a local wall-clock reading for a named zone (`America/New_York`,
> `Europe/Paris`), exposed through the `AT TIME ZONE` operator. It deliberately copies the **collation
> data-handling architecture** ([collation.md](collation.md)): the bare binary carries **no** tz data,
> a host hands the engine a **`JTZ` bundle of bytes** through a privileged, bytes/reader-based call
> (`db.LoadTimeZoneData`) — never a file path, so the engine does no I/O — and the named zones in it
> become usable. The bundle is a manifest-indexed container wrapping the standardized **RFC 8536 TZif**
> per-zone blobs (§4); each core reads TZif with a small hand-written reader (§5). `UTC` and fixed
> `±HH:MM` offsets are **built-in and table-free** — the `C`-collation analogue — so a database that
> never converts through a named zone needs **no loaded data and pins no tz version**.
>
> **The one structural fact that makes tz easier than collation:** `timestamptz` is stored as a UTC
> `i64` instant and its ordering is integer comparison of those micros — **completely independent of
> the tz database** (§2). So tz support adds **no on-disk format change, no per-file reference entry,
> and no version-skew verdict** today: the base type is structurally immune to the
> tz-data-version-corruption hazard that forced collation's reference-entry + graded-verdict machinery.
> That machinery becomes relevant only when a *tz-derived key can be stored* — a functional index or a
> STORED generated column over `AT TIME ZONE 'const'` — neither of which jed can build yet (§8). Until
> then the hazard is **latent**, and when those features land tz registers into
> [compatibility.md](compatibility.md)'s manifest exactly as collation already does — not as a bespoke
> mechanism.
>
> The settled ground this builds on, all in-tree: `timestamptz` is a UTC `i64` instant (the clock-seam
> micros — [entropy.md §1](entropy.md); the i64-instant key rule, [encoding.md §2](encoding.md)); the
> `TimeZone` / `time_zone` session slot exists and defaults to `UTC` ([session.md §6.2](session.md));
> the clock functions `now()` / `current_timestamp` / `clock_timestamp()` produce `timestamptz` through
> the clock seam ([entropy.md §5](entropy.md)). The host-seam pattern is [hosts.md](hosts.md) /
> [entropy.md](entropy.md); the determinism stance [determinism.md §3](determinism.md); the cost
> contract [cost.md](cost.md); the byte formats this doc's data uses are pinned in
> [../tz/README.md](../tz/README.md). The grammar of the consumer is [grammar.md §49](grammar.md).
>
> **Status: design decided, awaiting review before implementation; nothing built.** Three foundational
> choices have been made (and supersede the earlier *vendor-into-each-core* proposal this doc used to
> carry): (1) tz data is **host-loaded**, not compiled into the binary, mirroring collation's Slice-3
> pivot ([collation.md §2/§9](collation.md)); (2) the delivery vehicle is a **new `JTZ` bundle wrapping
> standard TZif**, parallel to `JUCD` ([../collation/README.md §5](../collation/README.md)); (3) the
> first slice ships the **data plumbing + the single `AT TIME ZONE` consumer** to exercise it, not the
> full conversion surface (§9 lists the deferred remainder). This doc is the contract all three cores
> will implement in lockstep (CLAUDE.md §2) once the slice is approved; the byte-format details are
> [../tz/README.md](../tz/README.md). Open decisions are §14.

---

## 1. Scope

Time-zone support adds three things on top of the already-settled `timestamptz` instant type:

1. **A host-loaded IANA time-zone database** (§3/§4) the engine consults to map a UTC instant ↔ a
   local wall-clock reading for a named zone. Delivered as a **`JTZ` bundle** the host loads; the bare
   binary carries none (`UTC` and fixed offsets excepted — they are built-in, §3.2).
2. **The `AT TIME ZONE` operator** (§6) — the one conversion this slice ships, in both directions
   (`timestamptz AT TIME ZONE zone → timestamp`, `timestamp AT TIME ZONE zone → timestamptz`). It is
   the consumer that exercises the loaded data, exactly as `COLLATE` / `ORDER BY` exercised the loaded
   collation tables.
3. **A session zone** — the `TimeZone` GUC, *already present* ([session.md §6.2](session.md), default
   `UTC`, capability `session.timezone`, corpus directive `# timezone:`). `AT TIME ZONE` takes an
   explicit zone argument, so this first slice does **not** depend on the session slot; the session-zone
   uses (rendering `timestamptz` as `text`, the bare `::date` cast) are part of the deferred surface (§9).

What it deliberately does **not** add: any change to how `timestamptz` is *stored* or *compared* (§2),
and the broader conversion surface — `date_trunc(unit, ts, zone)`, `timestamptz`↔`date`/`text` casts in
a zone, `EXTRACT`, `make_timestamptz`, `to_char` (§9). Those land later, each oracle-checked against
PostgreSQL.

## 2. The representation — `timestamptz` is UTC, and that buys structural immunity

`timestamptz` is a UTC `i64` instant (microseconds; the clock-seam representation,
[entropy.md §1](entropy.md)). The session `TimeZone` and the `AT TIME ZONE` operator affect **input
parsing** and **output interpretation** only — never the stored bytes, never the comparison. This is
PostgreSQL's model exactly ([CLAUDE.md §1](../../CLAUDE.md): match PG): a `timestamptz` column stores
instants, and "in zone Z" is an I/O-time interpretation, not a stored property.

The consequence is the single most important fact in this doc, and it is a sharp contrast with
collation:

> **`timestamptz` ordering is integer comparison of UTC micros — completely independent of the tz
> database.** So a plain B-tree index, `ORDER BY ts`, `UNIQUE(ts)`, or `ts` as a primary-key member is
> **structurally immune** to the tz-data-version corruption hazard that §8 describes. A tzdata update
> cannot reorder stored instants, because no tz rule participates in their order.

Collation has no such escape: the comparison function for `text` *is* the versioned thing, so **every**
collated index is exposed when the library reorders, which is why collation needs a per-file version pin
and the graded open-time verdict ([collation.md §3/§12](collation.md)). Here the base type's comparison
is tz-free, so **none** of the plain timestamp indexes are exposed. The exposure is confined entirely to
keys that are *derived by applying a zone* and then stored — and jed cannot build one yet (§8).

This means **time-zone support adds nothing to the on-disk format** — **no `format_version` bump, no
per-file collation-style reference entry, no skew verdict.** `timestamptz` is already a UTC `i64`; the tz
database lives in the loaded set (§3), not in the file. The collation reference-entry + `XX002` verdict
half is **deliberately absent here**, latent until derived stored keys exist (§8/§10).

## 3. The time-zone database — host-loaded, never the OS

### 3.1 The architecture is collation's, verbatim

[collation.md §2](collation.md) rules out two options *before* any design choice, and the identical
reasoning applies to tz:

- **Reading the host's `/usr/share/zoneinfo` at query time is impossible here.** Two containers with
  different distro tzdata versions would compute *different* conversions for the same instant, and the
  three cores would diverge — the cross-core byte-identity violation ([CLAUDE.md §8](../../CLAUDE.md),
  [determinism.md](determinism.md)). This is the ICU-collation trap ([types.md §11](types.md)) again,
  and jed already rejects it once, for collation.
- **Letting tz conversion be a sanctioned query-time non-determinism** (a ledger exception) is refused
  for the same reason collation's is ([determinism.md §3](determinism.md)): tz conversion must be
  deterministic data, not a sanctioned exception.

So jed **owns the tz data as bytes it loads** and the running engine consults a **host-loaded bundle** —
the host environment is consulted only at *build time*, to produce the bundle, never by the running
engine. This is the same Tier-2 "versioned reference data" pattern as collation
([compatibility.md §5](compatibility.md)); the two differ only in that tz's *base type is already
version-independent* (§2), so only *derived* keys (§8) ever reach the version-skew part of that tier.

### 3.2 `UTC` and fixed offsets are built-in — the `C` analogue

Just as `"C"` is the table-free, always-available, built-in collation ([collation.md §1](collation.md)),
time zones have a table-free built-in baseline:

- **`UTC`** (the session default) and **fixed numeric offsets** `±HH`, `±HH:MM`, `±HH:MM:SS` are
  **built-in and require no loaded data** — a fixed offset is pure integer arithmetic on the instant.
- **Every *named* IANA zone** (`America/New_York`, and notably the `Etc/*` zones, whose POSIX sign
  convention is a foot-gun) requires a **loaded bundle**; naming one no loaded bundle provides is
  **`22023`** (`invalid_parameter_value`, PG's "time zone not recognized"; §6).

So a database that only ever stores UTC instants and never converts through a named zone needs **zero**
loaded tz data and **pins no tz version at all** — portable to every jed binary, forever. This is the
exact parallel of "a `C`/ASCII-only database carries zero collation data and pins no Unicode version"
([collation.md §3/§16](collation.md)).

### 3.3 The load seam — `db.LoadTimeZoneData(bytes)`

Loading is a **privileged host operation** that hands the engine bundle **bytes** — *not* SQL-reachable
and *not* a filesystem path, so an untrusted query can only ever *use* an already-loaded zone by name,
never trigger a load, and the engine itself does no I/O (the [hosts.md](hosts.md) BlockStore principle).
The API mirrors `db.LoadUnicodeData` ([collation.md §4.2](collation.md)) one-for-one:

```
// production host API (privileged — not untrusted SQL, §10):

db.LoadTimeZoneData(bytesOrReader)  // load a JTZ bundle: its named zones + links (§4); additive
db.LoadedTimeZones()                // introspect the loaded set: zone names, tzdata_version, description

// build-time tooling ONLY — compiled out of the production engine (§7):
//   the BUILDER TOOL   committed TZif blobs → a JTZ bundle (zones + links)
//   the VECTOR GEN     (zone, instant) → (offset, abbrev, dst) cross-core vectors
```

- **The loaded set is engine-global**, a property of the running engine, not of one handle — identical
  to collation's `LOADED` set ([collation.md §4.2](collation.md), the `static`/global declarations).
  Engine-global *because* a host may load a bundle **before `open`**; though tz needs no on-disk
  reference today (§2), keeping the seam identical to collation's means the latent derived-key path
  (§8) inherits the "open resolves references against the already-loaded set" behavior for free. Each
  core exposes the load both as the `db.` method and as an engine-level call the host may invoke prior
  to `open` (Rust `jed::load_time_zone_data`, Go `jed.LoadTimeZoneData`, TS `loadTimeZoneData`); both
  populate the one engine-global set.
- **Additive** — multiple bundles may be loaded; resolution is by name in load order (a name an
  earlier bundle already provides is kept — first-wins, matching collation's load merge).
- **No host-zoneinfo import path.** There is no `db.ImportTimeZone` / `LoadHostTimeZone` — the only
  load is of jed's **own pinned bundle**, which deserializes pinned bytes and reaches no host data.
  `UTC` / fixed offsets are never bundled, loaded, or referenced — they are built in (§3.2).

### 3.4 TZif is standardized, so this is the *easy* cross-core case

A pleasant asymmetry with the LZ4 decision ([large-values.md §6](large-values.md), CLAUDE.md §9), and
the reason tz's per-core algorithm is *simpler* than collation's UCA executor:

- **LZ4 encoders are not standardized** → a per-core library would produce different bytes → jed
  hand-rolled a byte-pinned codec.
- **TZif is standardized ([RFC 8536](https://datatracker.ietf.org/doc/html/rfc8536))** — like AEAD in
  [encryption.md](encryption.md). Independent per-core readers of the *same* TZif bytes agree **by
  construction**.

So jed **wraps the compiled TZif bytes as the bundle payload** (§4) and each core implements a small
RFC 8536 reader (§5). **No core runs `zic`** — which sidesteps the one real divergence risk (different
`zic` versions emit slightly different TZif framing). PostgreSQL vendors its *own* `zic` + reader for the
same cross-platform-consistency motive ([its `src/timezone/README`](../../references/postgres/src/timezone/README));
pinning the compiled bytes and loading them is the same idea, one step further, and a better fit for
jed's "data over code" discipline (CLAUDE.md §5) and the host-load model. The compiled-TZif-is-the-shared-form
choice is the exact analogue of collation's "the `.coll` table is the one shared cross-core form"
([collation.md §9](collation.md)).

## 4. The `JTZ` bundle — wrapping standard TZif

The container a host **loads** at runtime via `db.LoadTimeZoneData` — the production delivery vehicle for
the tz database. **One tzdata version per bundle.** It is a **manifest-indexed container** (the byte
format is pinned in [../tz/README.md §3](../tz/README.md)) of independently-addressable sections, so a
loader can take only the zones it needs (a browser fetches the manifest, then a zone on demand). It
reuses jed's existing conventions verbatim — big-endian, `str` = `u16` length + UTF-8, CRC-32/IEEE,
LZ4-block bodies ([../fileformat/lz4.md](../fileformat/lz4.md)) — exactly as `JUCD` does
([../collation/README.md §5](../collation/README.md)).

The shape, in brief (bytes in [../tz/README.md §3](../tz/README.md)):

- **Magic `JTZ\0\0\0`** + `format_version` + the single **`tzdata_version`** axis (e.g. `"2025b"`) + a
  provenance `description` (excluded from section hashes).
- A **manifest** (table of contents) of **zone sections**, each naming an IANA zone
  (`America/New_York`), with the section's content hash, lengths, and body offset — addressable without
  reading bodies.
- A list of **links** (alias → target, e.g. `US/Eastern → America/New_York`), carrying **no body** — an
  alias resolves to its target zone at load.
- A **body** region of the zone sections' LZ4 blocks (each block is a complete RFC 8536 TZif file), in
  manifest order, then a trailing `bundle_crc`.

A `zone` section payload is **the zone's TZif bytes verbatim** — jed adds no re-encoding, so the bundle
is "standard TZif in a manifest." This is the key simplification over `JUCD`, which needed a custom
compiled-table payload and a root+delta merge ([../collation/README.md §5.1](../collation/README.md));
tz has no merge step — a zone section is self-contained.

## 5. The TZif reader — the one new per-core algorithm

Each core implements one hand-written reader (CLAUDE.md §5 forbids codegenning it), deterministic and
cross-core byte-identical *by construction* (it reads standardized bytes, §3.4). It is the tz analogue
of collation's UCA executor ([collation.md §6](collation.md)) — the whole of the production tz "compute"
surface. Its contract:

> **`offset_at(zone_tzif, instant_micros) → (utc_offset_seconds, abbrev, is_dst)`** — the local-time
> type in effect at that instant for that zone.

The reader parses the **RFC 8536 v2+ 64-bit data block** (transition times, transition types, local
time-type records `{utoff, isdst, desigidx}`, and the abbreviation string table) and the **footer POSIX
TZ string** for instants past the last explicit transition. It **ignores** leap-second records and the
standard/wall + UT/local indicators (PG ignores them for these conversions too). The lookup is a binary
search of the transition table; beyond the last transition the footer rule governs. The exact byte
layout, the before-first-transition rule, and the POSIX-footer evaluation are pinned in
[../tz/README.md §2/§4/§5](../tz/README.md), with `(zone, instant) → (offset, abbrev, is_dst)` golden
vectors (§10). The footer evaluator is the meatiest sub-part — the first cut supports the near-universal
`Mm.w.d` transition form; the rare `Jn` / `n` julian-day forms are a documented follow-on (§14).

## 6. The consumer: `AT TIME ZONE`

The single conversion this slice ships (grammar [grammar.md §49](grammar.md)), matching PostgreSQL in
both directions:

- **`timestamptz AT TIME ZONE zone → timestamp`** — render the instant as the local wall-clock reading
  in `zone` (the result is a zone-less `timestamp`). Computed as `instant + offset_at(zone, instant)`.
- **`timestamp AT TIME ZONE zone → timestamptz`** — interpret the wall-clock reading as being in `zone`
  and produce the UTC instant. Computed by finding the offset such that `instant = wallclock − offset`;
  at a DST gap/overlap the wall-clock is non-existent or doubled, and jed resolves it **as PostgreSQL
  does** (oracle-pinned — PG picks a defined branch), never erroring on the ambiguity.

Details:

- **Grammar / precedence.** `AT TIME ZONE` is an infix operator binding tighter than the comparison
  operators and `||`, matching PG ([grammar.md §49](grammar.md)). The `zone` operand is a text
  expression evaluated per row; `UTC` and fixed `±HH:MM` offsets need no loaded data (§3.2).
- **Unknown / absent zone → `22023`** (`invalid_parameter_value`), the PG-matching "time zone "X" not
  recognized" — never a silent substitution (the conservative choice, mirroring
  [compatibility.md §8](compatibility.md) "never silently substitute an ordering the user did not ask
  for"). A named zone no loaded bundle provides raises this; so an untrusted query naming an unloaded
  zone gets `22023`, never a load and never I/O (§10).
- **Purity.** `AT TIME ZONE` is a pure function of `(instant, loaded TZif bytes)` — no host reach, no
  I/O, no nondeterminism — so it stays inside the untrusted-query safety guarantee (§10, CLAUDE.md §13).

## 7. Where the tz database is consulted — the read/write split

Mirroring [compatibility.md §4.2](compatibility.md), almost every tz dependency is needed to *write or
derive*, not to *read a stored value*:

| Use | tz data needed to **read** a stored value? | Notes |
|---|---|---|
| `timestamptz` stored value / comparison / plain index | **No** | UTC `i64`; tz-free (§2). The common case. |
| `AT TIME ZONE` in a query expression | the query reads it | Pure over the instant + loaded bytes; affects the *computed result*, never stored bytes or order. |
| Parse a `timestamptz` literal in a zone (future) | — (write path) | One-time at INSERT; result is frozen UTC. |
| Render a `timestamptz` as `text` in the session zone (future) | display only | Affects output formatting, never stored bytes or order. |
| `tz`-dependent **expression DEFAULT** ([constraints.md §2](constraints.md)) | **No** | Evaluated once at INSERT, frozen as a stored value; a plain index over that value stays consistent. **Not exposed.** |
| `tz`-dependent **functional index key** (future) | **No** for the base heap-scan; **yes** for index-accelerated lookup | The exposed case — §8. |
| `tz`-dependent **STORED generated column** (future) | **No** — value on disk | Exposed only on recompute/maintenance. |
| `tz`-dependent **VIRTUAL generated column / view** (future) | **Yes** — computed on read | Read-required; the hard wall ([compatibility.md §11](compatibility.md)). |

The takeaway: a stored instant is always readable on any core regardless of tz-version skew (its bytes
encode no zone). What can go stale is a **key or value derived by applying a zone** and then *stored* —
and only when that derivation is later expected to match a fresh evaluation under a different tzdata
version (§8).

## 8. The functional-index stability hazard — latent until derived keys can be stored

The question that historically motivated this doc: *does a functional index on, e.g.,
`(ts AT TIME ZONE 'America/New_York')::date` have the collation index-corruption problem?*

**Yes — that specific shape is the same corruption class.** The index key is a value *derived from the
row by applying tz rules*. A B-tree relies on `stored_key == f(row)`. A tzdata update can change `f` for
a fixed UTC instant:

- **Future-dated rows** are the common exposure — a government drops DST next year; re-deriving the same
  stored instant under the new data yields a different local date.
- **Historical corrections** too — tz releases routinely fix past offsets (especially pre-1970).

When that happens, stored keys no longer match a fresh derivation → index scans miss rows, indexed vs.
heap-scan plans disagree, a `UNIQUE` functional index can admit logical duplicates. Same *failure* as
collation, via a slightly different *mechanism*: collation flips the **comparison between two stored
keys** (the comparator is versioned); a tz functional index keeps a stable comparator (date compare) but
**stales the key derived from the row** (the derivation is versioned). Detection and remedy are
identical — version-stamp and rebuild.

**This is not a new mechanism**, and crucially **it is latent today:**

1. **jed cannot build such an index now.** [indexes.md §1](indexes.md) is "plain column keys only —
   expression keys rejected," and there is no read-recomputed generated column. The hazard appears only
   the day expression indexes (or STORED generated columns) land *together with* tz functions.
2. **The trigger is jed's, not the OS's.** Because §3 host-loads a pinned version, the data shifts only
   when a host loads a different bundle — discrete, version-stamped, identical across cores — never
   silently under a host glibc/ICU-style upgrade.

So this slice ships **no** version-pinning machinery — it would have nothing to protect (§2). When the
triggering features land, tz registers into [compatibility.md](compatibility.md)'s manifest as a Tier-2
versioned-reference-data capability: at that point a file gains a tzdata-version pin on its tz-derived
indexes, the open-time graded verdict degrades a skewed such index to read-only heap-scan, and a future
`db.upgrade_timezones()`-style migration (the [collation.md §12](collation.md) `db.upgrade_collations()`
analogue) rebuilds it. **Designed, not built** — and explicitly not part of this slice.

### PG's stance (the cautionary detail)

PostgreSQL marks `timezone('zone', ts)` (`AT TIME ZONE 'const'`) and 3-arg `date_trunc(unit, ts, 'zone')`
**IMMUTABLE** *specifically so they can be indexed* — a deliberate fudge, since they genuinely depend on
mutable tz data. (The bare cast `ts::date` is only **STABLE** — it reads the session zone — so you cannot
index it directly; you are pushed to the immutable-but-actually-tz-dependent `AT TIME ZONE 'const'` form,
the exposed one.) PG built collation *versioning* (`pg_collation.collversion`, mismatch warnings,
`REINDEX`) but has **no** equivalent tz-version tracking for these expression indexes — so on PG this
breakage is *less* detectable than the collation one; the admin simply has to know to `REINDEX` after a
tzdata update. jed should do better (the §8 manifest registration when the feature lands), not inherit
the silence — but only when there is something to track.

## 9. Function / operator surface

**This slice (the single consumer):** `AT TIME ZONE` (both directions, §6).

**Already built** (clock seam, [entropy.md §5](entropy.md)): `now()` / `current_timestamp` (STABLE),
`clock_timestamp()` (VOLATILE) → `timestamptz`.

**Deferred, to land with later conversion slices** (each oracle-checked against PG, CLAUDE.md §7):
`timestamptz`↔`timestamp`/`date`/`text` casts in a zone, `date_trunc(unit, ts[, zone])`,
`EXTRACT`/`date_part`, `make_timestamptz`, `to_char`/`to_timestamp` (a larger, later surface), and the
session-zone-driven rendering of `timestamptz` → `text`. All are **pure given the tz seam** (CLAUDE.md
§13) — they read only the loaded tz data + the instant, never host state — so they stay inside the
untrusted-query safety guarantee. The IMMUTABLE-vs-STABLE label of each is the §8 indexability decision,
made when expression indexes land.

## 10. Untrusted-query safety, cost, and the determinism ledger

Identical posture to collation ([collation.md §11](collation.md)):

- **Loading is a privileged host op; using is pure** (CLAUDE.md §13). `db.LoadTimeZoneData` is a
  privileged host-API call taking pinned bundle **bytes** (or a reader): **not SQL-reachable** (an
  adversarial query cannot trigger a load), takes **no filesystem path** (the engine does no I/O — the
  host sources the bytes, [hosts.md](hosts.md)), and constructs nothing from host data (it deserializes
  jed's own pinned TZif bytes). So an untrusted query can only ever *use* an already-loaded zone by
  name, or get `22023` (§6). Using a zone is **pure** — an instant and loaded TZif bytes in, an
  `(offset, abbrev, dst)` out; no host reach, no I/O, no nondeterminism.
- **Bounded cost.** `AT TIME ZONE` evaluation is metered by a new **`timezone`** cost unit (the
  `collate` analogue, [collation.md §8](collation.md)), charged **per conversion** at the operator's
  evaluation site — the deterministic, cross-core-identical metering point. A TZif lookup is a bounded
  binary search (+ a bounded footer evaluation), so a query converting a large input is cost-ceilinged
  like any other work ([cost.md](cost.md)). The unit is registered in
  [../cost/schedule.toml](../cost/schedule.toml) when the slice's accrual site lands (kept in lockstep
  with the hand-written accrual site, like every cost unit — CLAUDE.md §5); this doc specifies it, the
  implementation slice adds the data row.
- **tz *use* stays OUT of the determinism ledger.** A query runs over **loaded** TZif bytes with a
  jed-owned reader, so it is a deterministic function of its inputs — precisely what
  [determinism.md §3](determinism.md) demands. *Which* zones are loaded is a host/configuration boundary
  (like *which file you opened*), not a query-time draw, so it needs no ledger entry either: no query
  observes the load.

## 11. Cross-core determinism and verification

Tz is a §8 divergence hotspot handled by the established machinery (the [collation.md §10](collation.md)
template):

- **TZif reader vectors** — `(zone, instant) → (offset_seconds, abbrev, is_dst)`
  ([../tz/README.md §6](../tz/README.md)), the primary cross-core contract for the reader (§5),
  including instants in DST, at a transition boundary, before the first transition, and **past the last
  transition** (exercising the POSIX footer). Produced by the Rust core's vector generator and
  cross-confirmed byte-for-byte by Go and TS (UCA's precedent — [../collation/README.md §7](../collation/README.md)).
- **`JTZ` bundle vectors** — `(bundle bytes) → (parsed manifest + per-section round-trip)`, the bundle
  `Open`∘`Save` byte-exact on every core ([../tz/README.md §3/§6](../tz/README.md)).
- **Conformance entries** drive `AT TIME ZONE` by **referencing a loaded bundle** (the committed
  fixture, never the host) via a new **`# load-timezone:`** directive (the `# load-collation:` analogue),
  so all three cores read the identical TZif → identical conversions; **oracle-checked against
  `postgres:18`** where jed matches PG and overridden-with-reason where it diverges. The session zone is
  set with the existing `# timezone:` directive.
- **No golden DB / on-disk vector is needed** — tz adds no on-disk bytes (§2). (This is the part of
  collation's verification suite tz *skips*; it returns only when derived stored keys land, §8.)

## 12. Build-time tooling

The build-time half (compiled out of the production engine, the [collation.md §4.1](collation.md)
pattern), Rust-only like `build_collation_bundle.rs` / `gen_collation_vectors.rs` (the other cores only
*load* the bundle and *run* the reader):

- **The builder tool** — `impl/rust/src/bin/build_timezone_bundle.rs` (proposed): reads a directory of
  committed TZif blobs + a links list and packs them into a `JTZ` bundle (presets for a curated
  starter set vs. the full IANA set, §13/§14 — the collation tier/preset analogue). It does **not** run
  `zic`; the TZif bytes are committed source (§13).
- **The vector generator** — `impl/rust/src/bin/gen_timezone_vectors.rs` (proposed): emits the
  `(zone, instant) → (offset, abbrev, dst)` reader vectors and the bundle round-trip vectors, which Go
  and TS cross-confirm.

## 13. The data: `spec/tz/` and the version pin

`spec/tz/` is a spec data directory parallel to `spec/collation/` — the **byte-format spec, the
committed TZif source, the production bundle, and the verification vectors** (the source the bundle is
built from). The byte formats are pinned in [../tz/README.md](../tz/README.md). It holds:

- the pinned `tzdata_version` (e.g. `2025b`) and the **committed TZif blobs** under `<version>/` — the
  cross-core-deterministic, host-free tz source (committed, not `zic`-generated per build, §3.4),
- the **`JTZ` bundle(s)** the builder emits (`fixtures/tzdata.jtz`),
- **reader vectors** — `(zone, instant) → (offset, abbrev, dst)`,
- **bundle vectors** — `(bundle bytes) → (manifest + per-section round-trip)`.

**The starter zone set** (the collation "ship `unicode` + `es` first" analogue): the first production
bundle ships a small curated set chosen to exercise the corners — `UTC`/`Etc/UTC`, `America/New_York`
(hour offset + DST), `Europe/Paris`, `Asia/Kolkata` (a `+05:30` half-hour offset), `Australia/Lord_Howe`
(a 30-minute DST step), and a future-transition case (the POSIX footer). The full ~600-entry IANA set is
a builder **preset**, a follow-on, not a code change (§12).

## 14. Status & open decisions

**Status: design decided (the three foundational choices below), awaiting review before implementation;
nothing built.** The settled ground it builds on (UTC-`i64` `timestamptz`, the clock seam, the
`TimeZone` slot) is real and in-tree; the tz database, the load seam, the TZif reader, and `AT TIME ZONE`
are not yet built.

**Decided (this revision):**

- **Host-loaded, not vendored.** Tz data is loaded via `db.LoadTimeZoneData` from a `JTZ` bundle; the
  bare binary carries no tz data (`UTC` + fixed offsets excepted) — mirroring collation's Slice-3 pivot
  ([collation.md §2/§9](collation.md)). *(Supersedes the earlier vendor-into-each-core proposal.)*
- **`JTZ` bundle wrapping standard TZif** ([../tz/README.md §3](../tz/README.md)) — a manifest +
  per-zone RFC 8536 TZif sections + links, parallel to `JUCD`, no custom compiled-table payload and no
  merge step (§4).
- **Plumbing + one consumer.** The first slice ships the data path + `AT TIME ZONE` (both directions,
  §6); the broader conversion surface (§9) is deferred.
- **No on-disk change.** No `format_version` bump, no reference entry, no skew verdict (§2); the
  collation-style version-skew machinery is latent until tz-derived stored keys exist (§8).

**Open — need a deliberate call before/at implementation:**

1. **POSIX-footer form coverage** (§5) — first cut supports the near-universal `Mm.w.d` transition rule;
   the rare `Jn` / `n` julian-day forms are a proposed follow-on. Confirm the narrowing.
2. **The `timestamp AT TIME ZONE zone` DST-ambiguity branch** (§6) — pin jed's gap/overlap resolution to
   PG's via the oracle, and record it as a divergence only if jed deliberately differs.
3. **Starter zone set & presets** (§13) — confirm the curated first set and that the full IANA set is a
   builder preset, not a separate mechanism.
4. **Abbreviation input** (§15 below) — defer (accept IANA names + fixed `±HH:MM` only); decide later
   whether to load a curated abbreviation table.
5. **When the latent skew machinery lands** (§8) — it activates with functional indexes / STORED
   generated columns; confirm tz reuses [compatibility.md](compatibility.md)'s manifest then, not a
   bespoke mechanism.

## 15. Session zone and abbreviations

- **`TimeZone` / `time_zone`** already exists ([session.md §6.2](session.md)): default `UTC`, capability
  `session.timezone`, corpus directive `# timezone: <zone>`. It selects the zone for the deferred
  rendering / bare-cast surface (§9); this slice's `AT TIME ZONE` takes an explicit zone, so it is
  independent of the slot. Setting it is pure session state — no storage effect, fully deterministic
  given the directive. *(Today the slot accepts `UTC` + fixed offsets; once the bundle loads, a named
  zone becomes settable too — gated on a loaded bundle providing it, else `22023`.)*
- **Abbreviations** (`EST`, `CST`, …) are ambiguous and PG keeps a separate curated table
  (`pg_timezone_abbrevs`). Proposal: **defer** abbreviation *input* (accept only IANA zone names + fixed
  `±HH:MM` offsets initially), decide later whether to load a curated abbreviation section in the bundle.
  Abbreviations in *output* render from the active zone's TZif data (unambiguous, via the reader's
  `abbrev`). Open (§14).
- **`SET TIME ZONE` grammar + `pg_timezone_names`-style introspection** are follow-ons (`db.LoadedTimeZones()`
  is the host-API introspection this slice provides; §3.3).

**Registers into:** [compatibility.md](compatibility.md) (the manifest + graded verdict, *when* tz-derived
keys land — §8), [indexes.md](indexes.md) (if/when functional indexes land), [constraints.md](constraints.md)
(tz-dependent DEFAULTs / generated columns), [session.md](session.md) (the `TimeZone` slot),
[encoding.md](encoding.md) (the i64-instant key rule, unchanged), [grammar.md §49](grammar.md) (the
`AT TIME ZONE` operator), [../tz/README.md](../tz/README.md) (the byte formats).
