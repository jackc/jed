# Time-zone data formats

> The byte-level formats the time-zone feature ([../design/timezones.md](../design/timezones.md))
> consumes and produces, plus the committed source data and verification vectors. Two formats are
> pinned here, decided **spec-first before coding** (CLAUDE.md §8 — "miserable to retrofit"):
>
> 1. **The TZif subset** jed reads (§2) — the standardized [RFC 8536](https://datatracker.ietf.org/doc/html/rfc8536)
>    Time Zone Information Format, the compiled per-zone form `zic` emits and PostgreSQL/glibc/Go read.
>    jed reads it, never writes it (no `zic`), so cross-core agreement is **by construction**
>    ([../design/timezones.md §3.4](../design/timezones.md)).
> 2. **The `JTZ` bundle** (§3) — the runtime-loaded multi-zone container: a manifest + per-zone TZif
>    sections + alias links, what a host hands `db.LoadTimeZoneData` ([../design/timezones.md §4](../design/timezones.md)).
>
> The reader's output (§4) is `(zone, instant) → (utc_offset_seconds, abbrev, is_dst)` — the local-time
> type in effect at an instant, the bridge from a UTC `i64` to a wall-clock reading.
>
> All multi-byte integers in the **`JTZ` wrapper** are **big-endian** (jed's on-disk convention;
> CLAUDE.md §2); hex is `0xHH`. **Within a TZif section the byte order is TZif's own — big-endian per
> RFC 8536** (which happens to agree with jed's convention). `str` = `u16` length + that many UTF-8
> bytes; LZ4-block bodies are [../fileformat/lz4.md](../fileformat/lz4.md); CRC is CRC-32/IEEE (the
> algorithm jed already ships for `format_version` 7 page checksums). **Status: design decided, not yet
> built** — this README is the format contract the first slice ([../design/timezones.md §14](../design/timezones.md))
> will implement in lockstep across the three cores.

## 1. Why wrap TZif rather than define a compiled table

Collation could not wrap a standard form — there is no portable "compiled collation table" standard, so
jed defines its own (`.coll`, [../collation/README.md §2](../collation/README.md)) and pays for a
hand-written compiler + a root+delta merge. Time zones **have** a portable compiled standard (TZif), and
independent readers of the same TZif bytes agree by construction ([../design/timezones.md §3.4](../design/timezones.md)),
so jed wraps it unchanged: a `JTZ` zone section's payload **is** a complete TZif file, verbatim. This
removes the two hardest pieces of the collation pipeline — there is **no jed-defined compiled payload**
and **no load-time merge** (a zone section is self-contained). The only hand-written per-core code is the
**reader** (§4) — and it reads a standardized layout, so it is pinned by `(input, output)` vectors (§6)
rather than by a byte-identity-of-our-own-encoding contract.

## 2. The TZif subset jed reads (RFC 8536)

A TZif file is: a **v1 header + v1 data block** (32-bit times, kept for legacy readers), then — for
version `'2'` / `'3'` — a **second header + v2+ data block** (64-bit times), then a **footer** (a POSIX
TZ string). jed reads the **v2+ block and the footer** and ignores the v1 block (32-bit times overflow
in 2038; the v2+ block is a superset). A version-`'\0'` (v1-only) file is accepted by reading its single
32-bit block with no footer (a legacy/edge case; the committed source is v2+).

### 2.1 Header (44 bytes)

```
header {
  u8[4]  magic            # "TZif" (0x54 0x5A 0x69 0x66)
  u8     version          # 0x00 ('\0'), 0x32 ('2'), or 0x33 ('3')
  u8[15] reserved         # must be zero; ignored
  u32    isutcnt          # count of UT/local indicators      (jed: skipped)
  u32    isstdcnt         # count of standard/wall indicators (jed: skipped)
  u32    leapcnt          # count of leap-second records      (jed: skipped)
  u32    timecnt          # count of transition times
  u32    typecnt          # count of local time-type records (≥ 1)
  u32    charcnt          # length of the time-zone designation byte block
}
```

For a v2+ file the **first** header's block is the 32-bit one jed skips; jed re-reads a second 44-byte
header (same layout, `version` repeated) immediately after the v1 data block, then reads the v2+ block
below using the second header's counts.

### 2.2 Data block (v2+, all integers big-endian)

```
data_block {
  i64    transition_time[timecnt]    # UT seconds of each transition, strictly ascending
  u8     transition_type[timecnt]    # index into local_time_type[] in effect AT/AFTER each transition
  local_time_type[typecnt] {
    i32  utoff                       # seconds to ADD to UT to get local time (east-positive)
    u8   isdst                       # 0 = standard, 1 = daylight
    u8   desigidx                    # byte offset into designations[] of this type's abbreviation
  }
  u8     designations[charcnt]       # NUL-terminated abbreviation strings; desigidx points into here
  leap_second[leapcnt]    { i64 occur; i32 corr }   # jed: read past, ignored
  u8     std_wall[isstdcnt]                          # jed: read past, ignored
  u8     ut_local[isutcnt]                           # jed: read past, ignored
}
```

- **`utoff` is east-positive** (the opposite of POSIX's west-positive footer offset, §5 — the reader
  normalizes both to east-positive).
- **`desigidx`** points at the *start* of a NUL-terminated abbreviation within `designations[]`; one
  designation string may be shared by several types (`desigidx` of the substring).
- **Leap seconds / std-wall / UT-local indicators are read past to reach the footer but otherwise
  ignored** — they do not participate in `AT TIME ZONE` (PG ignores them here too). Recording this
  explicitly so the reader cannot silently mis-handle them.

### 2.3 Footer (v2+ only)

```
footer = 0x0A  TZ-string  0x0A          # a newline, the POSIX TZ string, a newline
```

The `TZ-string` is a POSIX TZ specification (§5) governing instants **at or after the last transition**
(`transition_time[timecnt-1]`). An empty footer (`\n\n`) means "no rule beyond the table" — the last
transition's type continues indefinitely. The footer is the analogue of collation's tailoring resolution
— the one piece of TZif that is computed, not table-looked-up — and is pinned by vectors (§6).

## 3. The `JTZ` bundle

The container a host **loads** at runtime via `db.LoadTimeZoneData`
([../design/timezones.md §4](../design/timezones.md)). **One tzdata version per bundle.** A
manifest-indexed container of independently-addressable sections (a loader takes only what it needs),
reusing jed's conventions verbatim — big-endian wrapper, `str` = `u16` len + UTF-8, CRC-32/IEEE, LZ4
bodies — exactly as `JUCD` ([../collation/README.md §5](../collation/README.md)). Produced by the
build-time builder `impl/rust/src/bin/build_timezone_bundle.rs`
([../design/timezones.md §12](../design/timezones.md)); the canonical production bundle is
`fixtures/tzdata.jtz`.

```
bundle {
  u8[6]   magic                 # "JTZ\0\0\0"  (0x4A 0x54 0x5A 0x00 0x00 0x00)
  u16     format_version        # = 1
  str     tzdata_version        # e.g. "2025b"   (the single IANA-release version axis)
  str     description           # builder/provenance identity ("" = none; excluded from section hashes)
  u16     zone_count            # number of zone (TZif) sections
  u16     link_count            # number of alias links (no body)
  zone_manifest[zone_count] {   # the table of contents — sections addressable without reading bodies
    str   name                  # IANA zone name, e.g. "America/New_York"  (ascending by UTF-8 bytes)
    u32   content_hash          # CRC-32/IEEE over this section's UNCOMPRESSED TZif payload
    u32   uncompressed_len      # TZif payload length before compression
    u32   compressed_len        # length of this section's LZ4 block in the body region
    u32   body_offset           # byte offset of this section's LZ4 block from the start of the bundle
  }
  link[link_count] {            # alias → target, ascending by alias UTF-8 bytes
    str   alias                 # e.g. "US/Eastern"
    str   target                # e.g. "America/New_York"  (must be a zone[] name in THIS bundle)
  }
  u8[...] body                  # the zone TZif LZ4 blocks, in zone_manifest order (each: ../fileformat/lz4.md)
  u32     bundle_crc            # CRC-32/IEEE over everything above (header + manifest + links + bodies)
}
```

- **A `zone` section payload is a complete RFC 8536 TZif file (§2), verbatim** — no jed re-encoding. The
  `content_hash` is over the uncompressed TZif bytes (a drift/identity stamp, not a security hash — the
  bundle is loaded from bytes the host vouches for; `bundle_crc` catches truncation/transposition).
- **`zone_manifest[]` is ascending by zone name** and **`link[]` ascending by alias**, both by UTF-8
  byte order — total, implementation-free orders, so every core (and the builder) emits the identical
  bundle bytes from the identical input (CLAUDE.md §8). A duplicate zone name or alias is a build error.
- **A `link`** is an alias resolved at load: looking up `alias` uses `target`'s TZif. `target` must be a
  zone in the same bundle (a dangling link is a build error). Links carry **no body**.
- **`UTC` and fixed `±HH:MM` offsets are never in a bundle** — they are built-in and table-free
  ([../design/timezones.md §3.2](../design/timezones.md)). A bundle may still ship `Etc/UTC` as an
  ordinary named zone for completeness; the engine treats the *built-in* `UTC` / numeric offsets without
  consulting any section.
- **Round-trip is a §8 byte contract.** `open_bundle` then `save_bundle` reproduces the input bundle
  bytes exactly on every core (the `JUCD` precedent, [../collation/README.md §5](../collation/README.md)).

### 3.1 Why a container with a manifest

One artifact + one load call (distribution), while the manifest gives **selective load** (fetch the
manifest, then a zone on demand — a browser need not pull all ~2 MB) and keeps every zone on **one
`tzdata_version`** axis so two zones in a file can never silently disagree. This mirrors `JUCD`, ICU's
`.dat` table-of-contents, and jed's own "one file" storage ethos. Unlike `JUCD` there is **no shared
root and no merge** (§1) — each TZif section stands alone.

## 4. The reader (zone TZif → offset)

Each core implements one hand-written reader (CLAUDE.md §5 forbids codegenning it). Its contract:

> **`offset_at(zone_tzif, instant_seconds) → (utc_offset_seconds, abbrev, is_dst)`**

where `instant_seconds = floor_div(instant_micros, 1_000_000)` (the `timestamptz` micros reduced to UT
seconds; sub-second precision never affects which transition is in effect). The lookup:

1. **Empty table** (`timecnt == 0`): if a footer rule exists, evaluate it (§5) for `instant_seconds`;
   else use the first standard type (`isdst == 0`, lowest index), or `local_time_type[0]` if all types
   are DST.
2. **Before the first transition** (`instant_seconds < transition_time[0]`): use the first standard type
   (the RFC 8536 rule), falling back to `local_time_type[0]` if no standard type exists.
3. **Within the table**: binary-search for the largest `i` with `transition_time[i] ≤ instant_seconds`;
   the type is `local_time_type[transition_type[i]]`.
4. **At or after the last transition** (`instant_seconds ≥ transition_time[timecnt-1]`): if a non-empty
   footer rule exists, evaluate it (§5); else use the last transition's type (step 3 with
   `i = timecnt-1`).

The result's `abbrev` is the NUL-terminated string at `designations[desigidx]`. `AT TIME ZONE` then:

- **`timestamptz AT TIME ZONE zone`** → `timestamp` = `instant_micros + utc_offset_seconds × 1_000_000`.
- **`timestamp AT TIME ZONE zone`** → `timestamptz`: find the offset `o` such that the local reading maps
  back to UT (`utc = local_micros − o`). At a DST gap/overlap the answer is non-unique; jed resolves it
  to **PostgreSQL's branch** (oracle-pinned, [../design/timezones.md §6](../design/timezones.md)).

## 5. The POSIX TZ footer rule (§2.3 evaluation)

The footer governs instants past the last transition (§4 steps 1/4). Grammar (POSIX; RFC 8536 §3.3.1):

```
std offset [ dst [ offset ] [ , start [ /time ] , end [ /time ] ] ]
```

- **`std` / `dst`** — abbreviations: either 3+ letters, or a quoted `<...>` form (allowing `+`/`-`/digits,
  e.g. `<+0530>`).
- **`offset`** — `[+|-]hh[:mm[:ss]]`, the time to **add to local to get UT** (POSIX **west-positive**, the
  opposite sign of TZif `utoff` §2.2; the reader negates it to east-positive). `dst`'s offset defaults to
  `std`'s offset **− 1 hour** when omitted.
- **`start` / `end`** — the DST start/end rules. Three forms; **the first cut supports `Mm.w.d` only**
  (the form essentially every real zone uses), with `Jn` / `n` a documented follow-on
  ([../design/timezones.md §14](../design/timezones.md)):
  - **`Mm.w.d`** — month `m` (1–12), week `w` (1–5, where 5 = "last"), day `d` (0–6, 0 = Sunday).
  - *(follow-on)* `Jn` (1–365, Feb 29 never counted) and `n` (0–365, Feb 29 counted).
- **`/time`** — local time of the transition, `[+|-]hh[:mm[:ss]]`, default `02:00:00`. (POSIX allows hours
  beyond 0–24; supported.)

**Evaluation** for `instant_seconds`: compute the civil year containing the instant (in UT, adequate
since transitions are mid-year), resolve `start`/`end` to absolute UT instants for that year (a `Mm.w.d`
date at `/time` local, converted to UT using the offset in effect *just before* the transition), and
choose `dst` when the instant is in the daylight interval (handling southern-hemisphere zones where
`start > end`), else `std`. The interval test and the year-boundary handling are pinned by vectors (§6) —
this is the one computed, divergence-prone sub-routine, so it is the primary reader vector target. A
footer with no `,start,end` (a fixed-offset footer like `<-03>3`) is a constant offset for all instants
past the table.

## 6. Verification vectors

The cross-core contract (CLAUDE.md §8), the tz analogue of the [../collation/README.md §7](../collation/README.md)
vectors. Produced by the Rust core (`impl/rust/src/bin/gen_timezone_vectors.rs`,
[../design/timezones.md §12](../design/timezones.md)) and cross-confirmed byte-for-byte by Go and TS:

- **`vectors/tzif.toml`** — `(zone, instant_micros) → (utc_offset_seconds, abbrev, is_dst)`. The primary
  reader contract (§4). Cases **must** include: a standard-time instant, a daylight instant, an instant
  exactly at a transition boundary (the `≤` edge), an instant **before the first transition**, an instant
  **past the last transition** (exercising the §5 footer), a non-hour offset (`Asia/Kolkata`, `+05:30`), a
  sub-hour DST step (`Australia/Lord_Howe`), a southern-hemisphere `start > end` footer, and an alias
  (`US/Eastern` resolving to `America/New_York`).
- **`vectors/bundle.toml`** — `(bundle bytes) → (parsed manifest + per-section round-trip)`: the
  `open_bundle`∘`save_bundle` byte-exact identity (§3) and the resolved link table.

There is **no on-disk golden DB vector** for tz — tz writes no on-disk bytes
([../design/timezones.md §2/§11](../design/timezones.md)); that vector returns only if/when tz-derived
stored keys land (the §8 latent path).

## 7. The committed source and the starter set

`<tzdata_version>/` holds the **committed TZif blobs** the builder packs (committed, not `zic`-generated
per build — [../design/timezones.md §3.4](../design/timezones.md)) plus the links list, and the pinned
`tzdata_version`. The first production `fixtures/tzdata.jtz` ships a small curated set chosen to exercise
the §6 corners — `Etc/UTC`, `America/New_York`, `Europe/Paris`, `Asia/Kolkata`, `Australia/Lord_Howe`,
and a future-transition case — with `US/Eastern → America/New_York` as the link case. The full ~600-entry
IANA set is a builder **preset**, a follow-on, not a format change.

## 8. Cost

`AT TIME ZONE` evaluation adds a **`timezone`** unit to the shared cost schedule
([../design/cost.md](../design/cost.md), [../cost/schedule.toml](../cost/schedule.toml)), charged **once
per conversion** at the operator's evaluation site — the deterministic, cross-core-identical metering
point (the `collate` precedent, [../collation/README.md §8](../collation/README.md)). A lookup is a
bounded binary search plus a bounded footer evaluation, so a query converting a large input is
cost-ceilinged like any other work (CLAUDE.md §13). The unit's data row lands with the slice's accrual
site (kept in lockstep — CLAUDE.md §5).
