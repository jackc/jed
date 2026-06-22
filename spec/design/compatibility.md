# Compatibility & versioning — design PROPOSAL

> ⚠️ **PROPOSAL — NOT A SETTLED SPECIFICATION.** Unlike the other docs in `spec/design/` (which
> record *ratified* decisions the cores implement in lockstep), this is an **unratified design
> proposal** captured from an in-progress discussion so the idea is not lost. **Nothing in it is
> decided, contracted, or built.** It binds no core, it is not part of the conformance contract,
> and every part of it is open to revision or rejection. The current, settled on-disk policy
> remains **clean-break exact-version** ([../fileformat/format.md](../fileformat/format.md)). Read
> this as a sketch of a *possible* future model — not a description of how jed works or has agreed
> to work. The explicitly open decisions are listed in §12; **none of them has been made.**
>
> **The model for "can this binary open this file, and if not, exactly why?"** A jed database
> file's correct interpretation can depend on *versioned or arbitrary computed semantics* —
> a function existing, a function's behavior, a collation version, a stored query, host code.
> Where it does, "any application opens any file" is **not achievable in general** (host code
> is the irreparable case). This doc replaces that impossible guarantee with an achievable one:
> jed **never silently misinterprets a file** — on open it computes a **legible, graded
> compatibility verdict** (full / reduced-read-only / refused-with-a-named-reason) from a
> **requirements manifest** carried in the file. The two levers that make the achievable
> guarantee strong are (a) **value bytes are version-independent** — only *ordering structures*
> and *recomputations* depend on semantics — and (b) **almost every computed-semantics
> dependency is write-time, not read-time**, so reads degrade to a heap-scan fallback instead of
> failing. Collation version skew and built-in-function drift are **the same problem**
> (stored bytes produced by a versioned computation) and get **one** solution here. This doc is
> the cross-cutting contract that [collation.md](collation.md), [timezones.md](timezones.md)
> (time-zone-dependent keys), [constraints.md](constraints.md) (`DEFAULT` / generated columns),
> [indexes.md](indexes.md) (functional indexes), views, and the host-function surface
> ([session.md](session.md), [extensibility.md §2](extensibility.md)) all register into. The structural version layer is [../fileformat/format.md](../fileformat/format.md)
> (`format_version`); the legibility/host-boundary stance is CLAUDE.md §13; the cross-core
> identity requirement is [determinism.md](determinism.md) (CLAUDE.md §2/§8).
>
> **Status: UNRATIFIED PROPOSAL — nothing here is decided or implemented.** This is *not* a
> specification; it is a candidate model recorded for discussion. The **current, actual** on-disk
> policy is a **clean break, exact-version-only** read (a reader accepts *only* the current
> `format_version`, [../fileformat/format.md](../fileformat/format.md) — justified pre-1.0: "we
> own our surface", CLAUDE.md §1) and that remains the spec until and unless this proposal is
> adopted. The doc sketches a model jed *could* adopt if/when it commits to on-disk stability
> (≈1.0). It is written now, ahead of the features that would trigger it (functional indexes,
> generated columns, materialized views, collation versioning), only so that — *should it be
> adopted* — those features can register into a single manifest rather than being retrofitted.
> Adoption is itself an open decision (§12); writing it down is **not** adopting it.

---

## 1. The fundamental tension

A file is "openable anywhere" only as far as everything needed to interpret it is either **in
the file** or **in a version-stable contract every binary implements**. jed's bedrock — page
format, the catalog, scalar value codecs, scalar comparison/NULL semantics — is exactly that
stable contract (it changes only with a `format_version` bump, §13). On that bedrock alone, any
conforming binary reads any file.

The tension begins when correct interpretation depends on **computed semantics that can vary**:

- a **built-in function existing** (a `DEFAULT uuidv7()` written by a newer binary),
- a **built-in function's behavior** (a bug-fixed `lower()` changes a functional index's keys),
- a **collation version** (CLDR/ICU reorders `de` between Unicode releases),
- a **stored query** (a view referencing any of the above, or newer SQL grammar),
- **host-supplied code** (a host function in any of the above).

For the first four, the dependency is *freezable* (bake the data/semantics into the file) or
*re-establishable* (the opening binary supplies it). For **host code it is neither**: you cannot
bake arbitrary application code into a data file, and you could not safely run it if you did (it
would breach the untrusted-query guarantee, CLAUDE.md §13). So the honest conclusion is:

> **"Any application opens any file" is not achievable in general.** It is a fundamental limit,
> not a missing feature.

The rest of this doc is the *achievable* guarantee that replaces it.

## 2. The guarantee jed makes instead

> **jed never silently misinterprets a file.** On open it computes a **deterministic, legible,
> graded compatibility verdict** and either interprets the file correctly, degrades to a
> well-defined reduced mode, or refuses with an error that **names exactly what is missing** —
> *before* it executes anything, never by corrupting data or crashing mid-query.

The verdict has three levels (§7). The machinery is a **requirements manifest** in the file
(§6), checked at open against the capabilities the binary (and host) provide. The guarantee is
*per object*, not per file (§6.3): a missing capability disables the objects that need it, not
the database.

## 3. The unification: collation skew and function drift are one problem

Two things that look unrelated are the same:

- a **functional index** on `lower(name)` stores keys produced by `lower()`;
- a **collated index** on `name COLLATE "de"` stores keys produced by `de`'s sort algorithm.

Both are **stored bytes whose correctness depends on a versioned computation.** If the
computation's behavior changes (a `lower()` bug fix; a CLDR reorder), the stored keys are stale
and the index silently returns wrong answers — the identical failure mode. Likewise, a
`DEFAULT new_fn()` and a `COLLATE "de"` both depend on a named capability *existing* at write
time.

So jed does **not** build a bespoke collation-versioning mechanism and a separate
function-versioning mechanism. It builds **one** discipline — version-tag the semantics, then
**reference-and-degrade** (or, where a value is genuinely stored, freeze) — and collation is its
first instance ([collation.md](collation.md); §10 here). Every built-in function carries a
**semantics version** (bumped only when its output changes); every collation carries its
`(unicode_version, cldr_version)`. Both flow through the same manifest and the same verdict. (As of
collation.md's reference-only pivot, collation does **not** freeze its tables into the file — they
are **vendored into the binary** and the file references the version — making it the cleanest
instance of the reference-and-degrade path; [timezones.md](timezones.md) does the same for tzdata.)

A **time-zone-dependent stored key** is the same instance again: a functional index on
`(ts AT TIME ZONE 'America/New_York')::date` stores keys produced by applying the IANA tzdata, so
the **tzdata version** is the "computation version" and a tzdata bump stales those keys exactly as a
CLDR reorder stales a collated index — while a *plain* `timestamptz` index is immune, because the
stored value is UTC and its order uses no tz rule ([timezones.md §2/§5](timezones.md)). Same
manifest, same verdict.

## 4. The two levers

The achievable guarantee is strong because of two facts about jed's data model.

### 4.1 Value bytes are version-independent

A stored value is just bytes: `text` is UTF-8, an integer is an integer, a `decimal` is
sign+coefficient+scale. **No collation, function, or locale re-encodes a stored value** —
those affect only *key ordering* and *computed results*, never the value on disk. Therefore
**reading the rows of a table is always correct**, on any binary, regardless of semantic
version skew. What can be wrong is never the value; it is (a) the *order* a B-tree imposes and
(b) any value *recomputed on read*.

### 4.2 Almost every computed-semantics dependency is write-time

Walking the surface, the dependency is overwhelmingly needed to **write or maintain**, not to
**read**:

| Feature | Needed to **read** existing rows? | Needed to **write / maintain**? |
|---|---|---|
| `DEFAULT expr` | **No** — evaluated only at INSERT | Yes |
| Functional index | **No** for the base table (heap-scan); only for index-*accelerated* lookup | Yes (maintenance) |
| **STORED** generated column | **No** — the value is on disk | Yes (recompute) |
| **VIRTUAL** generated column | **Yes** — computed on read | Yes |
| Collation-ordered index / PK | **No** — heap-scan + recompute (§8) | Yes |
| Regular (non-materialized) **view** | **Yes** — recomputed every query | — |
| **Materialized** view | **No** — the result is stored data | Yes (`REFRESH`) |
| Host function in any position above | as the row dictates | Yes |

The pattern: combine 4.1 + 4.2 and **reads survive a compatibility gap for nearly the whole
surface** — fall back to "treat every B-tree as an unordered heap, scan it, recompute anything
collation/function-dependent in memory with whatever semantics the binary has" (§8). What is
genuinely *read-breaking* is small and enumerable: **VIRTUAL** generated columns, **regular
views**, and **host functions in a read-required position** (§11).

## 5. The dependency tiers

Every fact a file's interpretation can rest on sits on this ladder, most-portable first:

- **Tier 0 — pure data.** Page format, catalog structure, scalar value codecs. Portable to any
  binary of a compatible `format_version` (§13). The bedrock.
- **Tier 1 — built-in scalar semantics.** Comparison/ordering, NULL/3VL, type codecs. Part of
  the spec contract; stable, changes only with a `format_version` bump. Portable.
- **Tier 2 — versioned reference data.** Collation tables; **IANA time-zone data**
  ([timezones.md](timezones.md)); the Unicode property tables behind `lower`/`normalize`/regex. All
  are **vendored into the binary at a pinned version** and **referenced** by the file (`name` +
  version), never stored in it — the file records the version, the binary carries the data, and skew
  between them is resolved by the graded verdict (§7/§8). Collation's version handling is
  [collation.md §3/§12](collation.md) (its reference-only pivot makes it the worked instance, §10
  here). Tz differs only in that its *base type is already version-independent* (UTC instants,
  [timezones.md §2](timezones.md)), so only *derived* keys reach this tier.
- **Tier 3 — built-in function semantics in stored expressions.** `DEFAULT`, functional indexes,
  generated columns, views that call built-ins. Gated by function *existence* and *semantics
  version*; read-portable per §4 except VIRTUAL/regular-view positions.
- **Tier 4 — host-supplied semantics.** Any stored expression referencing host code. **Not
  portable.** Fails closed and legibly (§11); blast radius contained to the dependent object.

The manifest (§6) records, per object, the highest tier it touches and the specific capability.

## 6. The requirements manifest

Replace the single `format_version` integer with a **structured manifest carried in the file as
data** — the complete, declarative set of capabilities the file's correct interpretation needs.

### 6.1 What it records

Per capability, an entry with:

- **identity** — e.g. built-in function `lower` / collation `de` / host function `geo_distance`
  / SQL feature `window_functions`;
- **kind** — `builtin` (carries a **semantics version**), `collation` (carries
  `(unicode_version, cldr_version)` + content hash), `host` (provided only by a registering host),
  or `structural` (a `format_version` floor);
- **read/write tag** — is this needed to *read* objects that use it, or only to *write/maintain*
  them (§4.2)?
- **referencing objects** — which catalog objects depend on it (so the gate is per-object, §6.3).

### 6.2 The manifest must always be parseable (the bootstrap rule)

To produce a *legible refusal* a binary must be able to read "what does this file need?" even
when the answer to "can I run it?" is no. Therefore the manifest lives in a **fixed,
permanently-forward-compatible framing** (alongside the meta page / `format_version`, never
behind a feature it might itself gate). A binary too old to understand a *newer manifest
encoding* still falls back cleanly to the structural `format_version` refusal — but within a
given manifest-format generation, "what's missing" is always answerable.

### 6.3 Per-object granularity

The check is **"can I use *this object*"**, never "can I open this file." A `DEFAULT
host_fn()` on one table, an unsupported collation on one index, or a view over a missing
built-in disables *that object*; every other table, index, and view stays fully usable. This
per-object gating is what keeps the blast radius local and is the reason a single exotic
dependency never bricks a database.

## 7. The graded-open verdict

On open, jed performs a **capability handshake**: a pure function of `(manifest, capabilities
the binary + host provide)`. Because there is no reference implementation (CLAUDE.md §2), this
function is part of the cross-core contract — **every core computes the identical verdict** for
the same inputs (a §8 / [determinism.md](determinism.md) byte-identity concern; the verdict is
data, not prose). Per object, the verdict is one of:

- **Full** — all required capabilities present → read-write, indexes valid, views runnable.
- **Reduced (read-only)** — the binary lacks only a *write-time* capability (a `DEFAULT`'s
  function; a skewed collation/function version; an unmaintainable functional index). The object
  is **readable** via the heap-scan fallback (§8); **writes and index-acceleration are disabled**
  until a migration re-establishes the capability or rebuilds the structure.
- **Refused (legible)** — the binary lacks a *read-required* capability (§11). The object is
  unavailable; any use raises an **`XX002`-class error that names the missing capability**
  (`requires host function "geo_distance"` / `requires builtin "foo" semantics ≥ 18` / `requires
  collation "de" @ (15.0, 44)`). The *rest* of the database is unaffected (§6.3).

> **`XX002` is reserved in CLAUDE.md §13 ("a file needing host code to reopen fails closed and
> discoverably, `XX002`") but is not yet in [../errors/registry.toml](../errors/registry.toml).**
> Registering it (and deciding whether read-required built-in/collation gaps share it or get
> sibling codes) is part of adopting this model (§12). `XX001` (`data_corrupted`) already exists;
> `0A000` (`feature_not_supported`) is the existing code for an unsupported *construct*.

## 8. Degradation semantics (the heap-scan fallback)

The reduced mode is principled, not a hack: jed **always** has a correct, if slow, execution
strategy — ignore every ordered structure and compute from values.

- **Every B-tree becomes a heap.** Including the primary-key/clustered tree: a full scan visits
  every leaf and reads every row without relying on the order being correct (the order was built
  with a now-distrusted semantic; the *values* are fine, §4.1).
- **Index-accelerated access → full scan.** A seek/range that would use a suspect index instead
  scans + filters in memory.
- **Collation/function-dependent operations recompute in memory** with the binary's current
  semantics (e.g. `ORDER BY name COLLATE "de"` re-sorts the scanned rows). Results are correct
  *for the binary's version* — the best obtainable without the original.
- **Writes are disabled** for the object: a write would have to place a key into the distrusted
  order or evaluate the missing capability.

This is exactly the degradation [collation.md §12](collation.md) adopts for collation version-skew
(its reference-only pivot), generalized to every Tier 2–3 dependency: a binary at a different
vendored `(unicode, cldr)` heap-scans + recomputes rather than trusting a stale collated B-tree.

**Open behavior (§12):** when the required capability is *entirely absent* (not merely a version
mismatch) — e.g. the binary has no `de` at all and the query says `ORDER BY name COLLATE "de"` —
the choice is **error on that clause** vs **fall back to `C`/byte order**. This needs a
deliberate definition; the conservative default is to error (never silently substitute an
ordering the user did not ask for).

## 9. Per-feature placement

How each feature registers into the manifest, with its read/write tag and its failure mode:

- **`DEFAULT expr`** ([constraints.md §2](constraints.md)) — write-time. A missing function ⇒
  table is **read-only** (existing rows read fine; INSERT needing the default fails legibly).
  Constant defaults are folded and depend on nothing.
- **Functional index** (future, [indexes.md](indexes.md)) — write-time for maintenance,
  optional for acceleration. Missing/ drifted function ⇒ index **not maintained, not used for
  acceleration**; base table reads via heap-scan; rebuild on re-establishment.
- **Time-zone-dependent functional index / key** (future, [timezones.md §5](timezones.md)) —
  write-time; a tzdata-version bump stales keys derived via `AT TIME ZONE 'const'` /
  `date_trunc(…, 'zone')`. Same Tier-2 treatment as collation: pin one tzdata version per file,
  version-stamp, degrade to heap-scan + rebuild on migration. Plain `timestamptz` keys are immune
  (UTC, tz-free order), so only zone-derived keys register here.
- **Generated column** (future, [constraints.md](constraints.md)) — **STORED** is write-time
  (value on disk, read-portable); **VIRTUAL** is read-time (computed on read, *not* portable —
  §11). Recommend **STORED-only** (§12).
- **Collation-ordered key** (key encoding [collation.md §8](collation.md); versioning §12) —
  write-time; the worked instance of Tier 2.
- **Regular view** — read-time (recomputed every query). A definition over a missing semantic ⇒
  the **view is refused** (the rest of the file is fine). No stored value to degrade to. *But*
  regular views carry **no corruption hazard** — re-evaluation always uses current semantics, so
  there are no stale stored bytes (the safe axis; only availability is unforgiving). Store the
  **resolved** definition, not raw SQL, so the dependency set is explicit and an old binary need
  not parse newer grammar to discover it cannot run the view.
- **Materialized view** (future) — read-portable (stored result is Tier-0 data); only `REFRESH`
  is write-time. The escape hatch when a view's *data* must survive on a binary that cannot run
  its definition.
- **Host function** anywhere ([session.md](session.md), [extensibility.md §2](extensibility.md))
  — marked `host` in the manifest. Write-only position ⇒ read-only object; read-required position
  ⇒ refused, named. The host that wrote it owns the consequence (CLAUDE.md §13).

## 10. Collation as the worked instance

Collation is Tier 2 and the first thing to exercise this model. The decisions reached for it
(detailed in [collation.md](collation.md)) are the template:

- **One Unicode version pinned per file.** The file records `(unicode_version, cldr_version)`;
  *all* its collations are that version, so two columns both `COLLATE "de"` can never disagree.
  Changing a file's version is a deliberate **migration** (rebuild collated indexes), never an
  accident of which binary touched it when.
- **Reference-only, with degradation (settled).** [collation.md](collation.md) §3 has the file
  **reference** its collations by `name` + version and **never store the table**; the table itself is
  **loaded from a host-supplied `JUCD` bundle** (collation.md §9 — Slice 3 supersedes the slice-2
  "vendor into the binary at a footprint tier" delivery, but the reference-only *on-disk* posture is
  unchanged). A binary with a loaded bundle at the file's pinned version reads-writes fully; with a
  bundle at a different version (or none providing the collation) it **degrades to read-only
  heap-scan** (§8) or refuses legibly — never silently re-orders. This is the clean Tier-2 instance:
  no freeze path, no host-reimport hash, just reference + the graded verdict.
- **Delivery is decided: a host-loaded bundle (Slice 3).** Storing tables per-file would not shrink
  the distribution — it would only duplicate data and add a cross-version-skew hazard — so collation is
  **referenced from the file, never baked**, and the table is delivered in a bundle the host loads
  (the footprint is the deployer's choice, not the build's; collation.md §13). The universal Unicode
  **property/casing** tables ride the **same bundle** on the same `(unicode_version)` axis (collation.md
  §16), so casing and collation share one version pin. ([timezones.md](timezones.md) vendors tzdata for
  the same determinism reason; whether it too becomes loadable is an open follow-on.) This resolves the
  former open decision (§12).
- **Casing is the same instance.** A functional index on `lower(x)` or a `GENERATED ALWAYS AS
  (lower(x))` column stores a versioned-casing result — including the **ASCII-baseline vs. Unicode-`X`
  regime** distinction (collation.md §16) — so it registers into this manifest exactly like a collated
  index (write-time dependency, §9), degrading to the heap-scan verdict on a regime/version change
  rather than silently re-folding.

## 11. The honest hard walls

Named explicitly, because pretending they don't exist is the failure mode this whole doc avoids:

1. **Host functions in a read-required position** — a regular view, or a VIRTUAL generated
   column, whose definition calls host code. No binary lacking that code can produce the value.
   **Refused, legibly** (`XX002`-class, function named).
2. **VIRTUAL generated columns / regular views over any missing semantic** — read-required by
   construction (no stored value). Refused when the semantic is absent.

Everything else degrades to a correct read. Two restrictions buy back most read-portability
cheaply (§12): **STORED-only generated columns**, and **marking host-provided semantics
distinctly** from built-ins so the gate can tell "needs a newer jed" from "needs the host's
code."

## 12. Status & open decisions

**Status: design only.** Nothing here is built; the live policy is clean-break exact-version
([../fileformat/format.md](../fileformat/format.md)). This doc is adopted incrementally as the
triggering features land.

**Decided in principle (this design):**

- Replace "opens anywhere" with the **graded, legible, per-object verdict** (§2/§7).
- **One requirements manifest**, read/write-tagged, generalizing `format_version` (§6).
- **Collation skew = function drift = one versioning discipline** (§3); semantics-version every
  built-in; pin one Unicode version per file (§10).
- **Heap-scan read degradation** as the universal reduced mode (§8).
- **Collation is vendored + reference-only, never baked** (§10) — settled in
  [collation.md §3/§12](collation.md); the file references collations by `name` + `(unicode, cldr)`
  version and the binary carries the (tiered) tables. tzdata follows the same shape
  ([timezones.md](timezones.md)).
- Lean toward **STORED-only generated columns** and **distinct host-vs-builtin marking** (§11).

**Open (need a deliberate call before/at adoption):**

1. **When to adopt** — this is the ≈1.0 on-disk-stability commitment; until then, clean-break is
   simpler and owes nothing (CLAUDE.md §1).
2. **Whether to vendor universal Unicode property tables now** (forced by `normalize`/`lower`/regex)
   on the same one-version-per-binary axis as collation (§10) — the *bake-vs-vendor* question is
   settled (vendor); this is the remaining timing call.
3. **`ORDER BY … COLLATE x` with `x` entirely absent** (§8) — error vs `C`-fallback.
4. **Error-code shape** (§7) — register `XX002`; decide whether read-required built-in/collation
   gaps share it or get siblings; relation to `0A000`.
5. **Manifest encoding & location** (§6.2) — the permanently-parseable framing, and how a
   too-old binary degrades to the structural refusal.

**Deferred features that will register here when built:** functional indexes
([indexes.md](indexes.md)), generated columns ([constraints.md](constraints.md)), views &
materialized views, the host-function catalog ([session.md](session.md)), and **time-zone-dependent
keys** ([timezones.md](timezones.md) — the IANA tzdata version as a Tier-2 reference-data capability).
