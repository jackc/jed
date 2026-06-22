# Collation data formats

> The byte-level formats the collation feature ([../design/collation.md](../design/collation.md))
> consumes and produces, plus the dev fixtures and verification vectors. Four formats are pinned
> here, decided **spec-first before coding** (CLAUDE.md §8 — these are "miserable to retrofit"):
>
> 1. **The definition format** — what `CompileCollation` parses (§1): the Unicode **DUCET
>    `allkeys.txt`** root + **LDML** tailoring rules, the chosen-and-confirmed standards
>    ([../design/collation.md §9](../design/collation.md)).
> 2. **The compiled jed table** — what the UCA executor runs on (§2): a byte-pinned, sorted,
>    self-describing binary table. Cross-core byte-identical by construction (CLAUDE.md §8).
> 3. **The portable `.coll` artifact** — the shippable per-collation container (§3): magic + metadata +
>    provenance + the LZ4-compressed table; `SaveCollation`/`OpenCollation` are its writer/reader
>    ([../design/collation.md §4](../design/collation.md)).
> 4. **The `JUCD` bundle** — the runtime-loaded multi-collation container (§5): a manifest + a shared
>    DUCET **root** + per-locale tailoring **deltas** + the Unicode **property/casing** section, what a
>    host hands `db.LoadUnicodeData` (Slice 3, [../design/collation.md §13/§14](../design/collation.md)).
>
> The **sort key** the executor emits (§4) is the byte string whose `memcmp` order *is* the
> collation order — the bridge to jed's `memcmp` storage ([encoding.md §1](../design/encoding.md)).
>
> All multi-byte integers are **big-endian** (jed's on-disk convention; CLAUDE.md §2). Hex is
> written `0xHHHH`. **Status: slices 1b + 1c landed — the `CompileCollation` compiler, the `sortKey`
> executor, and the `SaveCollation`/`OpenCollation` artifact codec** (1b, host-free and
> byte-identical, pinned by the populated `vectors/`), **plus the first SQL surface** (1c: the
> `COLLATE` operator + `ORDER BY … COLLATE`, `db.ImportCollation` in-memory, the `collate` cost
> unit, and the `# load-collation:` corpus directive — `suites/collation/collate.test`) are
> implemented in all three cores (`impl/{rust,go,ts}`)
> ([../design/collation.md §14](../design/collation.md)). **Reference-only pivot (slice 2):** the
> compiled `.coll` artifacts here are **vendored into each core** and read at startup; a collation is
> used by reference (the database file records only its name + version, never the table), and the
> `# load-collation:` directive now declares which vendored collation a test needs rather than
> importing one ([../design/collation.md §2/§9](../design/collation.md)). **Slice 2e landed the real
> version-pinned data:** the production **vendored** set is now the real CLDR-tailored DUCET — `unicode`
> (UCA/UCD **17.0.0**, CLDR 48, [17.0.0/root.allkeys](17.0.0/root.allkeys)) and `es` (root + `&N<ñ<<<Ñ`).
> The dev fixture (§6) is **no longer vendored** — it survives only as the small hand-authored
> compiler/sort-key **vectors** (§7). Implicit weights / the CJK tier-3 root and the broader locale
> tailorings (sv/da/de — needing the deferred LDML features) remain follow-ons. The verification
> vectors (§7) are **populated** — produced by the Rust core
> (`impl/rust/src/bin/gen_collation_vectors.rs`) and cross-confirmed byte-for-byte by Go and TS
> (UCA sort keys are not safely hand-authored; §7). **Slice 3 (host-loaded bundle, proposed —
> [../design/collation.md §14](../design/collation.md)):** the compiled `.coll` tables are packed by a
> builder tool into a **`JUCD` bundle** (§5) a host *loads* at runtime, rather than being compiled into
> each core; the bundle shares one DUCET root across locales (per-locale deltas merged at load, §5.1).

## 1. The definition format (`CompileCollation` input)

A collation **definition** is a *root* (the language-neutral DUCET weights) optionally followed
by one or more *tailorings* (locale diffs). `CompileCollation(name, reader)` reads it and emits
a compiled table (§2). The two parts use the two Unicode standard formats unchanged, so the
definition is reference-comparable and the host-extract path (§ later slice) can target it.

### 1.1 Root weights — DUCET `allkeys.txt` subset

The Unicode Collation Algorithm's Default Unicode Collation Element Table, in the canonical
`allkeys.txt` line format. jed parses this subset:

```
@version 15.1.0                                   # version record (captured into the artifact, §3)
0020  ; [*0209.0020.0002]  # SPACE                 # one mapping: code point(s) ; collation element(s)
0061  ; [.1C47.0020.0002]  # LATIN SMALL LETTER A
0041  ; [.1C47.0020.0008]  # LATIN CAPITAL LETTER A
00E4  ; [.1C47.0020.0002][.0000.0047.0002]  # ä — a's weight + a secondary-only accent CE (expansion)
0063 0068 ; [.1C60.0000.0002][.0000.0000.0002]  # 'ch' contraction (two code points → CEs)
```

- **Line grammar.** `codepoint (' ' codepoint)* ';' element+ ('#' comment)?`. Leading/trailing
  whitespace and blank/`#`-only lines are ignored. A line with **2+ code points** before `;` is
  a **contraction** (the sequence maps as a unit). A mapping with **2+ elements** is an
  **expansion** (one input character produces several collation elements).
- **Collation element** `[` marker `wwww` `.` `wwww` `.` `wwww` `]`:
  - **marker** — `.` = non-ignorable, `*` = **variable** (space/punctuation; the variable bit,
    §2). jed's first slice fixes **non-ignorable** variable weighting
    ([../design/collation.md §6](../design/collation.md)), so the `*` bit is recorded but treated
    as non-ignorable until the *shifted* refinement lands.
  - **wwww.wwww.wwww** — the **L1.L2.L3** weights as 16-bit hex. A `0000` weight is **ignorable**
    at that level (skipped in the sort key, §4). DUCET's 4th (quaternary) weight is not used by
    jed's level model and is ignored if present.
- **Records.** `@version` is captured as the `unicode_version` (§3). `@implicitweights` ranges
  (DUCET's algorithmic weights for unassigned/CJK/Hangul code points) are **out of scope for
  the dev fixture** and land with the real DUCET (1b/follow-on); a definition that needs them
  without declaring them compiles only the explicitly-listed code points.

### 1.2 Tailorings — LDML collation rule subset

A locale diff over the root, in the CLDR LDML `<rules>` syntax. jed parses this subset:

```
&z < ä <<< Ä        # reset at z; ä sorts after z at the PRIMARY level; Ä is a TERTIARY diff after ä
&a << æ             # æ is a SECONDARY diff after a
&V = W              # W sorts identically to V
```

- **`&` reset** sets the anchor (an existing element). Each following **relation** places the
  next element relative to the running position:
  - `<` — **primary** difference (a distinct letter),
  - `<<` — **secondary** (an accent variant),
  - `<<<` — **tertiary** (a case variant),
  - `=` — **identical** (same weight, distinguished only at the identical level).
- The compiler applies the rules over the root to produce a **fully merged** compiled table (root
  + tailoring resolved into final weights); a tailored table carries no separate diff at runtime.
- Deferred LDML features (each a later follow-on): `[before N]`, `&[last regular]`-style logical
  resets, multi-character contractions in rules, `[reorder]`, and the settings block
  (`[strength], [alternate], …`). The first slice supports reset + the four relation strengths
  over single characters.

## 2. The compiled jed collation table (executor input)

The fully-resolved table the executor (§4) runs on — a `Collation`'s inner data
([../design/collation.md §4](../design/collation.md)). Sorted arrays (never hash maps) so the
bytes are deterministic and no iteration order can leak (CLAUDE.md §8):

```
table {
  u8   layout_version          # = 1
  u32  num_singles             # single-code-point mappings, sorted ascending by code point
  u32  num_contractions        # multi-code-point mappings, sorted by code-point sequence
  single[num_singles] {
    u32  codepoint
    u8   ce_count              # ≥ 1; > 1 is an expansion
    ce[ce_count]
  }
  contraction[num_contractions] {
    u8   seq_len               # ≥ 2 code points
    u32  codepoint[seq_len]
    u8   ce_count
    ce[ce_count]
  }
}

ce {                           # one collation element, 7 bytes
  u8   flags                   # bit0 = variable; bits 1-7 reserved (0)
  u16  l1                      # primary weight   (0x0000 = ignorable at L1)
  u16  l2                      # secondary weight (0x0000 = ignorable at L2)
  u16  l3                      # tertiary weight  (0x0000 = ignorable at L3)
}
```

- **Sorting is the contract.** `single[]` is ascending by `codepoint`; `contraction[]` is
  lexicographic by its `codepoint[]` sequence. Both orders are total and implementation-free, so
  every core emits the identical table bytes from the identical definition (CLAUDE.md §8). A
  duplicate code point / sequence in the definition is a compile error.
- **Lookup** (executor, §4): longest-contraction-first at each position, else the single mapping,
  else — when implicit weights land — the algorithmic derivation; until then an unmapped code
  point in the dev slice is a compile-time/`CompileCollation` error rather than a silent
  fallback (made explicit so the dev fixture cannot mask a gap).

## 3. The portable `.coll` artifact (`SaveCollation` / `OpenCollation`)

The shippable, DB-independent **per-collation** container — the unit the build pipeline produces and
the builder tool packs into a `JUCD` bundle (§5). (Under the earlier baked model a catalog snapshot was
these same bytes in catalog framing; since slice 2c the on-disk entry is metadata-only
([../design/collation.md §5](../design/collation.md)) and the table is **loaded from a bundle**, not
baked — so there is no `ImportCollation`/`ExportCollation`.)

```
artifact {
  u8[6]  magic                 # "JCOLL\0"  (0x4A 0x43 0x4F 0x4C 0x4C 0x00)
  u16    format_version        # = 1
  str    name                  # e.g. "en-US"   (str = u16 length + that many UTF-8 bytes)
  str    unicode_version       # e.g. "15.1.0"  (from the @version record, §1.1; "" if none)
  str    cldr_version          # e.g. "45"      ("" if root-only / none)
  str    description           # provenance, e.g. "Go 1.26.3 / Linux 7.1 / ICU 73"  ("" = none)
  u32    content_hash          # CRC-32/IEEE of the *uncompressed* table bytes (§2)
  u32    uncompressed_len      # length of the table bytes (§2) before compression
  u32    compressed_len        # length of the LZ4 block that follows
  u8[compressed_len] table_lz4 # the §2 table, LZ4-block compressed (spec/fileformat/lz4.md)
}
```

- **`content_hash`** is the §3/§4 **stamp**: CRC-32/IEEE (the algorithm jed already ships for
  `format_version` 7 page checksums — [../fileformat/format.md](../fileformat/format.md)), over
  the **uncompressed** table so the identity is independent of the compressor. It is a
  drift/identity stamp, **not** a security hash — on-disk tamper of a *baked* table is separately
  caught by the per-page checksum ([../design/storage.md §6](../design/storage.md)); it may be
  widened later without a behavior change. The **`description` is deliberately excluded** from the
  hash ([../design/collation.md §1/§4](../design/collation.md)), so two artifacts with the same
  table but different provenance strings are the same collation for dedup and the reference-mode
  check.
- **Round-trip is a §8 byte contract.** `OpenCollation` then `SaveCollation` reproduces the input
  bytes exactly on every core; the `description` is preserved verbatim (only *generated*, hence
  host-dependent, by `ExtractHostCollation`) so artifact identity holds cross-core
  ([../design/collation.md §10](../design/collation.md)).

## 4. The sort key (executor output)

The executor maps `(table, string) → sort key`: the byte string whose `memcmp` order equals the
collation's logical order (§ the algorithm is [../design/collation.md §6](../design/collation.md);
this section pins the **bytes**, refining the conceptual sketch in
[../design/collation.md §8](../design/collation.md)).

1. **Collation elements.** Walk the string left to right, at each position taking the longest
   matching contraction else the single mapping, concatenating each match's collation elements
   into one CE sequence (expansions contribute several CEs).
2. **Per-level weight runs.** For each level L1, L2, L3 in order, emit each CE's L-weight as a
   **`u16` big-endian**, **skipping `0x0000` (ignorable) weights** at that level.
3. **Level separators.** Emit a **`0x0000`** (two-byte) separator after each level's run —
   including after L3. Because every emitted weight is `≥ 0x0001`, the separator `0x0000` sorts
   **before** any weight at the same position (`memcmp`), so a string whose level is a prefix of
   another's sorts first — the correct "`a` < `ab`" behavior at every level. Weights and
   separators are all 2 bytes, so the pre-identical portion stays 2-byte aligned and `memcmp`
   effectively compares it weight-by-weight.
4. **Identical level.** Append the **`C`-key of the original string** — the reversible,
   order-preserving UTF-8 text key encoding ([encoding.md §2.4](../design/encoding.md)). This is
   the final tie-break (making the order **total**, so deterministic collations' equality is
   byte-identity — [../design/collation.md §6/§7](../design/collation.md)) and, for a
   `PRIMARY KEY`, what makes the original string recoverable from the key
   ([../design/collation.md §8](../design/collation.md)).

```
sort_key = L1-weights ‖ 0x0000 ‖ L2-weights ‖ 0x0000 ‖ L3-weights ‖ 0x0000 ‖ Ckey(original)
```

**Worked sketch** (dev-root §6; weights illustrative — the exact bytes are pinned as vectors in
1b, §7). `"a"` = CE `[.1C47.0020.0002]`, code point U+0061, §2.4 C-key `61 00 01`:

```
1C47        ‖ 0000 ‖ 0020   ‖ 0000 ‖ 0002   ‖ 0000 ‖ 61 00 01
(L1: a)       (sep)  (L2)     (sep)  (L3)     (sep)  (identical: "a")
```

`"A"` differs only at L3 (`0008` vs `0002`) and at the identical level, so it sorts immediately
after `"a"` — exactly the deterministic-collation "adjacent, not equal" property.

## 5. The `JUCD` bundle (the shippable Unicode-data container)

The container a host **loads** at runtime via `db.LoadUnicodeData`
([../design/collation.md §4](../design/collation.md)) — the production delivery vehicle for the §2
tables and the Unicode property/casing data ([../design/collation.md §16](../design/collation.md)).
**One Unicode version per bundle.** It is a **manifest-indexed container** of independently-addressable
**sections**, so a loader takes only what it needs: a `casing-only` host loads just the property
section; a browser loads the manifest + root, then a locale's delta on demand. It reuses jed's existing
conventions verbatim — big-endian, `str` = `u16` length + UTF-8, CRC-32/IEEE, LZ4-block bodies
([../fileformat/lz4.md](../fileformat/lz4.md)).

```
bundle {
  u8[6]   magic                 # "JUCD\0\0"  (0x4A 0x55 0x43 0x44 0x00 0x00)
  u16     format_version        # = 1
  str     unicode_version       # e.g. "17.0.0"   (the single version axis spanning collation + casing)
  str     cldr_version          # e.g. "48"       ("" if none / a property-only bundle)
  str     description           # builder/provenance identity ("" = none; excluded from section hashes)
  u16     section_count
  manifest[section_count] {     # the table of contents — sections are addressable without reading bodies
    u8    kind                  # 0 = property/casing, 1 = root, 2 = tailoring
    str   name                  # "" for property; the root collation name (e.g. "unicode") for root;
                                # the collation name (e.g. "es") for a tailoring
    u32   content_hash          # CRC-32/IEEE over this section's UNCOMPRESSED payload
    u32   uncompressed_len      # payload length before compression
    u32   compressed_len        # length of this section's LZ4 block in the body region
    u32   body_offset           # byte offset of this section's LZ4 block from the start of the bundle
  }
  u8[...] body                  # the section LZ4 blocks, in manifest order (each: spec/fileformat/lz4.md)
  u32     bundle_crc            # CRC-32/IEEE over everything above (header + manifest + bodies)
}
```

- **At most one `root` section** (the shared DUCET §2 table, stored **once**, and itself a usable
  collation under its manifest name — e.g. `unicode`); **at most one `property` section**; **any number
  of `tailoring` sections**, each naming the collation it defines (merged onto the root at load, §5.1).
  A bundle with only a property section is the `casing-only` preset
  ([../design/collation.md §13](../design/collation.md)).
- **A `tailoring` section is a SPARSE override of the root**, not a full table — this is what lets the
  root be shared. It reuses the §2 `single` / `contraction` records, but each is an **add-or-replace**
  against the root keyed by code point / sequence:

  ```
  tailoring_payload {
    u8    layout_version            # = 1
    u32   num_single_overrides      # §2 `single` records that ADD or REPLACE a root mapping, ascending by code point
    single[num_single_overrides]
    u32   num_contraction_overrides
    contraction[num_contraction_overrides]   # §2 `contraction` records, ascending by sequence
    # (a removal/tombstone form is reserved; the current LDML subset (§1.2) only adds or replaces)
  }
  ```

- **A `root` section payload is the §2 compiled table bytes verbatim.**
- **A `property` section payload** is the Unicode casing table (the first cut ships **case mappings
  only**; normalization is reserved — [../design/collation.md §16](../design/collation.md)):

  ```
  property_payload {
    u8    layout_version            # = 1
    u32   num_simple                # simple case mappings, ascending by code point
    simple[num_simple] {
      u32 codepoint
      u32 upper                     # simple uppercase (== codepoint if identity)
      u32 lower                     # simple lowercase
      u32 title                     # simple titlecase
    }
    u32   num_special               # SpecialCasing (full) entries, ascending by code point
    special[num_special] {
      u32 codepoint
      u8  upper_len; u32 upper[upper_len]
      u8  lower_len; u32 lower[lower_len]
      u8  title_len; u32 title[title_len]
      # (conditional/locale context — final-sigma, Turkish dotted-I — reserved; first cut: unconditional only)
    }
    # (normalization: combining class + decomposition tables — RESERVED, a later property sub-table)
  }
  ```

### 5.1 Load-time merge (`root` + `tailoring` → the §2 table)

Because a tailoring is sparse, `db.LoadUnicodeData` reconstructs the table the executor (§2/§4) expects
by a **deterministic, spec'd merge** — the executor itself is unchanged:

1. Start from the **root** section's `single` / `contraction` maps.
2. Apply each override entry: **replace** the root entry with the same key, or **add** it if absent.
3. **Re-sort** `single[]` ascending by code point and `contraction[]` lexicographically (the §2 total
   order).

The result is **byte-identical to the fully-resolved `<locale>.coll` table** the build produced for
that locale (§3) — so `OpenCollation(merge(root, delta))` equals `OpenCollation(<locale>.coll)`. That
equality is the **merge-identity vector** (§7): `merge(root, es-delta).table == es-full.table`. The
merge is the one new cross-core routine Slice 3 adds; like the executor it is hand-written per core and
byte-pinned by vectors (CLAUDE.md §5/§8).

### 5.2 Why a container with a manifest

One artifact + one load call (distribution), while the manifest gives **selective load** (casing-only;
root + one delta; lazy fetch) and **root-sharing** (the ~0.3 MB root stored once, so a 10-locale bundle
is ~0.4 MB, not ~3 MB — [../design/collation.md §13](../design/collation.md)). This mirrors ICU's single
`.dat` + table-of-contents, the time-zone-bundle precedent, and jed's own "one file" storage ethos —
and keeps casing and collation on one `(unicode_version)` axis so they cannot mismatch.

## 6. The minimal dev fixture

> **Role since slice 2e:** the dev fixtures are **no longer vendored into production** — the real
> version-pinned root (`unicode`) + `es` are ([17.0.0/](17.0.0/), [../design/collation.md §14](../design/collation.md)).
> The dev fixtures survive **only** as the small cross-core compiler/sort-key **vectors** (§7): tiny,
> hand-auditable definitions that pin the compiler + executor (expansion, a tailoring, an astral code
> point) without the full ~2.3 MB DUCET.

A small hand-authored subset to exercise the formats end to end in 1a/1b without the full
~2 MB DUCET. It is **dev data, not the version-pinned real DUCET** — illustrative DUCET-style
weights, enough to show primary order, a secondary (accent) diff, a tertiary (case) diff, and a
tailoring that moves a letter's *primary* position:

- [fixtures/dev-root.allkeys](fixtures/dev-root.allkeys) — `SPACE a A b B z Z ä Ä 😀` with
  DUCET-style weights (`ä`/`Ä` = `a`'s primary + a secondary accent CE, so they sort *near a*;
  `😀` = U+1F600, mapped with a high primary so it sorts last — its purpose is to exercise
  **code-point** iteration, the TS UTF-16 trap §7, not ordering).
- [fixtures/dev-nordic.ldml](fixtures/dev-nordic.ldml) — `&z < ä <<< Ä`, so under this tailoring
  `ä`/`Ä` sort *after z* (the sharp Nordic case that visibly disagrees with the root).

Expected orderings (the bytes are pinned in `vectors/sortkey.toml`, §7; oracle-checkable in a
later slice against `postgres:18` for the locales that map to a real PG collation). Note that the
multi-character a-words sort *within* the a-group, before `b`:

```
dev-root     :  ' ' < a < A < ä < Ä < aa < ab < az < a😀 < b < B < z < Z < 😀   (ä near a, by primary)
dev-nordic   :  ' ' < a < A < b < B < z < Z < ä < Ä < 😀                          (ä after z, by tailoring)
```

## 7. Verification vectors (populated, slice 1b)

The cross-core contract (CLAUDE.md §8), the collation analogue of the
[encoding.md](../design/encoding.md) key vectors. **Populated in 1b**: produced by the Rust core
(`impl/rust/src/bin/gen_collation_vectors.rs`, the source of truth for the case lists) and
cross-confirmed byte-for-byte by Go and TS — **not** hand-authored, because UCA sort keys are
error-prone to compute by hand. Each core's harness (`impl/rust/tests/collation.rs`,
`impl/go/collation_test.go`, `impl/ts/tests/collation.test.ts`) recompiles the `def_files` and
asserts the bytes:

- **`vectors/compiler.toml`** — `(definition fixtures) → (§2 table bytes + §3 artifact bytes)`.
  Pins `CompileCollation` and the `SaveCollation` container; the harness also round-trips each
  artifact through `OpenCollation` (open → re-save reproduces the bytes; the reopened collation
  equals the compiled one). `def_files` are concatenated (newline-joined) then compiled.
- **`vectors/sortkey.toml`** — `(collation, string) → (§4 sort-key bytes)`. The primary executor
  contract; entries are in ascending collation order so the harness also asserts the keys'
  `memcmp` order is strictly increasing. Includes an astral-character case (`😀` U+1F600, the TS
  UTF-16-vs-code-point trap, [types.md §11](../design/types.md)).
- A **golden DB** with a referenced-collation catalog entry + a collated index
  (`rust == go == ts == ruby`) pins the metadata-only on-disk entry and the collated B-tree key bytes
  ([../design/collation.md §5/§10](../design/collation.md)). (Slice 2c shrank the former baked snapshot
  to this metadata-only entry; Slice 3 leaves it byte-for-byte unchanged — delivery moves, the stored
  bytes do not.)
- **`vectors/bundle.toml` (Slice 3a)** — the `JUCD` bundle (§5): `(bundle bytes) → (parsed manifest +
  per-section round-trip)` (`Open`∘`Save` byte-exact on every core) and the **merge identity**
  `merge(root, delta).table == full.table` (§5.1), so the load-time root+delta merge is a cross-core
  byte contract, not per-core code. The existing `compiler.toml` table/artifact bytes are reused as the
  merge target.

## 8. Cost

Sort-key generation adds a **`collate`** unit to the shared cost schedule
([../design/cost.md](../design/cost.md)), charged **per code point** processed (table-bounded
weight lookups; contractions/expansions bounded by the table). **Landed in 1c**, charged at the
**comparison-operator evaluation** site — the deterministic, cross-core-identical metering point:
each ORDERING comparison (`< <= > >=`) under a collation accrues `collate × (codepoints(lhs) +
codepoints(rhs))`, so a collated `WHERE` / `SELECT` comparison over a large input is cost-ceilinged
like any other work (CLAUDE.md §13). `=`/`<>` charge nothing (deterministic-collation equality is
byte-equality, §4). The **`ORDER BY` sort stays unmetered** like every sort
([../design/cost.md §3](../design/cost.md), [../design/spill.md §6](../design/spill.md)) — its input
cardinality is already bounded by the upstream per-row costs, and its decorate sorter builds each
row's sort key exactly once (no `O(n log n)` recompute). Slice 1b's `sortKey` stayed a pure function
with no `Meter`; 1c added the metering at the evaluator, the one place a `Meter` is threaded through.
