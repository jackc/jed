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
> **Status: a framework + proposal — with the §5 dispatch foundation now built.** Resolution for
> the **built-in** named scalar functions and aggregates is now **data-driven over the generated
> catalog tables** (`OPERATORS` kind=`function`, `AGGREGATES`) in all three cores: the old
> known-name gate + result-type match + name→variant match collapsed into one registry lookup
> plus small shared result-code / plan interpreters, with the per-row kernel still reached by id
> (`ScalarFunc` / `AggPlan`, hand-written per core, §5). It is **behaviour-preserving** — the
> conformance corpus is byte- and cost-identical (the full corpus passes × rust/go/ts) and a per-core cross-check
> test pins the registry to the catalog. Everything else here remains a **proposal**: host scalar
> functions (§4.2), composite types (§4.1), host scalar types (§4.3), the persisted type catalog
> (§6), host-code cost metering (§7), the host determinism ledger (§8), and the host-extension
> conformance harness (§9). The doc defines the target shape, ties it to the existing
> determinism/cost/conformance contracts, and records two open forks (§10) for the maintainer. It
> is the Phase 9 design referenced by [TODO.md](../../TODO.md) ("Design the host-function API";
> "Host-defined functions must contribute to the cost system"). When a section is ratified, make
> the downstream edits in §11 in the same change.

> **Governing principle — the host-extension boundary ([CLAUDE.md](../../CLAUDE.md) §13).** Read
> this whole doc through one lens: jed's fundamental guarantees (cross-core byte-identity,
> self-describing files, single-file storage, transaction boundaries, untrusted-query safety) bind
> **jed's own engine and built-in surface**, not host extensions. A host may relax any of them for
> its own extension as it judges appropriate and **owns the consequences** — the whole point is to
> *enable* PostGIS-class extensibility, not to police it. So wherever this doc reads as jed
> *enforcing* a guarantee on a host — the cost gate (§7), the `XX002` reopen failure (§4.3/§6),
> taint containment (§8), the conformance harness (§9) — read it as a **safe default plus an
> opt-in tool that protects jed's *core* guarantees and that the host may relax**, never a mandate
> jed imposes on what the host builds. jed's job is the stable foundation and a clean seam; the
> freedom above it is the host's. §2 (the line that *moves*, not vanishes) is this principle in
> full.

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
| `typmodin` / `typmodout` | the `numeric(p,s)` typmod stored in the catalog column entry | [../fileformat/format.md](../fileformat/format.md) catalog section |
| canonicalize | decimal/interval value-canonicalization, NaN-on-store ([float.md](float.md) §10) | hand-written per core |

So [../types/scalars.toml](../types/scalars.toml) is **already a pg_type-style catalog**: each
`[[type]]` row carries `id`, `aliases`, `family`, `storable`, an `encoding.method` *name*, a
`collation`, a render tag, and parameterization caps. The data-over-code split (CLAUDE.md §5) is
already drawn — the *descriptor* is shared data, the *method bodies* are hand-written per core
(this is the same boundary [codegen.md](codegen.md) draws for the function catalog).

**The one thing missing is the indirection.** Today the method *name* (`"uuid-raw16"`,
`"int-be-signflip"`) resolves to a kernel by a hardcoded `match` on the in-memory `Type` enum.
Extensibility is the act of routing that resolution through a **registry** that the host can
also populate — exactly PG's `pg_type` → `pg_proc` indirection. Once a type is "a catalog entry
whose methods resolve through a registry," a host-supplied type and a built-in type are the
**same kind of thing** to the engine. That is the blur the maintainer wants, and it is a small
conceptual step from where the code already is.

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
> owns G2/G3** — jed offers the harness (§9) as an *opt-in tool* to prove it cross-core, and
> containment (§8) as a *safe default* that keeps an undischarged relaxation from silently reaching
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

## 3. The unified type-method interface

A type is a value of this method-set. For built-ins, each method *is* the existing hand-written
kernel; for host types, it is registered code. The interface is identical either way — that is
the "catalog model."

| Method | Signature (conceptual) | Built-in source | Required for tier |
|---|---|---|---|
| **text-in** (`typinput`) | `(text, typmod) → value \| 22P02` | literal parser | storable |
| **text-out** (`typoutput`) | `value → text` (the canonical render) | renderer | storable |
| **bin-out** (`typsend`) | `value → bytes` (value-codec body, after the presence tag) | value encoder | storable |
| **bin-in** (`typreceive`) | `bytes → value` | value decoder | storable |
| **canonicalize** (optional) | `value → value` (collapse equal representations; NaN-on-store) | per-type | storable |
| **compare** | `(a, b) → Lt \| Eq \| Gt` (total order; 3VL equality derived) | comparator | comparable |
| **key-encode / key-decode** | `value ↔ order-preserving bytes` | the `encoding.method` rule | indexable |
| **typmod-in / typmod-out** (optional) | parameter parse/format (`numeric(p,s)`) | typmod handling | parameterized only |

Two cross-cutting properties the registration also declares (they are not methods but contracts):

- **cross-core-deterministic?** — does the host assert (and the harness verify, §9) that
  text/bin/compare/key are byte-identical across the cores it ships? Default **no** → the type
  is tainted (§8).
- **cost contribution** — see §7 ([cost.md](cost.md) §6); required on any handle with a non-zero
  `max_cost` (the untrusted-query surface, CLAUDE.md §13).

The render tag for any host type is **`X`** (proposed, a new tolerant-but-exact-by-default tag):
its text-out is compared verbatim like `T` when the type is declared cross-core-deterministic,
and like the float `R` tag (host-supplied tolerant compare) when it is not.

---

## 4. The three kinds of extension, ranked by how cleanly they land

### 4.1 Composite types — safest; derived codec; G2 free (do this first)

`CREATE TYPE addr AS (street text, zip i32)`. The host supplies only the **shape** — a field
list of *existing* types (built-in or previously-defined composite). **No host codec code at
all.** jed derives every method compositionally from fields that are already cross-core-identical:

- **bin-in/out** = a NULL-bitmap over the fields followed by each field's value-codec body, in
  declaration order. One generic codec, parameterized by the field list — not code per type.
- **compare** = lexicographic over fields (PostgreSQL row-comparison semantics); 3VL `=` matches
  PG (`ROW(1,NULL) = ROW(1,2)` is NULL), the *sort* order is total (NULLs per the field rule).
- **key-encode** = compose the per-field key encodings — **this machinery already exists** as the
  composite-PRIMARY-KEY encoding ([encoding.md](encoding.md) §2.3, the nullable presence tag §2.2).
  A composite *type* in a key is the same problem already solved for composite keys.
- **text-out** = a generic `(a,b,c)` renderer with PG's field-quoting rules; **text-in** parses it.

Because every method is derived from cross-core-identical parts, **G1–G3 hold by construction** —
no ledger entry, no harness obligation. A further payoff: a composite value is **self-describing
and portable** — its field list is persisted in the type catalog (§6), so *any* jed can read a
file containing it without the host's code. (Contrast host scalars, §4.3, which bind a file to
the host's code.) This is why composites are both the safest and the most portable extension, and
why they go first.

### 4.2 Host-defined scalar functions — no type-system change; high value

These operate over **existing** types, so they touch the function catalog, not the type catalog.
Reuse the *shape* of an [../functions/catalog.toml](../functions/catalog.toml) `kind = "function"`
entry (name, `arg_families`, `result`, `null`, `arg_names`/`arg_defaults`), but as a **runtime
function registry** the resolver consults **after** the codegen'd static `OPERATORS` table.
Built-in functions stay codegen'd and win on collision (a host name colliding with a built-in is
rejected at registration — propose `42723 duplicate_function`). Three things registration must
carry beyond the catalog shape:

- **A vectorized / batched signature** — column-in → column-out, not row-at-a-time. This is the
  single decision [cores.md](cores.md) §2.1 says keeps a *wrapped* core viable (it amortizes the
  per-row FFI upcall for Swift/Java/C#). Bake it in from the first version or wrapping is
  foreclosed for every future reach language. A scalar-per-row convenience wrapper can sit on top;
  the *engine-facing* contract is batched.
- **A cost contract** — one of the three [cost.md](cost.md) §6 designs (§7 below). Without one,
  the function is admissible **only** on a handle with `max_cost = 0` — i.e. *not* the
  untrusted-query surface (CLAUDE.md §13).
- **Volatility + cross-core declaration** — two independent axes:
  - **volatility** (`immutable` / `stable` / `volatile`, PG's notion) governs planning: only an
    `immutable` function may later back an index expression or be constant-folded.
  - **cross-core-deterministic?** (default **no**) governs G2: a function that is `immutable` can
    *still* be non-cross-core (e.g. it calls the platform libm). Only a function declared *and
    harness-verified* cross-core-deterministic produces untainted results (§8).

### 4.3 Host-defined core (scalar) types — reasonable; tier it, the host picks the tier

The hard case: the host supplies text/bin/compare/key directly; jed derives nothing. Make host
scalar types follow the **exact staging jed used for its own types**. Recall jed shipped each
non-integer type storable-first and rejected key use with `0A000` until the order-preserving
encoding was ready — uuid was the first non-integer `PRIMARY KEY`; text/bytea/decimal/interval
*still* reject PK with `0A000` ([TODO.md](../../TODO.md) Phase 3, [encoding.md](encoding.md)). Host
types reuse that ladder:

| Tier | Host supplies | On-disk | Keys/index | Difficulty |
|---|---|---|---|---|
| **1 — storable opaque** | text-in/out, bin-in/out | `type_code = 14` (extension) + a **length-framed opaque body** jed round-trips without understanding | `0A000` | easy — enough for "store my domain value and get it back" |
| **2 — comparable** | + `compare` | (as tier 1) | `ORDER BY`/`WHERE` use the host `compare` (a metered call); no key bytes | medium |
| **3 — indexable** | + order-preserving `key-encode`/`key-decode` | (as tier 1) | PK/index allowed; key bytes are the cross-core obligation | hard — the full G2/G3 byte contract lands here |

Rows carry **no per-value type tag** — a column's type comes from the catalog ([../fileformat/format.md](../fileformat/format.md)),
so tier-1 slots in cleanly: jed frames the body (length prefix via the presence-tag machinery) and
treats it as opaque bytes. The cross-core byte obligation (§2) bites **only at tier 3**, and only
for a multi-core host — most host scalar types never leave tier 1–2.

**Durability caveat (the asymmetry vs composites).** A file using a host scalar type is **not
self-describing**: reopening it requires the host to re-register that type's codec (by name +
version, §6). Without it, the column cannot be decoded — a hard error (propose `XX002
extension_type_unavailable`), never silent corruption. A codec **version** is part of
registration; a version mismatch is the same hard error, mirroring `format_version`. This is the
real cost of host scalar types and the reason composites (self-describing) are preferred whenever
the value is expressible as a tuple of existing types.

---

## 5. Dispatch — registry the many, inline the few

> **Built (built-ins).** The "registry the many" half of this section is implemented for the
> built-in scalar functions and aggregates, in all three cores. The known-name gate
> (`is_scalar_func_name` / `is_aggregate_name`), the result-type match, and the name→variant match
> are gone, replaced by a catalog scan keyed by `(name, arg_families)` over the generated
> `OPERATORS` (kind=`function`) / `AGGREGATES` tables plus small shared result-code / plan
> interpreters; the per-row kernel is still reached by id (`ScalarFunc` / the `AggPlan`),
> hand-written per core. `make_interval` keeps its dedicated named/defaulted resolver (§11) but is
> gated through the registry like the rest. The core operators stay monomorphized (unchanged), and
> the **built-in type method-set is not yet dogfooded** through the registry — that is Fork A
> (§10), still open. Adding a built-in function is now: a catalog row + a hand-written kernel + (per
> core) one kernel-id entry; a per-core cross-check test fails if a catalog row has no kernel id or
> an unhandled result code. Host registration into the same table (§4.2) is still a proposal.

"Route everything through a registry" and "keep everything as hand-written `match`" are both
wrong because they treat *dispatch* as one decision. It is **four sites** with sharply different
cost profiles, and the right answer differs per site. The trap to avoid is the one this doc made
in an earlier draft: conflating the **function-name case statement** (which is mostly *cold*, and
whose real problem is maintenance, not speed) with the **per-value type method-set** (which is the
genuinely hot, inline-sensitive loop). They earn opposite verdicts.

A scalar function's name appears in **four** hand-written sites per core today
([executor.rs:5834](../../impl/rust/src/executor.rs) the known-name gate,
[:6020](../../impl/rust/src/executor.rs) result-type resolution,
[:6072](../../impl/rust/src/executor.rs) name→`ScalarFunc` variant,
[:8630](../../impl/rust/src/executor.rs) variant→kernel) — and **three of the four are
resolve-time (cold, once per query)**. The fourth, the per-row one, is a `match` on a small enum
that the compiler lowers to an **O(1) jump table**, not a linear string scan. So the thing that
makes a giant function set scary is *not a hot-path speed problem* — it is a **maintenance and
cross-core-drift** problem: 4 sites × 3 cores = 12 hand-edited points per function, growing
linearly, exactly the drift the data-over-code rule (CLAUDE.md §5) exists to kill.

| Dispatch site | Fires | Count | Indirection cost | Verdict |
|---|---|---|---|---|
| **Resolution** (name → kernel + result type) | cold, per query | hundreds | none — it's cold; a table lookup beats a 4-way hand-match | **registry** — strictly better, advances §5 |
| **Named-function eval** (`abs`, `lower`, `round`, …) | per row | hundreds | indirect call ≈ **<1%** of a kernel doing real decimal/string/float work | **registry** — PG-cheap, the right move |
| **Core operators** (`+ − * / = < > AND` …) | per row | ~a dozen | replaces an inlined ~1-instruction kernel with a real CALL → loses inlining/vectorization | **keep monomorphized** |
| **Type method-set** (compare / codec) for built-ins | per *value*, in sort/scan inner loops | ~10 methods × ~14 types | same — a fixed-width int compare is one instruction; wrapping it in a call is a 3–10× hit *on that op* | **keep monomorphized** (benchmark-gated, see below) |

**Why this is not a real conflict.** The set that must stay inlined is **small and bounded** (a
dozen operators + a fixed type method-set); the set that is "hundreds" is **exactly the set that is
fine — even better — through a registry**. "Scales to hundreds of functions" and "keep the hot loop
fast" do not fight, because the *many* are coarse-grained real-work calls and the *few* are
fine-grained hot kernels. So the recommendation is **registry the many, inline the few**:

- **Registry-ize resolution and named-function / aggregate evaluation.** Build a runtime function
  registry keyed by `(name, arg_families)`, populated from the codegen'd catalog descriptor
  ([codegen.md](codegen.md)). Resolution *consults the catalog data* instead of re-encoding the
  name set in three hand-written matches — so this is **more** §5-aligned, not less. Host functions
  (§4.2) register into the same table; a host name colliding with a built-in is rejected. The
  per-row kernel is reached by id/fn-pointer; the kernels stay hand-written per core (§5 still
  forbids codegenning them). This is PG's `fmgr` model and PG-cheap (real-work kernels amortize the
  indirect call), and it is the structure that actually scales to hundreds of functions.
- **Keep the dozen core operators and the built-in type method-set monomorphized.** The in-memory
  `Type` gains `Extension(TypeId)` / `Composite(TypeId)` arms that dispatch through the registry;
  built-in arms stay inlined `match`. CLAUDE.md §10 ("boring, explicit code; resist deep generics /
  over-interfacing") and the SQLite-footprint benchmark bar ([benchmarks.md](benchmarks.md)) both
  argue here, and *only* here — an indirect call at the bottom of a sort over `i64` keys replaces
  a one-instruction compare and is the one place SQLite-class engines guard.

**On "PostgreSQL routes everything through `fmgr` and is plenty fast" — accurate, with the caveat
that supports the split.** PG calls every function through a cold-set-up function pointer
(`fcinfo->flinfo->fn_addr`), indirectly, per row. It is fast *for PG* because PG is a tree-walking
interpreter whose per-tuple overhead (slot deforming, `FunctionCallInfo` packing) dwarfs the
indirect call — and PG nonetheless shipped **LLVM JIT (PG 11+) precisely because** that
expression/function-dispatch overhead *was* a measurable analytical-query bottleneck, i.e. it
compiles the indirection away when hot. So PG's example justifies registry dispatch for
**functions** (the hundreds), and is *not* evidence for indirection in the one-instruction
comparator (the few).

**The type method-set is benchmark-gated, not a principled no.** The "keep inlined" claim is
load-bearing only for **fixed-width** comparators (`int*`/`uuid`/`timestamp` — compare is one
instruction, inlining is the whole game). For **variable-width** types (`text`/`decimal`/`interval`)
`compare` already does real work (codepoint walk, limb compare), so the indirect call is a small
relative overhead and dogfooding *those* would barely register. Because the dispatch decision is
made at type granularity (`Type::Int64` → arm, `Type::Extension` → registry), keeping built-in
integers inlined costs nothing and forces nothing. If benchmarks ever show the built-in type-method
indirection is in the noise, dogfooding the type vtable too becomes defensible — so §10 Fork A is a
*measurement*, not a doctrine.

Net effect — **the line is erased where it should be (resolution + named functions: hundreds of
built-ins and host functions live in one registry, one mental model, no per-function drift) and
kept where it is real (the dozen core operators and the fixed-width comparators stay inlined, and
the determinism-ownership of §2 is unchanged).**

---

## 6. On-disk + catalog representation

Extensibility introduces what jed has not needed until now: a **persisted type catalog** — the
`pg_type` analog. Today the "type catalog" is the compiled-in [../types/scalars.toml](../types/scalars.toml),
referenced on disk by a stable `u8` type code (1–13; next free **14**, [../fileformat/format.md](../fileformat/format.md)).
Built-in types stay compiled-in and keep their codes. Host/composite types need per-database
persisted definitions:

- **A new `type_code = 14` ("extension").** A column of an extension type stores, in its catalog
  column entry, a reference (a per-database type id / name) into the new persisted type-catalog
  section, instead of relying on the global code alone.
- **The persisted type catalog** records, per host/composite type: its **name**, a **kind**
  (`composite` | `scalar`), a **version**, and — for composites — the **ordered field list**
  (`(field-name, type-ref)` pairs, where a type-ref is a built-in code or another type-catalog id).
- **Composite on-disk body** = NULL-bitmap ‖ field bodies (value-codec, declaration order). Fully
  described by the persisted field list ⇒ **self-describing / portable** (§4.1).
- **Host-scalar on-disk body** = a length-framed opaque blob (§4.3). The persisted entry records
  only name+version; decoding requires the registered codec ⇒ **not self-describing** (§4.3 caveat).

This mirrors PG exactly (built-in and user types coexist in one `pg_type`), and it is the deepest
structural consequence of the feature — call it out in [../fileformat/format.md](../fileformat/format.md)
and bump `format_version` when it lands.

---

## 7. Cost — metered, or it stays off jed's untrusted surface (the host can still run it unlimited)

A host function (and a host type's `compare` / codec) is **opaque to the meter** by default — its
code does not route through `Meter::charge` — which breaks two contracts at once: the
untrusted-query bound (CLAUDE.md §13 — an unmetered call burns unbounded CPU past `max_cost`) and
cross-core cost-identity (G2 on cost — a wrapped core and a native core must accrue the *same*
cost for the same call). The design space is already enumerated in [cost.md](cost.md) §6; this doc
adopts it and extends it from functions to type methods:

- **(a) Declared static weight** — a per-call constant (generalizes the now-live `cost` field in
  [../functions/catalog.toml](../functions/catalog.toml), [functions.md](functions.md) §8). Charged
  once per call like `operator_eval`. Simplest.
- **(b) Declared cost-as-fn-of-args** — a *pure, deterministic* function of argument values/sizes
  (the `decimal_work` / `value_compress` model — cost scales with input), charged **up front and
  guarded before** the call runs.
- **(c) A deterministic `charge(n)` callback** — the host charges as it works, enabling a
  chunk-boundary **mid-call abort**. Must be deterministic and cross-core identical (no wall-clock,
  no allocation/iteration-order dependence — CLAUDE.md §10).

**Type methods need this too:** a host `compare` called inside a sort, or a host codec on a large
value, can dominate a query's cost. A host type/function that declares **none** of (a)/(b)/(c) is
admissible **only** on a handle with `max_cost = 0` (unlimited) — explicitly **not** the
untrusted-query surface (CLAUDE.md §13). That is the same gate already written for host functions
in [TODO.md](../../TODO.md) Phase 9.

---

## 8. Determinism — the host-extension ledger and containment

Host code is the ultimate nondeterminism risk, so it slots directly into the default-deny model of
[determinism.md](determinism.md) §1/§4/§9. The rules:

1. **Default-deny / tainted-by-default.** A host function or type is assumed **not**
   cross-core-deterministic unless declared so *and* verified by the harness (§9). An untainted
   (verified cross-core) host extension keeps G1–G3 like any built-in. A tainted one drops G2/G3
   on its own results — a **class-A or class-I** member, ledgered.
2. **The host owns the ledger entry.** Built-in exceptions live in jed's
   [../conformance/determinism_exceptions.toml](../conformance/determinism_exceptions.toml); a host
   extension is a **host-owned** entry in the host's own extension ledger, stating the same fields
   (surface, class, drops, blast_radius, test). jed offers the containment *tooling*; it does not
   author the host's entry or decide the host's risk appetite.
3. **Containment is the safe default, not a mandate (the §4 invariant).** By default a tainted host
   value does not silently promote into jed's deterministic surface: jed propagates the
   weakest-guarantee taint through expressions and, by default, **fails closed and legibly** —
   marking, or declining to silently flow, a tainted value reaching a `PRIMARY KEY`/index, an
   `ORDER BY … LIMIT` boundary, or a narrowing `CAST` that changes the row multiset — exactly the
   discipline floats already follow ([determinism.md](determinism.md) §4). This default protects
   **jed's own** cross-core guarantee over the rest of the database; it is **not** jed policing the
   host. A host that wants the relaxation — e.g. its non-cross-core type *as* a `PRIMARY KEY`,
   accepting that files using it are no longer cross-core-portable — may opt out for its own surface
   and own that consequence (CLAUDE.md §13). A query that touches no host extension stays fully
   G1–G3 regardless.
4. **Single-core hosts are unaffected by G2.** Taint is about *cross-core* identity. On a single
   core, a host extension that is merely G1 (reproducible on that binary) is fine for everything
   except multi-core distribution — which that host does not do.

---

## 9. Conformance — turn jed's own machinery outward

This is the genuinely new mechanism, and the cleanest answer is to **not invent a new contract** —
extend §7/§8 of [conformance.md](conformance.md). A host extension is "conformant" by the same
means a core is: a corpus it passes and a byte-exact round-trip it survives, except the *host*
authors the cases instead of the spec.

- **jed ships a host-extension conformance harness** — the same sqllogictest runner each core
  already embeds, callable against a database with the host's types/functions registered.
- **The host authors a corpus** for its type/function (`statement ok`, `query`, `statement error`)
  — and, for an **indexable (tier-3) type**, **byte fixtures** for the order-preserving key codec,
  in the [../encoding/](../encoding/) `(value → bytes)` format jed uses for its own types.
- **The extension is conformant iff it passes the host's corpus on *every core the host ships*** —
  byte-identical where it declared cross-core-determinism, tolerant-compare (the `X`/`R` tag) where
  it ledgered an exception. A single-core host runs it on one core; a multi-core host runs it on
  all and *that* run is what discharges the G2/G3 obligation of §2.

So the conformance story is: jed provides the harness, the byte-fixture format, and containment as
a safe default (§8); the host provides the cases and (for multi-core) runs them everywhere, and may
relax what it owns. This is §7/§8 with the authorship inverted, not a parallel system.

---

## 10. Open forks (for the maintainer)

Two decisions materially shape the build and are the maintainer's to make. Recommendations given;
neither is resolved here.

**Fork A — how far the type-vtable dogfood goes.** Note this is *narrower* than the earlier
"layered seam vs full dogfood" binary, which §5 dissolved: the **function** side is no longer a
fork — registry-izing resolution + named-function evaluation is the recommendation outright
(cold/coarse, PG-cheap, scales to hundreds, advances §5). The only thing left open is the
**built-in type method-set** (compare / codec in the per-value inner loops):
- **(recommended) Inline the fixed-width comparators, registry the rest.** `int*`/`uuid`/`timestamp`
  compare/codec stay monomorphized `match` arms (one-instruction ops where inlining is the whole
  game); host types — and, if benchmarks bless it, the variable-width built-ins (`text`/`decimal`/
  `interval`, whose compare already does real work) — dispatch through the registry. Keeps the
  SQLite-footprint hot path (CLAUDE.md §10, [benchmarks.md](benchmarks.md)) while the engine still
  dogfoods the registry for everything but the cheapest ops.
- **Full type-vtable dogfood**: route *all* built-in types — fixed-width included — through the
  registry, so the type-extension path cannot bitrot. Real "can't-rot" argument, but pays the
  indirect call in the one-instruction comparator at the bottom of a sort. This is **benchmark-gated,
  not principled** (§5): land the inlined version, measure the fixed-width comparator regression
  under the registry, and dogfood the rest only if it is in the noise.

**Fork B — cross-core stance for host scalar types.**
- **(recommended) Admit on a single core freely**: a host scalar type is usable immediately on one
  core; the harness (§9) is *optional* and only meaningful when the host goes multi-core. Matches
  "G2 only bites multi-core hosts" (§2).
- **Gate behind the harness**: no host scalar type is blessed until its byte-fixture harness passes,
  even for a single-core host. Safer for the ecosystem (every shipped type is multi-core-ready) but
  taxes the common single-core embedder for a guarantee they do not use.

---

## 11. What ratification changes (downstream edits)

When a section here is ratified, update **in the same change** (mirrors [determinism.md](determinism.md) §10):

- **[CLAUDE.md](../../CLAUDE.md)** — §2 (the §2.1 host-function pivot already anticipates this), §8
  (host extensions as a divergence surface owned by the host), §13 (host code must meter or live at
  `max_cost = 0`). Add a pointer to this doc.
- **[TODO.md](../../TODO.md)** — the Phase 9 "Design the host-function API" and "Host-defined
  functions must contribute to the cost system" items point here; add composite-type and
  host-scalar-type slices, tiered per §4.3.
- **[api.md](api.md)** — the registration surface (register-function, `CREATE TYPE` for composites,
  register-scalar-type), the volatility/cross-core/cost declarations (§4.2/§7), and the new error
  codes.
- **[../fileformat/format.md](../fileformat/format.md)** — `type_code = 14`, the persisted type
  catalog, the composite body layout, the host-scalar opaque-framed body; bump `format_version`.
- **[../functions/catalog.toml](../functions/catalog.toml)** — confirm the runtime function registry
  reuses the entry shape; the `cost` field is already live ([functions.md](functions.md) §8) — a host
  function declares its static weight there.
- **[../errors/registry.toml](../errors/registry.toml)** — register the new codes proposed here
  (`42723` duplicate_function; `XX002` extension_type_unavailable; reuse `0A000` for not-yet-keyable
  tiers, `22P02` for host text-in failures).
- **[conformance.md](conformance.md)** — the host-extension harness (§9), the `X` render tag, and
  how a host corpus + byte fixtures slot into the §7/§8 contract.
- **[determinism.md](determinism.md)** — note that host extensions are host-owned ledger entries
  under the same default-deny/containment model (§8 here).
- **[README.md](README.md)** — add this doc to the index.

---

## 12. Status summary

| Section | Subject | Status |
|---|---|---|
| §1 | Catalog frame — jed already has the method-set, inlined | **proposed** (restates existing structure) |
| §2 | Determinism-ownership is the line that moves | **proposed** (the governing principle) |
| §3 | The unified type-method interface | **proposed** |
| §4.1 | Composite types (derived codec, G2 free, self-describing) | **proposed** — recommended first |
| §4.2 | Host scalar functions (runtime registry, vectorized, cost, volatility) | **proposed** |
| §4.3 | Host scalar types (tiered storable→comparable→indexable) | **proposed** |
| §5 | Dispatch — registry the many (resolution + named functions), inline the few (core operators + fixed-width comparators) | **built** for built-in scalar functions + aggregates (resolution data-driven over the catalog, all 3 cores; behaviour-preserving). Operators stay inlined; the built-in type-vtable depth is Fork A (§10), still open. Host registration into the table is **proposed** (§4.2). |
| §6 | Persisted type catalog + on-disk representation | **proposed** |
| §7 | Cost contribution for host code | **proposed** (adopts [cost.md](cost.md) §6) |
| §8 | Determinism ledger + containment for host code | **proposed** (extends [determinism.md](determinism.md)) |
| §9 | Conformance — host-authored corpus + byte fixtures | **proposed** |
| §10 | Open forks (unification depth; cross-core stance) | **unresolved** — maintainer's call |
