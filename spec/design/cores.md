# Cores vs. wrappers — design

> When does a new implementation earn its place, and which languages are worth the cost?
> CLAUDE.md §2 commits to **multiple native cores, no reference implementation**, with a
> priority list (Rust → Go → JS/TS → Java/C# → Swift) and a noted exception for wrapping
> Rust. This doc makes the *selection rule* behind that list explicit: a new
> implementation is judged by **how much new divergence it can surface**, not by how
> popular its language is or how likely it is to ship. It exists to stop a plausible-but-
> wrong instinct ("more implementations = more coverage, so add Ruby/JS early") before it
> costs N-times implementation effort for ~zero contract value.

The honesty mechanism (CLAUDE.md §2) is **divergence under a shared contract**: two
maximally-different implementations evolving together turn every spec ambiguity into a
failing test the day it is written. Everything below follows from taking that literally.

## 1. The principle: a core only earns its place if it can *disagree*

The value of an implementation, *for the spec*, is proportional to the new divergence it
can surface. The corollary is sharp:

> **A wrapper cannot disagree with the thing it wraps, because it *is* that thing.**

A Ruby gem over the Rust core, or a JS package over Rust→WASM, runs the exact same
parser / planner / executor / storage bytes. It surfaces **zero** new *semantic*
divergence. What it does test is the FFI/binding seam and packaging — real value for
**shipping** to an ecosystem, no value for **spec-hardening**.

This is the same reasoning CLAUDE.md §2 already applies to Swift ("wrapping the Rust core
is an acceptable *deliberate exception*, not a continuation of the rule"). This doc just
generalizes it: the rule is not "Swift is special," it is "wrappers don't harden the
contract — only independent reimplementations do."

## 2. Two buckets: conformance participants vs. distribution artifacts

Keep these separate; they answer different questions and are funded by different budgets.

| Artifact | Purpose | New divergence | Counts as a "core"? |
|---|---|---|---|
| Rust core, Go core | Harden the spec (§2) | High — the point | Yes |
| A *native* TS / Java / C# core | Harden the spec | Medium — **if** new axis (§3) | Yes |
| Ruby gem → Rust | Ship to Ruby | None | No — distribution |
| JS package → Rust-WASM | Ship to browser/Node | None | No — distribution |

A distribution artifact is still worth building — *when you want the ecosystem*. It is
tested as a binding/packaging layer, and it **never** participates in the conformance
corpus as an independent voice (it would only ever echo the core it wraps). Do not write a
native engine in a language *for the spec* unless that language clears the §3 bar.

## 3. "Which language" = "which new axis of divergence"

The marginal value of core #3 is not "another language" — it is **a data model the
current cores cannot disagree about because they agree by construction.** The axes that
actually generate spec-leakage:

- **Numeric tower.** Rust and Go *both* have native fixed-width two's-complement integers,
  so they agree on `int16/int32/int64` and overflow (CLAUDE.md §4/§8) almost for free.
  **JavaScript has no native int64 — only `f64` + `BigInt`.** A core whose only number is
  `f64` *forces* the spec to confront integer semantics the current pair quietly satisfies.
  This is the single highest-yield axis not currently exercised.
- **String encoding.** Rust and Go are both **UTF-8**. Java, C#, and JS are **UTF-16**
  internally. The moment `text` enters the type system (collation, length, codepoint
  ordering — CLAUDE.md §8), a UTF-16 core tests whether "codepoint order" was actually
  nailed down or merely happened to work in two UTF-8 cores.
- **Decimal / float formatting** (CLAUDE.md §8) — already the worst offender; a third
  language's number printer is another independent vote on the rule.

Ranked by *new* axis (not popularity, not ship-likelihood):

1. **JS/TS, native** — `f64`+`BigInt` numerics *and* UTF-16 strings: maximally unlike
   Rust/Go on the two axes that matter, **and** the browser is the one genuinely distinct
   *target environment* on the roadmap. Top pick by a clear margin.
2. **Java / C#, native** — UTF-16, JIT, checked/unchecked arithmetic, Java's historic
   no-unsigned. Solid, and they are the "every modern environment" coverage (CLAUDE.md §2)
   — but a second UTF-16/managed pair after JS has diminishing axis-yield.
3. **Python / Ruby** — arbitrary-precision integers are *an* axis, but neither is a target
   *environment* the project has named, so both are gem/package-over-Rust territory, not
   cores. (Ruby's runtime model — GC'd, dynamic — is nothing Go does not already cover.)

The takeaway: **Ruby and JS would *ship* as wrappers, and that is exactly why a *native*
version of them adds nothing — unless the native runtime's data model is uniquely
divergent.** That is true for JS (UTF-16 + f64/BigInt + the browser) and false for Ruby.
JS is the interesting case; Ruby is not.

## 4. Timing: "early" means *type-system-early*, not *calendar-early*

Both directions of the §2 logic are real; name them honestly.

- **For adding a divergent core early:** ambiguities are cheapest to fix while the spec is
  soft; a third core hardens foundational decisions (key encoding, file format, integer
  semantics) *before* they ossify.
- **Against:** CLAUDE.md §2 itself says later cores "reveal far fewer new ambiguities,"
  two cores already do "the bulk of the honesty work," and §5 names parser/planner/
  executor/storage as the irreducibly *per-language* cost. A third core multiplies the
  porting tax on **every** future vertical slice — precisely while the slice surface is
  changing fastest.

The synthesis: **the unique divergence axes of a third core do not bite until the type
system exercises them.** While the engine is integer-only (the current state — CLAUDE.md
§4 first step, §11 step 5/5b), there is no `text` (so UTF-16-vs-UTF-8 cannot disagree),
no `decimal`/`float` (no formatting fight), and integers are exactly where Rust+Go agree
by construction. Adding a third core *now* would mostly re-prove what the current pair
already proves, while taxing every slice.

**Trigger, therefore, is a milestone, not a date:** add core #3 when the type system grows
past integers — specifically when `text` (encoding/collation) and `decimal`/`timestamp`
(formatting) land (CLAUDE.md §4 deferred scalars). That expansion is the first moment a
native UTF-16, `f64`-only core catches what the current pair structurally cannot.

## 5. The one genuinely open choice: native TS vs. Rust→WASM for the browser

CLAUDE.md §2 leaves JS "TBD" between a native TS implementation and a Rust→WASM wrap. §1–4
above resolve everything *except* this, because the two goals pull apart:

- For **divergence** (hardening the spec): only a **native** TS core helps; a WASM wrap is
  the Rust core and surfaces nothing.
- For **shipping** (a browser/Node artifact): a **Rust→WASM** wrap is the cheap, correct-
  by-construction path.

These are not mutually exclusive. The defensible split is: **maintain a native TS core as a
conformance participant *and* ship the browser build as Rust→WASM.** The alternative —
ship WASM only, no native TS — is legitimate, but it means the browser target receives
**zero** divergence-testing. That should be a deliberate choice, not a default fallen into.
**This is the decision reserved for the maintainer**; everything else in this doc follows
from the §1 principle.

## 6. Current recommendation / status

As of CLAUDE.md §11 step 5b (integers only, `CREATE`/`INSERT`/`SELECT` alive, file-format
round-trip done):

1. **Do not build any wrapper-destined core for the spec.** Ship Ruby/JS as wrappers
   (gem→Rust, package→Rust-WASM) *when the ecosystem is wanted*; treat them as
   distribution + binding-seam tests, never as conformance voices.
2. **Hold at two cores (Rust + Go) through the integer-only slices.** They are saturating
   the divergence available there.
3. **When `text` and `decimal` land, add a *native* TS core** as core #3 — highest new-axis
   yield (UTF-16 + f64/BigInt) plus tooling for the one distinct runtime (browser).
4. **Decide §5 explicitly** at that point: native-TS-for-conformance *and* WASM-for-shipping,
   versus WASM-only with the browser untested for divergence.

This doc records the *rule*; CLAUDE.md §2 remains the canonical priority list. If the rule
here and §2 ever conflict, fix both in the same change (per the CLAUDE.md preamble).
