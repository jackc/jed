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
> the tz database** (§2). Plain column indexes remain timezone-independent. Derived expression
> keys and partial predicates pin their named timezone dependencies in format 32; mismatches
> disable their read access paths and refuse writes until an explicit atomic rebuild.
> [index-dependencies.md](index-dependencies.md) defines admission and lifetime rules.
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
   uses (rendering `timestamptz` as `text`, the bare `::date` cast) came later (§9).

**Slice 2 adds the conversion surface** — `date_trunc` (incl. the 3-arg `date_trunc(unit, ts, zone)`),
`EXTRACT`, and the cross-family `timestamp`/`timestamptz`/`date` casts in a zone (§9) — and with it makes
the session `TimeZone` slot ([session.md §6.2](session.md)) observable: it is the zone a `timestamptz` is
decomposed *in*. Each is oracle-checked against PostgreSQL.

What it deliberately does **not** add: any change to how `timestamptz` is *stored* or *compared* (§2);
the `text`↔datetime casts, `make_timestamptz`, `to_char`/`to_timestamp`, and `date_part` (§9, the
deferred remainder); and the **session-zone rendering** of a `timestamptz` to text — the session zone
drove *computation* in this slice, not *output formatting*. Those landed later, each oracle-checked
against PostgreSQL (session-zone rendering, the runtime `text`→datetime casts, and offset-less input in
the session zone — §9.5, §9.3).

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
keys that are *derived by applying a zone* and then stored. Expression keys and partial predicates
now admit these operations under [index-dependencies.md](index-dependencies.md). Format 32 pins the
exact zone names, release labels, and TZif checksums used by each index. Plain instant columns and
indexes still need no timezone metadata.

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
- **Range — a shift out of range is `22008`.** jed's timestamp range is the full `i64` of microseconds
  (≈ 290309 BC … 294247 AD, [timestamp.md](timestamp.md) §1), with `i64::MIN`/`MAX` reserved for
  `±infinity`. A finite value within an offset of either end can shift past it, so both kernels are
  **checked**: a result that overflows `i64` *or lands exactly on a sentinel* (which would silently read
  as `±infinity`) traps `22008` "timestamp out of range" — PostgreSQL's code for an out-of-range
  timestamp. `±infinity` inputs pass through unshifted and never reach the kernels. Every zone-shift
  consumer inherits the guard: `AT TIME ZONE`, the cross-family casts (§9.3), `date_trunc` (§9.1) and
  `EXTRACT`/`date_part` (§9.2) in a zone, `make_timestamptz`, and the session-zone text render (§9.5).
  These extremes lie far outside PostgreSQL's range, so they are not oracle-checkable; each core pins
  them in its `zone_shift_range` test.
- **Purity.** `AT TIME ZONE` is a pure function of `(instant, loaded TZif bytes)` — no host reach, no
  I/O, no nondeterminism — so it stays inside the untrusted-query safety guarantee (§10, CLAUDE.md §13).

## 7. Where the tz database is consulted — the read/write split

Mirroring [compatibility.md §4.2](compatibility.md), almost every tz dependency is needed to *write or
derive*, not to *read a stored value*:

| Use | tz data needed to **read** a stored value? | Notes |
|---|---|---|
| `timestamptz` stored value / comparison / plain index | **No** | UTC `i64`; tz-free (§2). The common case. |
| `AT TIME ZONE` in a query expression | the query reads it | Pure over the instant + loaded bytes; affects the *computed result*, never stored bytes or order. |
| Parse an offset-less `timestamptz` string in the session zone (every input path: literal, assignment, bound parameter, container text, the runtime `text` cast — §9.3) | — (write path) | The result is a UTC instant; STABLE, so never a persisted index key. |
| Render a `timestamptz` as `text` in the session zone (the `::text` cast, `array → text`, and the host's session render of a result cell — §9.5) | the query reads it | Affects the computed text, never stored bytes or order; STABLE, so never a persisted index key. |
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

**Persisted derived expressions now carry this protection.** Format 32 stores each index's timezone
dependencies. Static zone arguments pin their named data; a dynamic zone argument pins the whole
loaded set. Missing or mismatched data excludes the index from read plans and refuses writes with
`XX002`. The host can load the intended bundle in a fresh process and rebuild indexes using atomic
`BEGIN; DROP INDEX; CREATE INDEX; COMMIT`. A failed rebuild can be rolled back without losing the
old committed index. See [index-dependencies.md](index-dependencies.md) for admission, encoding,
cache behavior, and the full rebuild procedure.

### PG's stance (the cautionary detail)

PostgreSQL marks `timezone('zone', ts)` (`AT TIME ZONE 'const'`) and 3-arg `date_trunc(unit, ts, 'zone')`
**IMMUTABLE** *specifically so they can be indexed* — a deliberate fudge, since they genuinely depend on
mutable tz data. (The bare cast `ts::date` is only **STABLE** — it reads the session zone — so you cannot
index it directly; you are pushed to the immutable-but-actually-tz-dependent `AT TIME ZONE 'const'` form,
the exposed one.) PG built collation *versioning* (`pg_collation.collversion`, mismatch warnings,
`REINDEX`) but has **no** equivalent tz-version tracking for these expression indexes — so on PG this
breakage is *less* detectable than the collation one; the admin simply has to know to `REINDEX` after a
tzdata update. jed detects this dependency mismatch and requires the explicit rebuild described above.

## 9. Function / operator surface

**Slice 1 (the first consumer):** `AT TIME ZONE` (both directions, §6).

**Slice 2 — the conversion surface (LANDED, this revision):** `date_trunc`, `EXTRACT`, and the
cross-family datetime casts, all consuming the session zone (§9.1–§9.4). This is the slice that turns
the session `TimeZone` slot ([session.md §6.2](session.md)) from a stored string into observable
behavior: a `timestamptz` decomposed into wall-clock components is decomposed **in the session zone**,
exactly as PostgreSQL does. Each is oracle-checked against `postgres:18` (CLAUDE.md §7).

**Already built** (clock seam, [entropy.md §5](entropy.md)): `now()` / `current_timestamp` (STABLE),
`clock_timestamp()` (VOLATILE) → `timestamptz`.

**Still deferred** (each oracle-checked against PG when it lands, CLAUDE.md §7): `to_char` /
`to_timestamp` (a parsing/formatting surface, a different axis from "in a zone").
**`age`** has landed ([interval.md](interval.md) §5.2): `age(timestamptz, timestamptz)` decomposes
both instants in the session zone, and the one-argument forms read the current day there.
Session-zone rendering of a `timestamptz` — the `::text` cast, nested in `array → text`, and result
cells through the host's session render — has landed (§9.5). The runtime `text → date` /
`timestamp` / `timestamptz` casts ([date.md §6](date.md), §9.3), offset-less `timestamptz` input in
the session zone on every path (§9.3), **`date_part`** (§9.2), and
`make_timestamptz` (the make_timestamp slice, [functions.md §11](functions.md)) have landed. All remain
**pure given the tz seam** (CLAUDE.md §13) — they read only the loaded tz data + the instant, never host
state — so they stay inside the untrusted-query safety guarantee. The IMMUTABLE-vs-STABLE label of each
is the §8 indexability decision, made when expression indexes land.

### 9.1 `date_trunc`

`date_trunc(unit, source)` rounds a value **down** to the start of `unit`. `unit` is a `text` value
(evaluated per row, case-insensitive, with PostgreSQL's aliases — `hrs`, `mons`, `y`, … — §9.6); an
unrecognized unit is **`22023`** (`invalid_parameter_value`, "unit "x" not recognized for type Y"),
raised **at evaluation** like PG (so `date_trunc('nope', ts)` over zero rows raises nothing). Overloads:

| Form | Returns | Zone |
|---|---|---|
| `date_trunc(unit, timestamp)` | `timestamp` | none — truncates the stored wall clock |
| `date_trunc(unit, timestamptz)` | `timestamptz` | **the session zone** — truncates in it, returns the instant of that local boundary |
| `date_trunc(unit, interval)` | `interval` | none |
| `date_trunc(unit, timestamptz, zone)` | `timestamptz` | the **explicit** `zone` argument (3-arg form; an unknown zone is `22023`) |

Units for `timestamp`/`timestamptz`: `microseconds`, `milliseconds`, `second`, `minute`, `hour`, `day`,
`week` (back to Monday), `month`, `quarter`, `year`, `decade`, `century`, `millennium`. For `interval`
the same units **except `week`** (`0A000`, matching PG); the year-group units (`decade`/`century`/
`millennium`) truncate the months field to a multiple of 10/100/1000 years. The `timezone*` units are
recognized but unsupported for every source type (`0A000`). The unit is checked before a `±infinity`
source passes through unchanged, as in PG. A `date` argument is
**`42883`** (no overload) — jed has no implicit `date`→`timestamp` cast (PG accepts it via that implicit
cast; a documented divergence, cast explicitly with `::timestamp`). The session-zone form charges the
`timezone` cost unit (§10); the zone-free forms do not.

### 9.2 `EXTRACT`

`EXTRACT(field FROM source)` returns the requested `field` of `source` as **`numeric`** (PG 14+ —
matchable exactly; jed has exact `decimal`). The `field` is **syntactic** (an identifier or a string
literal, case-insensitive — [grammar.md §50](grammar.md)), so it is validated at **resolve** time, not
per row. **`date_part('field', source)` has since landed** (`float` unblocked its `double precision`
result): the SAME field values via the shared extract kernel, converted decimal→f64 (one
correctly-rounded parse — the `R` tolerant render applies). Its `field` is a **runtime text value**
(a column works), case-insensitive, validated per row **at eval** like `date_trunc`'s unit (`22023`
unrecognized / `0A000` unsupported, zero rows raise nothing). Its **`date` overload widens to
midnight** and uses the *timestamp* matrix below — PG's own catalog definition (`date_part(text,
date)` is a SQL wrapper over `::timestamp`), so `date_part('hour', d)` is `0` where `EXTRACT(hour
FROM d)` is `0A000` and the `0A000` message names the widened type. `julian` is the one field not
converted from EXTRACT's decimal: it is PG's own float8 formula, `jd + (seconds + fsec/1e6) / 86400.0`
(`timestamp_part_common`), so the value is PostgreSQL's to the bit. Its `timestamptz` overload decomposes in the session
zone (volatility **stable**, matching PG; EXTRACT's selective `timezone` charge); the
date/timestamp/interval overloads are **immutable** (suites/expr/date_part.test, oracle-checked).

`source` may be `timestamp`, `timestamptz`, `date`, or `interval`. For `timestamptz` every field is
computed **in the session zone** (so `hour`/`day`/… shift with it) **except `epoch`** (zone-independent
— the instant itself) and the `timezone*` fields (the session zone's offset at that instant). The
field-validity matrix matches PG exactly (`EXTRACT(hour FROM date)` → `0A000`, `EXTRACT(dow FROM
interval)` → `0A000`, etc.):

| field | timestamp / timestamptz | date | interval |
|---|---|---|---|
| `microseconds` `milliseconds` `second` `minute` `hour` | ✓ | `0A000` | ✓ |
| `day` `month` `quarter` `year` `decade` `century` `millennium` `week` | ✓ | ✓ | ✓ |
| `dow` `isodow` `doy` `isoyear` | ✓ | ✓ | `0A000` |
| `epoch` | ✓ | ✓ | ✓ |
| `timezone` `timezone_hour` `timezone_minute` | tstz ✓ / ts `0A000` | `0A000` | `0A000` |
| `julian` | ✓ | ✓ | `0A000` |

A field may be spelled with any of PostgreSQL's aliases (`hours`, `msec`, `y`, `jd`, `mm`, …, §9.6).
An unsupported field-for-type is **`0A000`** (`feature_not_supported`, PG's "unit X not supported for
type Y"); an **unrecognized** field name is **`22023`** (PG's "unit X not recognized for type Y"). A
reserved word other than `epoch` (`infinity`, `now`, `today`, …) is `0A000` for a date/time source
and `22023` for an interval, as in PG. Both checks precede the infinite-source rule below.
`julian` is the Julian Day (PG's `date2j`; 4714-11-24 BC is day 0, 1970-01-01 is 2440588). Over a `date` it
is the integer day number, `days + 2440588`. Over a `timestamp` it adds the fraction of the day, computed
as PostgreSQL's numeric path does: `jd + (time-of-day µs / 86400000000)`, the division at numeric's
`select_div_scale` ([decimal.md §4](decimal.md)) — so the scale is PG's (`.0000000000000000000000000000`,
28 digits, at midnight; 20 at noon; `2000-01-01 00:00:00.000001` is
`2451545.0000000000115740740740740741`). A `timestamptz` decomposes in the session zone like every other
wall-clock field, so local midnight is `.0` (oracle-checked in `suites/expr/extract_julian.test`).
**An infinite source** follows PostgreSQL 18's `NonFiniteTimestampTzPart` (timestamp/timestamptz) and
`extract_date` (date), after the field is decoded as above. A **monotonically-increasing** field — `year`,
`decade`, `century`, `millennium`, `julian`, `isoyear`, `epoch` — is `±Infinity` with the source's sign;
every other field PG accepts **oscillates** and is **NULL**. For a `timestamp`/`timestamptz` every unit is
one or the other — even `timezone*` of a `timestamp`, which is NULL — and the session zone is never
consulted. For a `date` the oscillating fields are `day`/`month`/`quarter`/`week`/`dow`/`isodow`/`doy`;
the time and `timezone*` fields stay `0A000`. `date_part` returns both answers (its float8 has
`±Infinity`, [float.md](float.md)); its `date` overload widens to an infinite timestamp, so
`date_part('hour', date 'infinity')` is NULL. `EXTRACT` returns the NULLs, but its `±Infinity` traps
**`22003`** ("cannot extract field from an infinite timestamp" / "… date") — jed's `decimal` is
finite-only ([decimal.md](decimal.md) §2), where PG returns numeric `±Infinity`. And because `EXTRACT`'s
field is validated at resolve against a finite value, `EXTRACT(timezone* FROM timestamp 'infinity')` is
`0A000` like a finite timestamp's, where PG returns NULL. Both are documented divergences, per-core
tested; the rest is oracle-checked in `suites/expr/extract_infinity.test`. Field formulas:
`second`/`milliseconds`/`microseconds` carry the fractional seconds (exact decimal); an `interval`'s
fields follow PG's `interval_part_common` — its months/days/micros decompose independently, each keeping
its own sign (`year = months/12`, `month = months%12`, truncating; `week = days/7`; `hour` = whole hours of
the time, unbounded, then `minute`/`second` of the remainder; `decade`/`century`/`millennium` truncate
`year` toward zero), and `quarter` works from the *signed months count*, not the month-of-year:
`(months%12)/3 + 1` for `months ≥ 0`, else `−((−months%12)/3 + 1)` — so `−12 mons` is quarter `−1` (the
negation of `12 mons`'s `1`); `epoch` of an
`interval` is `(months/12)·31557600 + (months%12)·2592000 + days·86400 + time` (PG's `365.25`-day year,
`30`-day month — exact, since `365.25·86400 = 31557600` is integral, and carried exactly even past i64
microseconds — PG's numeric fallback); `century = (year−1)/100 + 1`,
`decade = ⌊year/10⌋`, ISO `week`/`isoyear`/`isodow` per ISO-8601.

### 9.3 Cross-family datetime casts

The `timestamp`/`timestamptz`/`date` cross-family cast matrix (`spec/types/casts.toml`), the **"casts in
a zone"** of CLAUDE.md §1. Every cast that crosses the `timestamptz` boundary consults the session zone;
the others are zone-free:

| Cast | Behavior |
|---|---|
| `timestamptz` → `timestamp` | render the instant as the local wall clock **in the session zone** (= `AT TIME ZONE session`) |
| `timestamptz` → `date` | the date of that local wall clock (session zone) |
| `timestamp` → `timestamptz` | interpret the wall clock **in the session zone** → the instant (= `AT TIME ZONE session`; DST gap/overlap resolved as PG, §6) |
| `timestamp` → `date` | the date part (zone-free) |
| `date` → `timestamp` | midnight (zone-free) |
| `date` → `timestamptz` | midnight **in the session zone** → the instant |

These reuse the §5 reader kernels (`instant_to_local_micros` / `local_to_instant_micros`) with the
session zone, so they inherit `AT TIME ZONE`'s oracle-clean DST behavior and its range guard (a shift
out of range is `22008`, §6). A session-zone-consulting cast charges the `timezone` unit. `date` is wider
than `timestamp` ([date.md](date.md) §1), so widening a date past the timestamp range (`date` →
`timestamp`/`timestamptz`) is `22008` too.

The runtime **`text` → datetime** casts on a non-literal text expression have landed: `text → date`
([date.md §6](date.md)), and `text → timestamp` / `timestamptz` ([types.md](types.md) §5), each running
its literal's parser per row (`22007`/`22008`). `text → timestamp` ignores an offset. **`text →
timestamptz` reads an offset-less string in the session zone** — the wall clock goes through
`local_to_instant_micros` exactly like `timestamp → timestamptz` (same DST gap/overlap choice, one
`timezone` unit; a shift past `i64` is `22008`) — while an explicit offset or `±infinity` fixes the
instant with no zone consult. All three are STABLE (PG's `*_in` are), so `42P17` in an index expression.

**Every other `timestamptz` input reads an offset-less string in the session zone too**, through the
same parser (DST and the `22008` range guard included): a string literal adapting to a `timestamptz`
context (`tzcol = '2024-01-15 12:00'`), the `timestamptz '…'` typed literal and a cast of a string
literal (`'…'::timestamptz`), an `INSERT … VALUES` / `UPDATE … SET` assignment, a constant column
`DEFAULT`, a text value bound to a `timestamptz` parameter, and a `timestamptz` inside array / range /
record text (`'{…}'::timestamptz[]`, `'[…)'::tstzrange`, `record_in`) and the range constructor and
element operators. Each core threads the zone explicitly through those parsers as an *input zone*
that also records whether a parse consulted it; there is no ambient state. An explicit offset or
`±infinity` never consults the zone. Oracle-checked in `suites/types/timestamptz_session_input.test`.

- **When the zone is read.** A literal is folded at resolve, so it reads the zone of the session that
  resolves the statement, matching PostgreSQL's parse-time fold. A constant `DEFAULT` folds once at
  `CREATE TABLE` / `ADD COLUMN` in the creating session's zone and is stored as an instant, as in PG.
  An assignment, a bound parameter, and per-row container text read the zone of the executing
  statement. Literal folds are unmetered, like every literal fold; the runtime `text → timestamptz`
  cast keeps its per-row `timezone` charge.
- **Plan cache (a deliberate divergence).** A resolve-time fold that read the zone makes its plan
  uncacheable, because a cached plan is shared across sessions ([api.md](api.md) §2.4). A prepared
  statement therefore re-reads the session zone on every execution. PostgreSQL's cached plan keeps the
  instant folded under the prepare-time zone, the same footgun jed declines for `'today'`
  ([date.md](date.md) §6).
- **Persisted expressions (a deliberate divergence).** jed re-resolves an index expression or
  partial-index predicate per statement, so a literal folded in the session zone would change meaning
  with each statement's zone. A fold that read the zone therefore marks the expression nonimmutable: an
  offset-less `timestamptz` literal in an index expression or predicate is `42P17` (write the offset).
  PostgreSQL instead folds it into the index definition. A `CHECK` constraint and an *expression*
  `DEFAULT` are also re-resolved per statement, so they follow that statement's zone, where PostgreSQL
  folds both at creation.

### 9.4 Where the session zone is read — eval, not render

The session zone reaches evaluation through the active session (it is **session state**, swapped into
the executing handle for the statement's duration), so `date_trunc(unit, timestamptz)`, `EXTRACT` of a
`timestamptz`, and the `timestamptz`-crossing casts read it at their evaluation site (the deterministic,
cross-core metering point — like `AT TIME ZONE`). They are never folded at resolve time (no constant
folding evaluates them), so the zone is always the one in effect when the row is processed.

### 9.5 Rendering: every `timestamptz` → text follows the session zone

The explicit **`timestamptz → text` cast** (`tz::text`, `CAST(tz AS text)`, `tz::varchar(n)` —
[types.md](types.md) §5) renders the instant **in the session zone**, as PostgreSQL's `timestamptz_out`
does: the local wall clock of §9.3's `timestamptz → timestamp` conversion, followed by the offset in
effect at that instant in PostgreSQL's `EncodeTimezone` spelling — `+HH` for a whole-hour offset,
`+HH:MM` when the minutes are nonzero (`Asia/Kolkata`'s `+05:30`), `+HH:MM:SS` when the seconds are (a
pre-standard-time local mean time such as New York's `-04:56:02`), placed before any ` BC` era.
`±infinity` render bare and consult no zone. A finite instant charges one `timezone` unit, and the cast
is **STABLE** (`42P17` in an index expression or predicate), the same posture as the §9.3 casts. The
element-wise `timestamptz[] → text[]` cast renders each element the same way. Oracle-checked
(`suites/cast/scalar_to_text.test`).

Every other place a `timestamptz` becomes text follows the same rule, so a session zone governs
computation (§9.4), input (§9.3), and output alike. Oracle-checked in
`suites/types/timestamptz_session_render.test`.

- **The `array → text` cast** (`array_out`, [array.md](array.md) §7) renders a `timestamptz` at any
  depth in the session zone: an element, a field of a composite element (`record_out`), or a bound of
  a range element (`range_out`). It charges one `timezone` unit per finite `timestamptz` it renders
  (NULL and `±infinity` charge nothing), all charged and guarded before rendering, on top of its
  existing `operator_eval` and output-byte charges. The cast is **STABLE iff its element type contains
  `timestamptz`** anywhere (through composite fields, range subtypes, and nested arrays), so
  `42P17` in an index expression or predicate; an array whose element type holds no `timestamptz`
  renders zone-free and stays admissible. (PostgreSQL marks `array_out` STABLE for every element type;
  jed's per-operation dependency rule, [index-dependencies.md](index-dependencies.md), admits the
  zone-free ones.) The composite → text and range → text casts remain `0A000`. The JSON conversions
  (`to_json`, `to_jsonb`, `array_to_json`, `row_to_json`, `JSON_SCALAR`, the json[b] builders and
  aggregates) render a
  `timestamptz` at any depth in the session zone in PostgreSQL's ISO form
  (`"2024-01-15T17:30:00+05:30"`), with the same per-instant `timezone` charge and the same
  STABLE-iff-it-holds-`timestamptz` rule ([json-sql-functions.md](json-sql-functions.md) §2.2).
- **Result cells.** A result cell is a zone-free value; turning it into text is a host act. The host
  API therefore has two renders:
  - the **canonical render** — Rust `Value::render`, Go `Value.Render`, TS `render(value)` — is
    zone-free: a `timestamptz` renders in UTC (`+00`). It needs no session, never fails, and its
    output re-parses to the same instant under any session zone. The CLI's `--dump` uses it so a dump
    does not depend on the zone it was taken in.
  - the **session render** — Rust `Session::render` / `Transaction::render`, Go `Session.Render` /
    `Transaction.Render`, TS `Session.render` / `Transaction.render` — renders a value the way
    PostgreSQL shows a result column: every `timestamptz` in it, at any depth inside an array,
    composite, or range, in the session's **current** zone. It is host-side work after the
    statement, so it is unmetered. It fails with `22008` only for a finite instant within an offset of
    jed's `i64` extremes (§6), far outside PostgreSQL's range; inside PostgreSQL's range it cannot fail.
  The conformance harness renders every result cell through the session render of the session that ran
  the record, so a `# timezone:` directive governs the record's rendering too. A render failure fails
  the record with its code, as an output-time error fails the query in PostgreSQL.
- **Hosts with no zone setting.** The CLI and the web playground never set a zone; their sessions run
  in `UTC`, where the session render equals the canonical render. The TS ergonomic row mapping
  (`valueToJs`) returns the canonical text for the types it does not map natively, so it is zone-free;
  a host that wants session-zone text calls the session render.
- **EXPLAIN (a deliberate divergence).** `EXPLAIN (VERBOSE)` prints a `timestamptz` constant in its
  canonical UTC spelling (`'2024-01-15 12:00:00+00'`), where PostgreSQL deparses it in the session
  zone. jed's plan text is its own context-free spelling (structural column slots, not a SQL
  deparse — [explain.md](explain.md)), so the same plan prints identically under every session; the
  explicit `+00` keeps the constant unambiguous.

### 9.6 Unit names and their aliases

`EXTRACT(field FROM …)`, `date_part(text, …)`, and `date_trunc(text, …)` read a unit name the way
PostgreSQL 18 does (`datetime.c`'s `DecodeUnits` / `DecodeSpecial`), so every PostgreSQL alias works
and then behaves exactly like its canonical unit. The two keyword tables, `deltatktbl` and
`datetktbl`, are copied entry for entry into shared spec data,
[spec/types/datetime_units.toml](../types/datetime_units.toml), and codegen'd into each core
(`datetime_units_gen.{rs,go,ts}`, `scripts/gen_datetime_units.rb`). The lookup is hand-written:

1. **Fold.** The name is ASCII-lowercased (`downcase_truncate_identifier`; a non-ASCII letter keeps
   its case). The bare-word form `EXTRACT(HOURS FROM …)` is already folded by the lexer; a quoted
   identifier `"HOURS"` or a string `'HrS'` is folded here. Error messages print the folded name.
2. **Truncate.** A lookup compares at most 10 bytes (`TOKMAXLEN`): a name matches a token when its
   first 10 bytes equal it. So a token shorter than 10 bytes must match the whole name, and any name
   starting with a 10-byte token is that token: `microseconds` and `microsecondsXYZ` are
   `microsecon`, `millisecond` is `millisecon`, `millenniums` is `millennium`, `timezone_hour` and
   `timezone_hours` are `timezone_h`. `millis` matches nothing.
3. **Search.** `date_trunc` searches `deltatktbl` only; a `units` entry is its unit, anything else
   is `22023`. `EXTRACT` / `date_part` search `deltatktbl` and, only when the name is absent there,
   `datetktbl`; a `units` entry is the field, the reserved word `epoch` is the `epoch` field, any
   other reserved word is unsupported (`0A000`, but `22023` for an interval), and anything else —
   `ago`, `@`, month and weekday names — is `22023`.

The aliases, by canonical unit:

| unit | aliases (`deltatktbl`) | `EXTRACT` / `date_part` only (`datetktbl`) |
|---|---|---|
| `microseconds` | `us` `usec` `usecs` `usecond` `useconds` `microsecon…` | |
| `milliseconds` | `ms` `msec` `msecs` `msecond` `mseconds` `millisecon…` | |
| `second` | `s` `sec` `secs` `seconds` | |
| `minute` | `m` `min` `mins` `minutes` | `mm` |
| `hour` | `h` `hr` `hrs` `hours` | |
| `day` | `d` `days` | |
| `week` | `w` `weeks` | |
| `month` | `mon` `mons` `months` | |
| `quarter` | `qtr` | |
| `year` | `y` `yr` `yrs` `years` | |
| `decade` | `dec` `decs` `decades` | |
| `century` | `c` `cent` `centuries` | |
| `millennium` | `mil` `mils` `millennia` `millennium…` | |
| `timezone` `timezone_hour` `timezone_minute` | `timezone_h…` `timezone_m…` | |
| `julian` | | `j` `jd` `julian` |
| `dow` `doy` `isodow` `isoyear` `epoch` | | (their own names only) |

PostgreSQL's shadowing carries over: `m` is the minute (`deltatktbl`) although `datetktbl` reads it as
a month, and `mm` exists only in `datetktbl`, so `EXTRACT(mm FROM ts)` is the minute while
`date_trunc('mm', ts)` is `22023`. `EXTRACT`'s field is PostgreSQL's `extract_arg` (grammar.md §50):
an identifier, bare or quoted, or a string, so `EXTRACT(hours FROM …)`, `EXTRACT("HOURS" FROM …)`, and
`EXTRACT('HOURS' FROM …)` agree; a zero-length `EXTRACT("" FROM …)` is `42601`, and `EXTRACT('' FROM
…)` is an unrecognized unit (`22023`). `dec` is a keyword in PostgreSQL, so there only the string or
quoted form (`EXTRACT('dec' FROM …)`, `EXTRACT("dec" FROM …)`) works, where jed's bare word also
accepts `EXTRACT(dec FROM …)` — jed reserves no keywords (grammar.md §3). Oracle-checked in
`suites/expr/datetime_unit_aliases.test`.

## 10. Untrusted-query safety, cost, and the determinism ledger

Identical posture to collation ([collation.md §11](collation.md)):

- **Loading is a privileged host op; using is pure** (CLAUDE.md §13). `db.LoadTimeZoneData` is a
  privileged host-API call taking pinned bundle **bytes** (or a reader): **not SQL-reachable** (an
  adversarial query cannot trigger a load), takes **no filesystem path** (the engine does no I/O — the
  host sources the bytes, [hosts.md](hosts.md)), and constructs nothing from host data (it deserializes
  jed's own pinned TZif bytes). So an untrusted query can only ever *use* an already-loaded zone by
  name, or get `22023` (§6). Using a zone is **pure** — an instant and loaded TZif bytes in, an
  `(offset, abbrev, dst)` out; no host reach, no I/O, no nondeterminism.
- **Bounded cost.** A zone consultation is metered by the **`timezone`** cost unit (the `collate`
  analogue, [collation.md §8](collation.md)), charged **once per zone lookup** at the evaluation site —
  the deterministic, cross-core-identical metering point. `AT TIME ZONE` charges it per conversion; the
  Slice-2 consumers charge it exactly when they consult a zone (`date_trunc(timestamptz)` / its 3-arg
  form, an `EXTRACT` of a `timestamptz` that decomposes it in the session zone, and a `timestamptz`-
  crossing cast) and **not** for the zone-free forms (`date_trunc(timestamp)`, `EXTRACT(epoch FROM
  timestamptz)`, `EXTRACT` of a `timestamp`/`date`/`interval`, the zone-free casts). A TZif lookup is a
  bounded binary search (+ a bounded footer evaluation), so a query converting a large input is cost-
  ceilinged like any other work ([cost.md](cost.md)). The unit is registered in
  [../cost/schedule.toml](../cost/schedule.toml) (`timezone`, landed with Slice 1).
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

**Status: BUILT.** Slice 1 (the data path + `AT TIME ZONE`) and **Slice 2 (the conversion surface —
`date_trunc`, `EXTRACT`, the cross-family datetime casts, and the now-observable session `TimeZone`
slot — §9)** are landed across all three cores in lockstep, oracle-checked against `postgres:18`. The
settled ground it builds on (UTC-`i64` `timestamptz`, the clock seam, the `TimeZone` slot) is in-tree;
the tz database, the load seam, the TZif reader, `AT TIME ZONE`, and the conversion surface are built.

**Decided (this revision):**

- **Host-loaded, not vendored.** Tz data is loaded via `db.LoadTimeZoneData` from a `JTZ` bundle; the
  bare binary carries no tz data (`UTC` + fixed offsets excepted) — mirroring collation's Slice-3 pivot
  ([collation.md §2/§9](collation.md)). *(Supersedes the earlier vendor-into-each-core proposal.)*
- **`JTZ` bundle wrapping standard TZif** ([../tz/README.md §3](../tz/README.md)) — a manifest +
  per-zone RFC 8536 TZif sections + links, parallel to `JUCD`, no custom compiled-table payload and no
  merge step (§4).
- **Plumbing + consumers.** Slice 1 shipped `AT TIME ZONE` (both directions, §6); Slice 2 shipped
  `date_trunc` / `EXTRACT` / the cross-family casts (§9); the runtime `text → date` cast and
  `date_part` (§9.2) have since landed ([date.md §6](date.md)), and so have the runtime
  `text → timestamp`/`timestamptz` casts and offset-less `timestamptz` input in the session zone on
  every path (§9.3), and session-zone rendering everywhere a `timestamptz` becomes text (§9.5). Still
  deferred: `to_char` and `age`.
  (`make_timestamptz` landed with the make_timestamp slice — [functions.md §11](functions.md).)
- **Persisted dependencies.** Format 32 adds per-index timezone pins for derived keys and partial
  predicates ([index-dependencies.md](index-dependencies.md)); plain values keep their UTC encoding.
- **Session zone drives computation, input, and rendering.** The session `TimeZone` is the zone a
  `timestamptz` is decomposed *in* (§9.4), the zone offset-less input is read in (§9.3), and the zone
  every `timestamptz` → text renders in (§9.5): the `::text` cast, `array → text`, and a result cell
  through the host's session render. The canonical value render stays zone-free (UTC), and EXPLAIN
  keeps that canonical spelling (a documented divergence, §9.5).
- **`date_part` and `julian` have landed** (§9.2). A `date` argument to `date_trunc` / an implicit
  `date`→`timestamp` cast is `42883` (§9.1).

**Resolved at implementation (Slice 1/2):**

1. **POSIX-footer form coverage** (§5) — RESOLVED: ship the near-universal `Mm.w.d` transition rule; the
   rare `Jn` / `n` julian-day forms are a documented follow-on.
2. **The `timestamp AT TIME ZONE zone` DST-ambiguity branch** (§6) — RESOLVED: pinned to PG's gap/overlap
   resolution via the oracle (no deliberate divergence); the Slice-2 `timestamp`→`timestamptz` cast
   inherits it.
3. **Starter zone set & presets** (§13) — RESOLVED: the curated first set shipped; the full IANA set is a
   builder preset (not a separate mechanism).

**Still open (later follow-ons):**

4. **Abbreviation input** (§15 below) — deferred (accept IANA names + fixed `±HH:MM` only); decide later
   whether to load a curated abbreviation table.
5. **When the latent skew machinery lands** (§8) — it activates with functional indexes / STORED
   generated columns; confirm tz reuses [compatibility.md](compatibility.md)'s manifest then, not a
   bespoke mechanism.

## 15. Session zone and abbreviations

- **`TimeZone` / `time_zone`** ([session.md §6.2](session.md)): default `UTC`, capability
  `session.timezone`, corpus directive `# timezone: <zone>`. **Implemented with Slice 2** — it is the
  zone a `timestamptz` is decomposed *in* by `date_trunc` / `EXTRACT` / the cross-family casts (§9.4).
  It is set through the host API (`set_time_zone` / `SessionOptions::time_zone`), validated against the
  loaded set (`UTC` + fixed offsets always; a named zone gated on a loaded bundle providing it, else
  `22023`) and stored as a resolved `ZoneRef`; there is **no SQL `SET TIME ZONE`** yet. Setting it is
  pure session state — no storage effect, fully deterministic given the directive — and it drives
  *computation*, input, and the rendering of every `timestamptz` → text, result cells included through
  the host's session render (§9.5). `AT TIME ZONE` (Slice 1) takes an explicit zone, so it is
  independent of the slot.
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
