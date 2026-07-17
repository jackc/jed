# Host extensibility — design

> How a host application extends the engine with its own **functions**, **composite types**,
> and (where reasonable) **core scalar types** — and how that is done *without* dissolving the
> commitments that make jed jed: cross-core byte-identity (CLAUDE.md §2/§8), determinism
> (CLAUDE.md §10, [determinism.md](determinism.md)), the deterministic cost bound (CLAUDE.md
> §13, [cost.md](cost.md)), and "boring, explicit code over clever abstraction" (CLAUDE.md §10).
>
> The organizing idea — proposed by the maintainer and adopted here — is a **PostgreSQL-style
> catalog type system**: every type, built-in or host-supplied, is a catalog entry that names
> the functions for its text format, binary format, and sort order. That **blurs the line**
> between system and extension types at the *interface*. This doc's central job is to show that
> the line does not vanish — it **moves**, from "type machinery" (where it can and should
> disappear) to "**who owns the cross-core determinism contract**" (where it is load-bearing and
> must stay). §2 is that argument; everything else follows from it.
>
> **Status: a framework + proposal — with the §5 dispatch foundation built, composite types landed,
> and the host-function injection seam (§14 step 3) shipped.** Three pieces are real: (a) resolution
> for the **built-in** named scalar functions and aggregates is data-driven over the generated catalog
> tables (§5, all three cores, behaviour-preserving); (b) **composite (row) types have shipped** as a
> landed feature ([composite.md](composite.md), `format_version` 9 — the *open type* pivot §3 rests
> on); (c) **host scalar functions over existing types have shipped** (§4.2 / §14 step 3, all three
> cores): a host registers scalar functions into a frozen `ExtensionRegistry` at open/create and they
> resolve + evaluate through a registry dispatch arm beside the built-in one — the function *seam* of
> §5.1, ephemeral (no persisted use). **Everything else here remains a proposal**: the `TypeExpr`
> model (§3), host scalar types (§4.3),
> the persisted host-type catalog (§6), the extension registration surface (§7), the host-code index
> connection (§8), host-code cost metering (§9), the host determinism ledger (§10), the graded
> missing-on-reopen verdict (§11), and the host-extension conformance harness (§12). The doc defines
> the target shape, ties it to the existing determinism/cost/conformance contracts, and records the
> open forks (§13) for the maintainer. It is the Phase 9 design referenced by [TODO.md](../../TODO.md).
> When a section is ratified, make the downstream edits in §15 in the same change.
>
> **This revision (2026-07-17) reconciles the doc against current reality and an adversarial design
> review.** The material changes from the previous draft: the `TypeExpr` type model (§3) replaces
> the loose "in-memory `Type` gains arms" framing; the capability ladder splits **Equatable from
> Ordered** and drops the host key *decoder* (§3/§4.3); the extension `type_code` is corrected from
> the stale `14` (which collides with the landed composite type) to the **next free `21`** (§6);
> **component identity** becomes a five-field version tuple, not one "version" (§6/§7); host function
> collision is **signature-level, not name-level** (§4.2); the index connection gains
> **resolved-dependency persistence** (§8, a soundness fix) and the **opclass registry** (§8.2);
> the blunt `XX002`-brick reopen failure is replaced by the **graded per-object verdict** already
> designed in [compatibility.md](compatibility.md) (§11); and cost is stated as **cooperative
> accounting, not a sandbox** (§9). §5 now separates the dispatch **seam** (add an arm — the host
> prerequisite) from the **dogfood** (remove built-in arms — optional, benchmark-gated) and sequences
> the **function seam before the type seam** (§5.1/§14).

> **Governing principle — the host-extension boundary ([CLAUDE.md](../../CLAUDE.md) §13).** Read
> this whole doc through one lens: jed's fundamental guarantees (cross-core byte-identity, self-describing
> files, single-file storage, transaction boundaries, untrusted-query safety) bind **jed's own engine
> and built-in surface**, not host extensions. A host may relax any of them for its own extension as
> it judges appropriate and **owns the consequences** — the whole point is to *enable* PostGIS-class
> extensibility, not to police it. So wherever this doc reads as jed *enforcing* a guarantee on a host
> — the cost gate (§9), the graded reopen verdict (§11), taint containment (§10), the conformance
> harness (§12) — read it as a **safe default plus an opt-in tool that protects jed's *core*
> guarantees and that the host may relax**, never a mandate jed imposes on what the host builds.
> jed's job is the stable foundation and a clean seam; the freedom above it is the host's. §2 (the
> line that *moves*, not vanishes) is this principle in full.

---

## 1. The frame — jed already has the pg_type vtable, inlined as match-arms

PostgreSQL's catalog matches each type with functions for its text format (`typinput` /
`typoutput`), its binary format (`typreceive` / `typsend`), and its sort order (the default
btree opclass), plus typmod handling for parameterized types. jed has **every one of these
pieces already** — they are just expressed as hand-written `match` arms scattered across each
core rather than named as one interface and reached through indirection:

| pg_type slot | jed's existing piece | Where it lives today |
|---|---|---|
| `typinput` / `typoutput` | literal parser + renderer (the `I`/`B`/`D`/`T`/`R` render tags) | hand-written per core; tag in [conformance.md](conformance.md) §1 |
| `typreceive` / `typsend` | the **value codec** (1-byte presence tag + per-type body) | spec'd in [../fileformat/format.md](../fileformat/format.md) §"Value codec"; hand-written per core |
| btree opclass (sort) | the **comparator** + the order-preserving **key encoding** | `encoding.method` names it in [../types/scalars.toml](../types/scalars.toml); kernel hand-written per core, vectors in [../encoding/](../encoding/) |
| `typmodin` / `typmodout` | the `numeric(p,s)` / `varchar(n)` typmod stored in the catalog column entry | [../fileformat/format.md](../fileformat/format.md) catalog section |
| canonicalize | decimal/interval value-canonicalization, NaN-on-store ([float.md](float.md) §10) | hand-written per core |

So [../types/scalars.toml](../types/scalars.toml) is **already a pg_type-style catalog**: each
`[[type]]` row carries `id`, `aliases`, `family`, `storable`, an `encoding.method` *name*, a
`collation`, a render tag, and parameterization caps. The data-over-code split (CLAUDE.md §5) is
already drawn — the *descriptor* is shared data, the *method bodies* are hand-written per core
(this is the same boundary [codegen.md](codegen.md) draws for the function catalog).

**The one thing missing is the indirection — and today there is none for types.** The method
*name* (`"uuid-raw16"`, `"int-be-signflip"`) resolves to a kernel by a hardcoded `match` on the
in-memory type. Concretely (a code audit of the Rust core, mirrored in Go/TS): `ScalarType` is a
**closed 17-variant enum** and adding a scalar ripples through ~15 exhaustive matches (the value
codec encode/decode, the `type_code` ↔ scalar pair, the `eq3`/`lt3`/`gt3`/`render` comparators,
the order-preserving key encoder, `width_bytes`/`is_fixed_width`/`rank`, the per-family
predicates); there is **no** type registry. Extensibility is the act of routing that resolution
through a **registry** the host can also populate — exactly PG's `pg_type` → `pg_proc`
indirection. Once a type is "a catalog entry whose methods resolve through a registry," a
host-supplied type and a built-in type are the **same kind of thing** to the engine. That is the
blur the maintainer wants; §5 shows it is already the reality for *functions* and a small
conceptual step for *types*.

---

## 2. The line that moves, not vanishes — who owns cross-core determinism

Here is the qualification that governs the whole design. In PostgreSQL there is **one**
implementation, so a type's `typsend` *is* the byte format, definitionally. jed's identity is
the opposite (CLAUDE.md §2): **G2 — `rust == go == ts == ruby`, byte-for-byte** — is the
honesty mechanism, and there is no reference implementation to defer to. The four guarantees
([determinism.md](determinism.md) §1) decide everything below:

- **Built-in type / function.** *jed* owns G2/G3: the spec + the conformance corpus + the byte
  fixtures force every core to agree. This is non-negotiable and unchanged.
- **Host-defined composite type.** G2/G3 are preserved **by construction** — the codec is
  *derived* from fields that are already cross-core-identical (§4.1). The host writes no codec.
- **Host-defined core (scalar) type.** The host supplies the codec **in each core's language** —
  a Go `encode` and a Rust `encode`, possibly by different authors. jed **cannot** enforce that
  they agree. The G2/G3 obligation **transfers to the host**.

So the system/extension line does not disappear; it relocates from the type machinery to the
**ownership of the determinism contract**:

> Built-in: jed owns G2/G3. Host composite: G2/G3 hold by construction. Host scalar: **the host
> owns G2/G3** — jed offers the harness (§12) as an *opt-in tool* to prove it cross-core, and
> containment (§10) as a *safe default* that keeps an undischarged relaxation from silently reaching
> jed's *own* guarantees. Neither is a mandate on the host: a single-core host need not run the
> harness, and a host may relax containment for its own surface and own the result (CLAUDE.md §13,
> the host-extension boundary).

**The mitigation that makes this livable: G2 only bites a host that ships its custom type on
more than one core.** The overwhelmingly common embedder uses *one* language; for them the
cross-core worry evaporates entirely — they need only round-trip and sort sensibly, which is a
single-core G1 property the host controls outright. And a host that *is* multi-core is, by
definition, capable of running jed's conformance harness against its own type.

**This is not a bolt-on — it is the existing float precedent generalized.** A binary float is
class **A** in the ledger ([determinism.md](determinism.md) §6, [../conformance/determinism_exceptions.toml](../conformance/determinism_exceptions.toml)):
its computed values drop G2/G3 but are *contained* (§4 of that doc — taint propagates; a tainted
value may not silently flow through `WHERE` / `ORDER BY … LIMIT` / a narrowing `CAST` and change
the row multiset). A host scalar type or a non-cross-core host function is **exactly that** — a
**host-owned ledger entry**. jed already has the full vocabulary (G1–G4, classes B/A/I/P, taint,
containment, the default-deny posture); host extensibility is its natural extension, not a new
contract.

---

## 3. The type model — `TypeExpr`, and where the container axis stops

Today the in-memory type is `Type = Scalar(ScalarType) | Composite(CompositeRef) | Array(Box<Type>)
| Range(Box<Type>)` — a closed scalar enum, a *nominal* composite (a by-name catalog reference),
and two *structural* containers (element carried inline). To hold host types cleanly, make the
model explicit as three constructors:

```
TypeExpr =
    Builtin(ScalarId)                     -- the closed compiled-in scalar set (i16 … jsonb)
  | Nominal(DatabaseTypeId)               -- a catalog object: a composite OR a host scalar type
  | Apply(BuiltinConstructor, [TypeExpr]) -- a structural container: array<T>, range<T>
```

Three properties are load-bearing:

- **`Nominal` unifies composite and host-scalar at the *representation* level, keyed by a stable
  id, not a SQL name.** A composite and a host scalar type are both "a catalog object with an id
  and a method-set" — the same arm, resolved the same way. Using a **`DatabaseTypeId`** (not the
  current `CompositeRef { name }`) decouples type identity from the SQL-visible name, which §6/§7
  need for dependency pinning. **But the unification is representational, not behavioral**: a
  composite is **self-describing** (its field list is persisted, so any core reopens it with no host
  code — §4.1) while a host scalar is **opaque** (decoding needs the host's registered codec — §4.3).
  Same arm, opposite reopen behavior; §11's manifest is what keeps that distinction legible.

- **`Apply` is a *closed* set of builtin container constructors. Host-defined type *constructors*
  are out of scope (Fork F1, §13).** A host cannot introduce a new container kind (`Foo<T>`) — that
  would require host-supplied recursive codecs, type inference, constructor syntax, subscripting, and
  capability derivation, a far larger surface than a host *scalar* type, for a rare need. The
  container axis stays jed-owned. A host "container-ish" value (a vector, a geometry) is expressed as
  a **host scalar with an opaque body** (§4.3) or a **composite** (§4.1).

- **The structural↔nominal split has a principled boundary: identity-as-a-bijection-on-element.**
  `array<T>` is structural because array type identity **is a bijection on the element type** — there
  is exactly one `int[]` per `int`, with no surface to name or distinguish a second (PG materializes
  a companion `_int4` type, but that identity is a bijection too, so a structural representation
  computes the identical identity — [array.md §10.2](array.md)). **Range identity is *not* a
  bijection on its subtype**: PostgreSQL permits multiple range types over one subtype with different
  `canonicalize`/ordering choices. jed's six built-in ranges are structural only *by coincidence of
  the built-in set* (one per subtype); a **future host-defined range must therefore be `Nominal`,
  not `Apply`** — it carries a canonicalization choice that its subtype does not determine. This is
  the sharp form of "defer host ranges": they are not a structural-container extension at all.

### 3.1 Container capabilities are *derived* from the element (the payoff of structural)

Because `array<T>` is structural, its methods are **derived** from `T`'s, not implemented per
element type — exactly as PostgreSQL's `array_ops` is polymorphic over `anyarray` (one
implementation, not one per `_int4`/`_text`). So the moment a host scalar type `geo_point` supplies
the capabilities below, `geo_point[]` becomes valid **with no additional host code**:

| `geo_point[]` capability | derived when `geo_point` supplies… |
|---|---|
| storable | Storable (text-in/out, bin-in/out) |
| element-wise `=` / DISTINCT / GROUP BY | Equatable |
| element-wise order / `ORDER BY` | Ordered |
| array as a B-tree key | Keyable (order-preserving element bytes) |
| `array_ops` GIN over `geo_point[]` | Equatable + a canonical term encoding (§8.2) |

This is the strongest argument for keeping containers structural: one array implementation,
parameterized by the element's registered method-set, rather than a materialized array type (and a
re-implemented array opclass) per element.

### 3.2 The type-method interface — a capability ladder

A type is a value of this method-set. For built-ins each method *is* the existing hand-written
kernel; for host types it is registered code; the interface is identical either way (the "catalog
model"). Capabilities are acquired **progressively** — the host supplies only as far up the ladder
as its type needs, and each rung unlocks a class of use. The **split of Equatable from Ordered** is
deliberate: some useful host types are equatable with no sensible total order (PG separates its
hash/equality opclasses from btree ordering), so bundling them (as the previous draft's single
"comparable" tier did) is wrong.

| Rung | Host supplies | Core owns | Enables |
|---|---|---|---|
| **Storable** | text-in (`typinput`, `22P02` on bad input), text-out (`typoutput`), bin-in/out (the value-codec body); optional **canonicalize** | NULL presence tag + length framing of the opaque body | columns, parameters, results |
| **Equatable** | `equals` (or "canonical bytes are equal") | 3VL derivation | `=` `<>`, `DISTINCT`, `GROUP BY`, hash join/agg |
| **Ordered** | a **total** `compare(a,b) → Lt\|Eq\|Gt` | NULL sort position (§2) | `<` `<=` `>` `>=`, `ORDER BY`, `MIN`/`MAX` |
| **Keyable** | an **order-preserving** `key-encode(value) → bytes` — *encoder only* | **self-delimiting framing** of that key (length-prefix / terminator-escape) so index components and the row-key suffix stay parseable — **no host key-decoder** | `PRIMARY KEY`, `UNIQUE`, ordered index |
| **Indexed** | access-method-specific opclass routines (§8.2) | the shared tree machinery | GIN / GiST domain-specific indexes |
| **typmod-in/out** (optional, orthogonal) | parameter parse/format (`numeric(p,s)`, `varchar(n)`) | the catalog typmod slot | parameterized types |

Two refinements over the previous draft, both from the review:

- **No host key *decoder*.** The host produces order-preserving bytes; jed frames them
  self-delimitingly (the same terminator-escape it already uses for variable-width `text`/`bytea`
  keys, [encoding.md §2.4/§2.6](encoding.md)), so the row-storage-key suffix after a host key is
  still recoverable without decoding the host value. Keyable is therefore *encode-only*, halving the
  host's tier-3 burden.
- **The Keyable invariant is a conformance property, not a hope.** A host's `compare` and
  `key-encode` must agree exactly:

  ```
  sign(compare(a, b)) == sign(memcmp(key(a), key(b)))
  compare(a, b) == 0   iff   key(a) == key(b)
  ```

  The §12 harness tests **this joint property**, not merely that each method round-trips
  independently — a host whose comparator and key encoder disagree is the classic silent
  index-corruption bug, and it is exactly what jed's no-reference-implementation stance cannot
  discover any other way.

Two cross-cutting declarations the registration also carries (contracts, not methods):

- **cross-core-deterministic?** — does the host assert (and the harness verify, §12) that
  text/bin/compare/key are byte-identical across the cores it ships? Default **no** → the type is
  tainted (§10). Render tag **`X`** (proposed): text-out compared verbatim like `T` when declared
  cross-core-deterministic, and like the float `R` tag (host-supplied tolerant compare) when not.
- **cost contribution** — see §9 ([cost.md](cost.md) §6); required on any handle with a non-zero
  `max_cost` (the untrusted-query surface, CLAUDE.md §13).

---

## 4. The three kinds of extension, ranked by how cleanly they land

### 4.1 Composite types — safest; derived codec; G2 free; **landed** (and the early key win)

`CREATE TYPE addr AS (street text, zip i32)`. The host supplies only the **shape** — a field
list of *existing* types (built-in or previously-defined composite). **No host codec code at all.**
jed derives every method compositionally from fields that are already cross-core-identical:

- **bin-in/out** = a NULL-bitmap over the fields followed by each field's value-codec body, in
  declaration order. One generic codec, parameterized by the field list — not code per type.
- **compare** = lexicographic over fields (PostgreSQL row-comparison semantics); 3VL `=` matches
  PG (`ROW(1,NULL) = ROW(1,2)` is NULL), the *sort* order is total (NULLs per the field rule).
- **key-encode** = compose the per-field key encodings — **this machinery already exists** as the
  composite-PRIMARY-KEY encoding ([encoding.md §2.10](encoding.md), the nullable presence tag §2.2).
- **text-out** = a generic `(a,b,c)` renderer with PG's field-quoting rules; **text-in** parses it.

Because every method is derived from cross-core-identical parts, **G1–G3 hold by construction** —
no ledger entry, no harness obligation. A composite value is **self-describing and portable** — its
field list is persisted in the type catalog (§6), so *any* jed reads a file containing it without
the host's code. This is why composites are both the safest and the most portable extension, and
why the type itself **shipped first** ([composite.md](composite.md), `format_version` 9): the open
`TypeExpr` (§3) is *already real* for composite.

**The one derived-and-cheap indexable custom type — hoist composite-as-key early.** A composite
`PRIMARY KEY` / index is `0A000` today ([composite.md §6](composite.md); the order-preserving
encoding is authored at [encoding.md §2.10](encoding.md) but unexercised). Lifting it is the **only
way to get an indexable custom type with zero host codec and zero determinism transfer** (it is
derived, G2-free), and it is **independent of the entire host-code registry**. It should therefore
land *ahead* of the host-scalar ladder, not folded into it (§14).

### 4.2 Host-defined scalar functions — no type-system change; high value

These operate over **existing** types (or, once §4.3 lands, host types), so they touch the function
catalog, not the type catalog. Reuse the *shape* of a [../functions/catalog.toml](../functions/catalog.toml)
`kind = "function"` entry, but as a **runtime function registry** the resolver consults **after** the
codegen'd static `OPERATORS` table (the structure §5 already built for built-ins). What registration
must carry:

- **A signature over type patterns, and signature-level (not name-level) collision.** Key the
  registry by `(name, [TypePattern…]) → TypePattern`, and reject only an **identical signature**, not
  every reuse of a name. A host must be able to add `abs(geo_point)` while the built-in `abs(decimal)`
  keeps working — PostgreSQL overloads by input types, and blanket name rejection (the previous
  draft's rule) forecloses the common polymorphic case. A built-in wins on an *exact*-signature
  collision (propose `42723 duplicate_function` only for the identical signature). This extends the
  existing family-based overload selection (`family_matches`) the resolver already runs.
- **A vectorized / batched signature** — column-in → column-out. This is the single decision
  [cores.md](cores.md) §2.1 says keeps a *wrapped* core viable (it amortizes the per-row FFI upcall
  for Swift/Java/C#). Define the **batch ABI now**, but the executor may call it with a **batch of
  one** initially — batching helps scans and index builds; it **cannot** eliminate the upcall for a
  single-row write, and **error ordering must be identical to scalar** evaluation (a batch that
  raises must raise for the same row, in the same order, a scalar call would).
- **A cost contract** — one of the three [cost.md](cost.md) §6 designs (§9 below). Without one, the
  function is admissible **only** on a handle with `max_cost = 0` — i.e. *not* the untrusted-query
  surface (CLAUDE.md §13).
- **Volatility + cross-core declaration** — two independent axes:
  - **volatility** (`immutable` / `stable` / `volatile`, PG's notion) governs planning: only an
    `immutable` function may back an index expression (§8.1) or be constant-folded.
  - **cross-core-deterministic?** (default **no**) governs G2: an `immutable` function can *still* be
    non-cross-core (e.g. it calls the platform libm). Only a function declared *and harness-verified*
    cross-core-deterministic produces untainted results (§10).

### 4.3 Host-defined core (scalar) types — reasonable; climb the §3 ladder, the host picks how high

The hard case: the host supplies text/bin/compare/key directly; jed derives nothing. Host scalar
types climb the **§3 capability ladder** (Storable → Equatable → Ordered → Keyable → Indexed),
which is the **exact staging jed used for its own types** — jed shipped each non-integer type
storable-first and rejected key use with `0A000` until the order-preserving encoding was ready
(uuid was the first non-integer `PRIMARY KEY`; text/bytea/decimal/interval later joined —
[TODO.md](../../TODO.md) Phase 3, [encoding.md](encoding.md)). The on-disk shape:

| Ladder reached | On-disk | Keys/index | Difficulty |
|---|---|---|---|
| **Storable / Equatable / Ordered** | `type_code = 21` (extension) + a **length-framed opaque body** jed round-trips without understanding | `0A000` | easy–medium — "store my domain value, get it back, sort it" is single-core G1 |
| **Keyable** | (as above) | PK/index allowed; the order-preserving key bytes are the cross-core obligation | hard — the full G2/G3 byte contract lands here (multi-core only) |
| **Indexed** | (as above) + opclass routines (§8.2) | GIN/GiST | hardest — deepest determinism surface (GiST tree bytes) |

Rows carry **no per-value type tag** — a column's type comes from the catalog
([../fileformat/format.md](../fileformat/format.md)) — so the opaque body slots in cleanly: jed
frames it (length prefix via the presence-tag machinery) and treats it as opaque bytes. The
cross-core byte obligation (§2) bites **only at Keyable**, and only for a multi-core host — most
host scalar types never leave Storable/Ordered, where an expression B-tree index on an *immutable
host function of the column* (§8.1) already gives acceleration without any key-byte contract.

**Durability caveat (the asymmetry vs composites).** A file using a host scalar type is **not
self-describing**: reopening it requires the host to re-register that type's codec (by component
identity + version, §6/§7). Without it, the column cannot be *decoded* — but that failure is **graded
per object**, not a whole-file brick (the previous draft's blunt `XX002`): §11 specifies exactly
what stays readable. This asymmetry is the real cost of host scalar types and the reason composites
(self-describing) are preferred whenever the value is expressible as a tuple of existing types.

---

## 5. Dispatch — registry the many, inline the few

> **Built (built-ins).** The "registry the many" half of this section is implemented for the
> built-in scalar functions and aggregates, in all three cores. The known-name gate, the result-type
> match, and the name→variant match are gone, replaced by a catalog scan keyed by `(name,
> arg_families)` over the generated `OPERATORS` (kind=`function`) / `AGGREGATES` tables plus small
> shared result-code / plan interpreters; the per-row kernel is still reached **by id** (a compiled
> `ScalarFunc` enum → `match`, hand-written per core, §5). A code audit confirms the shape: metadata
> is data-driven over `OPERATORS`, but the id → kernel path is a **three-site compiled pattern**
> (enum variant + name→id match + eval match) with **no runtime injection seam** — a *host* function
> has no way in today. The built-in type method-set is likewise **not yet dogfooded** through a
> registry (Fork A, §13). Host registration into the same table (§4.2) is the proposal.

"Route everything through a registry" and "keep everything as hand-written `match`" are both wrong
because they treat *dispatch* as one decision. It is **four sites** with sharply different cost
profiles, and the right answer differs per site. The trap to avoid is conflating the
**function-name case statement** (mostly *cold*; its real problem is maintenance) with the
**per-value type method-set** (the genuinely hot, inline-sensitive loop). They earn opposite
verdicts.

| Dispatch site | Fires | Count | Indirection cost | Verdict |
|---|---|---|---|---|
| **Resolution** (name → kernel + result type) | cold, per query | hundreds | none — a table lookup beats a hand-match | **registry** — strictly better, advances §5 |
| **Named-function eval** (`abs`, `lower`, `round`, …) | per row | hundreds | indirect call ≈ **<1%** of a kernel doing real decimal/string/float work | **registry** — PG-cheap, the right move |
| **Core operators** (`+ − * / = < > AND` …) | per row | ~a dozen | replaces an inlined ~1-instruction kernel with a real CALL → loses inlining/vectorization | **keep monomorphized** |
| **Type method-set** (compare / codec) for built-ins | per *value*, in sort/scan inner loops | ~10 methods × ~17 types | same — a fixed-width int compare is one instruction; wrapping it in a call is a 3–10× hit *on that op* | **keep monomorphized** (benchmark-gated) |

**Why this is not a real conflict.** The set that must stay inlined is **small and bounded** (a
dozen operators + a fixed-width type method-set); the set that is "hundreds" is **exactly the set
that is fine — even better — through a registry**. So the recommendation is **registry the many,
inline the few**:

- **Registry-ize resolution and named-function / aggregate evaluation.** Keyed by `(name,
  arg_families)` (extended to type patterns for host overloads, §4.2), populated from the catalog
  descriptor. Host functions register into the same table. The per-row kernel is reached by
  id/fn-pointer; the kernels stay hand-written per core (§5 forbids codegenning them). This is PG's
  `fmgr` model and PG-cheap, and it is the structure that scales to hundreds of functions. **The one
  missing piece for hosts is the injection seam** — today the id→kernel path is a compiled `match`; a
  host kernel needs a registered fn-pointer reachable by id.
- **Keep the dozen core operators and the built-in type method-set monomorphized.** `TypeExpr` gains
  a `Nominal(id)` path that dispatches through the registry; built-in scalar arms stay inlined
  `match`. CLAUDE.md §10 ("resist deep generics / over-interfacing") and the SQLite-footprint
  benchmark bar both argue here, and *only* here.

**On "PostgreSQL routes everything through `fmgr` and is plenty fast" — accurate, with the caveat
that supports the split.** PG's per-tuple overhead (slot deforming, `FunctionCallInfo` packing)
dwarfs the indirect call — and PG nonetheless shipped LLVM JIT precisely because that dispatch
overhead *was* a measurable analytical bottleneck. So PG justifies registry dispatch for
**functions** (the hundreds), and is *not* evidence for indirection in the one-instruction
comparator (the few).

**The type method-set is benchmark-gated, not a principled no.** The "keep inlined" claim is
load-bearing only for **fixed-width** comparators. For **variable-width** types (`text`/`decimal`/
`interval`) `compare` already does real work, so dogfooding *those* through the registry would barely
register. Because the dispatch decision is made at type granularity, keeping built-in integers
inlined costs nothing and forces nothing — so Fork A (§13) is a *measurement*, not a doctrine.

### 5.1 The seam is not the dogfood — start with the function seam

Two moves hide inside "make dispatch a registry," and separating them is what makes host
extensibility incremental instead of a big-bang reshape:

- **The seam** — *add* a registry dispatch path (`TypeExpr::Nominal(id)`, or a host function id → a
  registered fn-pointer) **alongside** the built-in arms. This is the actual prerequisite for host
  types/functions: host-registered code cannot be reached through a compiled `match` on a closed
  enum.
- **The dogfood** — *remove* the built-in arms and route jed's own types/functions through that same
  registry. This is **optional**, **benchmark-gated**, and **not on the host-extension path** at all.

They feel like one decision; they are not. The seam ships with every built-in arm still inlined;
"remove the match arms" is the dogfood (Fork A, §13). Two consequences for *where to start*:

1. **The function seam precedes the type seam.** Function *resolution* is already data-driven (the
   built half above); the remaining work is the id → kernel injection seam (today a compiled
   `ScalarFunc` enum → `match`; a host kernel needs a registered fn-pointer reachable by id). That
   seam delivers **host functions over existing types** (§4.2) — a whole extension category that
   needs **no** type registry and touches **none** of the per-value type-method hot loops. So it is
   the cheapest first move (§14 step 3), ahead of the type-method catalog (§14 step 5, the
   prerequisite for host *types*).
2. **The corpus gates the seam; benchmarks gate the dogfood.** A registry refactor is
   behaviour-preserving — rows/cost stay byte-identical, so the corpus passes unchanged (exactly how
   the resolution restructure was verified). But the hot-loop regression the dogfood risks is
   **wall-clock**, invisible to the corpus — a `rake bench:diff` fact. So the seam is *corpus*-gated
   (safe, behaviour-preserving) and the dogfood is a separate *benchmark* measurement.

---

## 6. On-disk + catalog representation

Extensibility introduces what jed has not needed until now beyond composite: a **persisted host-type
catalog** — the `pg_type` analog. Today the "type catalog" is the compiled-in
[../types/scalars.toml](../types/scalars.toml), referenced on disk by a stable `u8` type code
(0–20 are allocated — [../fileformat/format.md](../fileformat/format.md) *Stable type codes*; **21
is the next free**). Built-in types stay compiled-in and keep their codes. The additions:

- **The extension `type_code` is `21`, not `14`.** (`14` is the *landed* composite type — the
  previous draft's `type_code = 14` for "extension" was a stale collision. `15` array, `16` date,
  `17` range, `18`/`19` json/jsonb, `20` jsonpath are also taken.) A column of a host scalar type
  stores `type_code = 21` + a reference (a per-database `DatabaseTypeId`) into the persisted host-type
  catalog. Composite columns keep `type_code = 14`; the persisted-catalog machinery composite already
  established (`entry_kind`-tagged catalog entries, [../fileformat/format.md](../fileformat/format.md))
  is the template.
- **The persisted host-type catalog** records, per host/composite type: its **name**, a **kind**
  (`composite` | `host-scalar`), the **component identity** (§7), and — for composites — the **ordered
  field list** (`(field-name, type-ref)` pairs). This mirrors PG exactly (built-in and user types
  coexist in one `pg_type`) and, when a host scalar type first lands, is the deepest structural
  change — bump `format_version` **30** (the next free; currently 29) and record it in
  [../fileformat/format.md](../fileformat/format.md).
- **Composite on-disk body** = NULL-bitmap ‖ field bodies (value-codec, declaration order). Fully
  described by the persisted field list ⇒ **self-describing / portable** (§4.1) — *already true*.
- **Host-scalar on-disk body** = a length-framed opaque blob (§4.3). The persisted entry records name
  + component identity; decoding requires the registered codec ⇒ **not self-describing**, gated by §11.

---

## 7. Registration vs. schema — code at open, catalog in SQL

Separate the **executable registry** (host-provided code) from the **database schema** (persisted
catalog objects). This is the discipline that keeps SQL migrations reproducible and keeps a database
from silently mutating when a host merely links a newer library.

1. **The host supplies an immutable `ExtensionRegistry` in the open/create options** — the codecs,
   comparators, key-encoders, function kernels, and opclass routines its types/functions name. This
   registry is **frozen for the database handle's lifetime**: late mutation would invalidate prepared
   plans, overload resolution, and index assumptions mid-session, so it is fixed at open.
2. **SQL DDL transactionally binds catalog objects to registered components** — and *only* SQL
   changes the schema. Registering code alone must **never** silently create a type or function:

   ```sql
   CREATE TYPE geo_point AS HOST 'com.example.geo/point' VERSION 'storage-1';
   CREATE FUNCTION geo_distance(geo_point, geo_point) RETURNS f64
     LANGUAGE HOST 'com.example.geo/distance' IMMUTABLE;
   ```

   So a migration file — not a linked library version — is the record of what the database contains
   (the [jed-migrate](../../migrate/design.md) reproducibility contract).

**Component identity is a five-field tuple, not one "version."** A single "codec version" (the
previous draft) cannot express that a *storage-format* change requires value migration while a
*comparator/semantic* change leaves values readable but invalidates indexes. Record, per component:

```
provider/package id · component id · host-ABI version · storage-format version · semantic version
```

- a **storage-format** bump ⇒ stored bytes must be migrated (re-encode);
- a **semantic** bump (a changed `compare`, a bug-fixed function) ⇒ values stay readable but any
  **stored key / index / stored expression** built from the old semantics is stale and must be
  rebuilt (the [compatibility.md](compatibility.md) function-drift problem, §11).

This tuple is what the persisted catalog (§6) stores and what the graded reopen verdict (§11) and the
index dependency list (§8.1) compare against.

---

## 8. The index connection — two distinct problems

Host functions and host types touch indexing in two very different ways. Keep them separate: the
first is cheap and high-value; the second is the frontier.

### 8.1 Expression & partial indexes over host functions — persist *resolved dependencies*

jed already lets an index key or a partial-index predicate be an **immutable** expression
([indexes.md §1](indexes.md)), and pushes down `WHERE f(col) = $1` by **syntactic structural match**
against the stored key expression ([indexes.md §5.1](indexes.md)). So an immutable host function can
back `CREATE INDEX ON t(geo_hash(pt))` and get pushdown. This is the highest-value index connection —
it covers "index a derived value" without any opclass work.

**But the current persistence mechanism is unsound for host functions and must be extended.** jed
persists an index expression as **canonical SQL text** and *re-resolves* it on reopen
([indexes.md §6](indexes.md)). For a built-in that is fine (deterministic, versionless). For a host
function it is **not**: after reopen, the same text `geo_hash(pt)` can re-bind to a **different
implementation** (a different registered component, or a newer semantic version) — silently changing
the stored keys' meaning. The fix: **persist the resolved dependency alongside the text** —

```
function component id · exact signature · semantic version · result type
```

— and admit a host function into an index expression (or predicate) **only** when it is `immutable`
+ persistently-registered + version-pinned + deterministic under the file's declared portability
policy. On reopen, a **semantic-version mismatch** forces a rebuild (or degrades the index per §11),
never a silent stale-key read. (PostgreSQL likewise requires index-expression/predicate functions to
be `IMMUTABLE`; jed adds the cross-core version pin because it has no single reference implementation.)

### 8.2 Operator classes — unify support routines into the extension registry

For GIN/GiST acceleration of a host type, reuse the **existing opclass seam** ([gin.md §2](gin.md),
[gist.md §2](gist.md)) rather than attaching "index methods" to the type. Those AMs already separate
generic tree machinery (shared, opclass-agnostic) from a small set of type-specific routines
(`union`/`consistent`/`penalty`/`picksplit`/`same` for GiST; `extractValue`/`extractQuery`/
`consistent` for GIN). Unify those routines with the §7 extension registry as one more registered
component kind:

```
OpClass { identity, access_method, input_type_pattern, strategies (operator → strategy),
          support_routines, semantic_version, default_for_type }
```

The first surface can permit **one default opclass per (type, access method)** while keeping the data
model open to multiple opclasses later. PostgreSQL's durable idea: an access method defines the
required *support roles*, and an opclass binds a type's routines + operator→strategy map to them.

Two firm boundaries:

- **Do not expose host-defined *access methods*.** Host opclasses for jed's **built-in** B-tree /
  GIN / GiST are enough. A host-defined *access method* would expose page layout, recovery, locking,
  cost, and the GiST tree-shape determinism contract — decline it (the container-kind decline of §3,
  applied to indexing).
- **Host GiST/GIN opclasses are the deepest determinism surface in the engine and are deferred**
  (Fork F5, §13). jed's GiST tree *shape* is a cross-core byte contract (operation-deterministic
  `penalty`/`picksplit`, [gist.md §3](gist.md)); a host opclass must honor it byte-for-byte across
  cores to stay untainted — the place a single-core host will most want to relax cross-core identity.
  Ship §8.1 (expression indexes) + Ordered-tier `compare` first; they meet most acceleration needs.

**Capability derivation applies here too** (§3.1): `array<T>`'s `array_ops` GIN is derived from `T`'s
term encoding + Equatable, not implemented per element — so a host element that supplies those gets
`geo_point[]` GIN for free, exactly as `int[]` does.

---

## 9. Cost — cooperative accounting, not a sandbox

A host function (and a host type's `compare` / codec / opclass routine) is **opaque to the meter** by
default — its code does not route through `Meter::charge`. The registration API must therefore carry a
cost contract, one of the [cost.md](cost.md) §6 designs, extended here from functions to type methods:

- **(a) Declared static weight** — a per-call constant (generalizes the live `cost` field in
  [../functions/catalog.toml](../functions/catalog.toml)). Charged once per call like `operator_eval`.
- **(b) Declared cost-as-fn-of-args** — a *pure, deterministic* function of argument values/sizes
  (the `decimal_work` / `value_compress` model), charged **up front and guarded before** the call.
- **(c) A deterministic `charge(n)` callback** — the host charges as it works, enabling a
  chunk-boundary **mid-call abort**. Must be deterministic and cross-core identical (no wall-clock,
  no allocation/iteration-order dependence).

**Type methods need this too:** a host `compare` inside a sort, or a host codec on a large value, or
an opclass routine deep in a tree descent (the hottest position of all), can dominate a query's cost.
A host type/function declaring **none** of (a)/(b)/(c) is admissible **only** on a handle with
`max_cost = 0` (unlimited) — explicitly **not** the untrusted-query surface (CLAUDE.md §13).

**What the cost contract is and is not.** It exists for **cross-core cost identity** (a wrapped core
and a native core must accrue the *same* cost for the same call — G2 on cost, §8) and **admission
policy**. It is **not a sandbox**: metering is *cooperative* — a hostile or buggy host callback can
ignore `charge` and loop forever before returning, and a declared static weight is an accounting
figure, not a CPU governor. This is consistent with CLAUDE.md §13's standing rule that **host code is
outside the built-in untrusted-query guarantee**: a real bound needs OS-level isolation or a trusted
cooperative callback, which jed does not provide. The cost contract makes host cost *honest and
cross-core-identical*, not *safe*.

---

## 10. Determinism — the host-extension ledger and containment

Host code is the ultimate nondeterminism risk, so it slots into the default-deny model of
[determinism.md](determinism.md) §1/§4/§9:

1. **Default-deny / tainted-by-default.** A host function or type is assumed **not**
   cross-core-deterministic unless declared so *and* verified by the harness (§12). An untainted host
   extension keeps G1–G3 like any built-in; a tainted one drops G2/G3 on its own results — a
   **class-A or class-I** member, ledgered.
2. **The host owns the ledger entry.** Built-in exceptions live in jed's
   [../conformance/determinism_exceptions.toml](../conformance/determinism_exceptions.toml); a host
   extension is a **host-owned** entry in the host's own extension ledger, stating the same fields.
   jed offers the containment *tooling*; it does not author the host's entry or decide its risk
   appetite.
3. **Containment is the safe default, not a mandate.** By default a tainted host value does not
   silently promote into jed's deterministic surface: jed propagates the weakest-guarantee taint and
   **fails closed and legibly** — declining to silently flow a tainted value into a `PRIMARY
   KEY`/index, an `ORDER BY … LIMIT` boundary, or a narrowing `CAST` that changes the row multiset —
   exactly the discipline floats follow ([determinism.md](determinism.md) §4). This protects **jed's
   own** cross-core guarantee over the rest of the database; it is **not** jed policing the host. A
   host that wants the relaxation (its non-cross-core type *as* a `PRIMARY KEY`, accepting that files
   using it are no longer cross-core-portable) may opt out for its own surface and own it (CLAUDE.md
   §13). A query that touches no host extension stays fully G1–G3.
4. **Single-core hosts are unaffected by G2.** Taint is about *cross-core* identity; a host extension
   that is merely G1 (reproducible on one binary) is fine for everything except multi-core
   distribution, which that host does not do.

---

## 11. Missing on reopen — the graded per-object verdict (not a whole-file brick)

A file using a host scalar type or a host-function-backed index depends on host code that a reopening
binary may not have registered. The previous draft's answer — a hard `XX002` that bricks the file —
is both too blunt and **inconsistent with jed's own** [compatibility.md](compatibility.md), which
already designs the right model: on open, compute a **graded, legible, per-object compatibility
verdict** from a **requirements manifest** carried in the file, and either interpret correctly,
degrade a *dependent object* to a reduced mode, or refuse **naming exactly what is missing** — never
silently misinterpret. (compatibility.md is itself an unratified proposal; its **first instance —
the collation version-skew verdict — has landed**, [collation.md §12/§14](collation.md), so the
pattern is proven. A host extension is another *registrant* into that same manifest, not a separate
mechanism.)

Graded, per object (blast radius contained to the dependent object, never the database):

- **An opaque, length-framed host-scalar *value* stays structurally readable and copyable** — you can
  `SELECT` the row, dump/copy the bytes, even `DELETE` it; you just cannot *interpret* the value
  without the codec.
- **An index whose codec/function/opclass is unavailable is never used for reads** — the planner
  ignores it; queries fall back to a correct heap scan.
- **A table whose indexes or constraints cannot be *maintained* becomes read-only** (writes that
  would need the missing routine are refused, reads continue).
- **A missing function behind an ordinary *expression* index does not brick unrelated tables** — only
  that index/table degrades.
- **A missing function behind `UNIQUE` or `EXCLUDE` must *never* be bypassed** — the constraint is
  load-bearing, so its table is read-only rather than accepting writes the constraint can't check.
- **A host type in the `PRIMARY KEY`** (its key bytes uninterpretable) may force the whole table into
  **read-only heap-scan** mode.
- **A re-established *different* semantic version** (§7) requires an explicit **rebuild/migration**,
  never a silent stale-key read.

This dependency machinery is **not optional** once host code can appear in persisted types or index
expressions — and it need not wait for the full compatibility.md manifest: a **small per-object
dependency list** (the §7 component-identity tuples an object references) can land first and grow into
the general manifest.

---

## 12. Conformance — turn jed's own machinery outward

A host extension is "conformant" by the same means a core is: a corpus it passes and a byte-exact
round-trip it survives, except the *host* authors the cases. Extend §7/§8 of
[conformance.md](conformance.md) rather than inventing a new contract:

- **jed ships a host-extension conformance harness** — the same sqllogictest runner each core embeds,
  callable against a database with the host's types/functions registered.
- **The host authors a corpus** (`statement ok`, `query`, `statement error`) — and, for a **Keyable
  (indexable) type**, **byte fixtures** for the order-preserving key codec **and a test of the joint
  `compare`/`key` invariant** (§3.2) — testing that `sign(compare) == sign(memcmp(key))` and
  `compare == 0 iff key ==`, not merely that each round-trips. This is the check that catches the
  silent comparator/key-disagreement corruption jed cannot otherwise discover.
- **The extension is conformant iff it passes on *every core the host ships*** — byte-identical where
  it declared cross-core-determinism, tolerant-compare (the `X`/`R` tag) where it ledgered an
  exception. A single-core host runs it on one core; a multi-core host runs it on all, and *that* run
  discharges the G2/G3 obligation of §2.

jed provides the harness, the byte-fixture format, and containment as a safe default (§10); the host
provides the cases and (for multi-core) runs them everywhere, and may relax what it owns.

---

## 13. Open forks (for the maintainer)

**Fork A — how far the type-vtable dogfood goes.** The **function** side is no longer a fork —
registry-izing resolution + named-function evaluation is the recommendation outright (§5). The only
thing open is the **built-in type method-set**:
- **(recommended) Registry the variable-width built-ins + host types; inline the fixed-width
  built-ins.** Route `text`/`decimal`/`interval`/`bytea` through the registry **as the anti-rot
  move**: their `compare` already does real work (codepoint walk, limb compare), so the indirect
  call is *noise*, and now the extension path is the same path jed's own hottest non-integer types
  run on — exercised by the whole existing corpus, so it cannot bitrot. `int*`/`bool`/`uuid`/
  `timestamp`/`date` stay monomorphized (one-instruction comparators, where inlining is the whole
  game). This buys the full dogfood's "shared path can't rot" benefit **without** the fixed-width
  regression, and keeps the SQLite-footprint hot path.
- **Full type-vtable dogfood** (fixed-width included) is **benchmark-gated, not principled**: land the
  inlined version, measure the fixed-width regression under the registry, dogfood the rest only if in
  the noise.

**Fork B — cross-core stance for host scalar types.**
- **(recommended) Admit on a single core freely**: usable immediately on one core; the harness (§12)
  is *optional* and only meaningful when the host goes multi-core (matches "G2 only bites multi-core
  hosts", §2).
- **Gate behind the harness**: no host scalar type is blessed until its byte-fixture harness passes,
  even for a single-core host. Safer for the ecosystem, taxes the common single-core embedder.

**Fork F1 — host container *kinds*.** ✅ **Recommended: decline** (§3). The container axis stays
jed-owned; hosts get `Nominal` scalars + composites, and host-container-ish values ride an opaque host
scalar. Recorded here so the closed-`Apply` decision is explicit.

**Fork F5 — host GiST/GIN opclasses.** ✅ **Recommended: defer, define the seam now** (§8.2). Unify
the opclass registration model into the extension registry (one default per type/AM), but ship the
cheap index connections (§8.1 expression indexes + Ordered `compare`) first; decline host access
methods outright.

---

## 14. Delivery order

Front-load the wins that need **no** new machinery; defer the registry/opclass reshape (which the §5
code audit shows touches every dispatch site in every core). In dispatch terms (§5.1): the **function
seam** (step 3) precedes the **type-method catalog** (step 5), and each is an *added* registry arm,
never a removal of built-in arms — the full type-vtable dogfood (Fork A) is a later, benchmark-gated
cleanup, not a prerequisite. A suggested sequence:

1. **Rewrite this doc against current reality** ✅ (this revision) — ratify `TypeExpr`, component
   identity, and the capability vocabulary.
2. **Composite-as-key** (§4.1) — the one derived, G2-free, indexable custom type; **independent of the
   host-code registry**, so it lands earliest.
3. **Runtime host scalar *functions* over existing types only** (§4.2), *no persisted use* — the
   function-registry injection seam + signature overloading + volatility/cross-core/cost, batch-of-one.
   ✅ **landed (all three cores)**: an `ExtensionRegistry` a host builds and passes in
   `CreateOptions`/`OpenOptions`, frozen for the handle and shared into every session (streaming
   included); a separate `HostFunc`/`hostFunc` resolved node reached **by id** through the registry
   alongside the untouched built-in dispatch; built-ins win an exact-signature collision; `cost`
   (design (a) static weight) charged per call + guarded against the ceiling; wrong-typed kernel
   results caught (`22000`); `42723` for a duplicate registration. **Deferred to later slices**:
   the vectorized/batched kernel ABI (batch-of-one only for now), non-strict host functions, host
   functions over container args, and runtime *enforcement* of the `volatility`/`cross_core`
   declarations (recorded but not yet acted on — no host function is constant-folded, and there is no
   runtime taint mechanism yet, matching how `float` is handled at the spec layer, §2).
4. **Catalog-bound, versioned functions + resolved-dependency persistence** for expression/partial
   indexes (§7/§8.1) — the soundness fix that lets a host function appear in a persisted index.
5. **Opaque host scalar *types*** (Storable/Equatable/Ordered) + **derived `array<host-type>`** (§3.1,
   §4.3, `type_code 21`, `format_version` 30) + the **graded reopen verdict** (§11).
6. **Keyable** host scalar types (order-preserving encoder + the invariant harness, §3.2/§12).
7. **Host GIN/GiST opclasses** (§8.2) behind the unified seam.
8. **Non-default opclass SQL syntax** + richer migration/rebuild tooling.

---

## 15. What ratification changes (downstream edits)

When a section here is ratified, update **in the same change** (mirrors [determinism.md](determinism.md) §10):

- **[CLAUDE.md](../../CLAUDE.md)** — §2 (the §2.1 host-function pivot already anticipates this), §8
  (host extensions as a host-owned divergence surface), §13 (host code metered-or-`max_cost = 0`, and
  cost-is-not-a-sandbox). Add a pointer to this doc.
- **[TODO.md](../../TODO.md)** — the Phase 9 items point here; add the §14 slices (composite-as-key,
  the host-function injection seam, resolved-dependency index persistence, host scalar types tiered
  per §4.3, host opclasses).
- **[api.md](api.md)** — the `ExtensionRegistry` open/create option (§7), the DDL surface
  (`CREATE TYPE … AS HOST`, `CREATE FUNCTION … LANGUAGE HOST`), the volatility/cross-core/cost
  declarations, and the new error codes.
- **[../fileformat/format.md](../fileformat/format.md)** — `type_code = 21`, the persisted host-type
  catalog, the host-scalar opaque-framed body, the index resolved-dependency list; bump
  `format_version` to **30**.
- **[../functions/catalog.toml](../functions/catalog.toml)** — confirm the runtime function registry
  reuses the entry shape; the `cost` field is already live.
- **[../errors/registry.toml](../errors/registry.toml)** — `42723` duplicate_function (identical
  signature only); reuse `XX002` (already registered by collation, [collation.md](collation.md)) for
  the graded reopen refusal; `0A000` for not-yet-keyable rungs; `22P02` for host text-in failures.
- **[compatibility.md](compatibility.md)** — register host code as a manifest capability kind (`host`,
  already sketched there §6) and the per-object graded verdict (§11 here).
- **[indexes.md](indexes.md)** — the resolved-dependency persistence for expression/partial index keys
  (§8.1).
- **[conformance.md](conformance.md)** — the host-extension harness (§12), the `X` render tag, the
  compare/key invariant fixture.
- **[determinism.md](determinism.md)** — host extensions as host-owned ledger entries under the same
  default-deny/containment model (§10 here).
- **[gin.md](gin.md) / [gist.md](gist.md)** — the opclass routines as a registered component kind (§8.2).
- **[README.md](README.md)** — keep this doc in the index.

---

## 16. Status summary

| Section | Subject | Status |
|---|---|---|
| §1 | Catalog frame — jed already has the method-set, inlined | **proposed** (restates existing structure) |
| §2 | Determinism-ownership is the line that moves | **proposed** (the governing principle) |
| §3 | The `TypeExpr` model + the capability ladder + the closed container axis | **proposed** (composite arm is **landed**) |
| §4.1 | Composite types (derived codec, G2 free, self-describing) | **landed as a type**; composite-**as-key** proposed (recommended early) |
| §4.2 | Host scalar functions (registry, signature overloads, vectorized, cost, volatility) | **landed** (all 3 cores): ephemeral registry + resolve + eval seam, exact-signature overloading, cost charged/gated, strict, wrong-type-caught, 42723. Vectorized ABI, non-strict, container args, volatility/cross-core *enforcement* deferred (§14 step 3) |
| §4.3 | Host scalar types (Storable→Indexed ladder, `type_code 21`, opaque) | **proposed** |
| §5 | Dispatch — registry the many, inline the few (§5.1 splits the **seam** from the **dogfood**; function seam first) | **built** for built-in scalar functions + aggregates *and* the **host function injection seam** (§14 step 3, all 3 cores): a host kernel is reached by id through the frozen registry alongside the inlined built-in arms. Type-vtable depth (Fork A) + the type-method seam (step 5) **proposed** |
| §6 | Persisted host-type catalog + on-disk representation (`type_code 21`, `format_version 30`) | **proposed** |
| §7 | Registration (ephemeral registry) vs. schema (DDL) + 5-field component identity | **proposed** |
| §8 | The index connection — expression indexes (resolved-dependency persistence) + opclass registry | **proposed** (§8.1 is the recommended first index connection) |
| §9 | Cost — cooperative accounting + admission, **not** a sandbox | **proposed** (adopts [cost.md](cost.md) §6) |
| §10 | Determinism ledger + containment for host code | **proposed** (extends [determinism.md](determinism.md)) |
| §11 | Graded per-object missing-on-reopen verdict | **proposed** (registers into [compatibility.md](compatibility.md); collation instance **landed**) |
| §12 | Conformance — host-authored corpus + byte fixtures + the compare/key invariant | **proposed** |
| §13 | Open forks (type-vtable depth; cross-core stance; host container kinds; host opclasses) | **unresolved** — maintainer's call (F1/F5 recommendations given) |
| §14 | Delivery order | **proposed** |
