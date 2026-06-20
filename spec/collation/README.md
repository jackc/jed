# Collation data formats

> The byte-level formats the collation feature ([../design/collation.md](../design/collation.md))
> consumes and produces, plus the dev fixtures and verification vectors. Three formats are pinned
> here, decided **spec-first before coding** (CLAUDE.md §8 — these are "miserable to retrofit"):
>
> 1. **The definition format** — what `CompileCollation` parses (§1): the Unicode **DUCET
>    `allkeys.txt`** root + **LDML** tailoring rules, the chosen-and-confirmed standards
>    ([../design/collation.md §9](../design/collation.md)).
> 2. **The compiled jed table** — what the UCA executor runs on (§2): a byte-pinned, sorted,
>    self-describing binary table. Cross-core byte-identical by construction (CLAUDE.md §8).
> 3. **The portable `.coll` artifact** — the shippable container (§3): magic + metadata +
>    provenance + the LZ4-compressed table; `SaveCollation`/`OpenCollation` are its
>    writer/reader and `ImportCollation` bakes it into a database
>    ([../design/collation.md §4](../design/collation.md)).
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
> ([../design/collation.md §14](../design/collation.md)). The dev fixture (§5) is a small
> hand-authored subset; the version-pinned real DUCET + curated locale tailorings land with a
> later slice (1f). The verification vectors (§6) are **populated** — produced by the Rust core
> (`impl/rust/src/bin/gen_collation_vectors.rs`) and cross-confirmed byte-for-byte by Go and TS
> (UCA sort keys are not safely hand-authored; §6).

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

The shippable, DB-independent container. A baked catalog snapshot
([../design/collation.md §5](../design/collation.md)) is **these same bytes** in catalog
framing, so a golden DB with a baked collation doubles as an artifact golden, and
`ExportCollation` is a near-copy.

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

**Worked sketch** (dev-root §5; weights illustrative — the exact bytes are pinned as vectors in
1b, §6). `"a"` = CE `[.1C47.0020.0002]`, code point U+0061, §2.4 C-key `61 00 01`:

```
1C47        ‖ 0000 ‖ 0020   ‖ 0000 ‖ 0002   ‖ 0000 ‖ 61 00 01
(L1: a)       (sep)  (L2)     (sep)  (L3)     (sep)  (identical: "a")
```

`"A"` differs only at L3 (`0008` vs `0002`) and at the identical level, so it sorts immediately
after `"a"` — exactly the deterministic-collation "adjacent, not equal" property.

## 5. The minimal dev fixture

A small hand-authored subset to exercise the formats end to end in 1a/1b without the full
~2 MB DUCET. It is **dev data, not the version-pinned real DUCET** — illustrative DUCET-style
weights, enough to show primary order, a secondary (accent) diff, a tertiary (case) diff, and a
tailoring that moves a letter's *primary* position:

- [fixtures/dev-root.allkeys](fixtures/dev-root.allkeys) — `SPACE a A b B z Z ä Ä 😀` with
  DUCET-style weights (`ä`/`Ä` = `a`'s primary + a secondary accent CE, so they sort *near a*;
  `😀` = U+1F600, mapped with a high primary so it sorts last — its purpose is to exercise
  **code-point** iteration, the TS UTF-16 trap §6, not ordering).
- [fixtures/dev-nordic.ldml](fixtures/dev-nordic.ldml) — `&z < ä <<< Ä`, so under this tailoring
  `ä`/`Ä` sort *after z* (the sharp Nordic case that visibly disagrees with the root).

Expected orderings (the bytes are pinned in `vectors/sortkey.toml`, §6; oracle-checkable in a
later slice against `postgres:18` for the locales that map to a real PG collation). Note that the
multi-character a-words sort *within* the a-group, before `b`:

```
dev-root     :  ' ' < a < A < ä < Ä < aa < ab < az < a😀 < b < B < z < Z < 😀   (ä near a, by primary)
dev-nordic   :  ' ' < a < A < b < B < z < Z < ä < Ä < 😀                          (ä after z, by tailoring)
```

## 6. Verification vectors (populated, slice 1b)

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
- A **golden DB** with a baked collation snapshot + a collated index lands in 1d/1e
  (`rust == go == ts == ruby`), doubling as an artifact golden (§3).

## 7. Cost

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
