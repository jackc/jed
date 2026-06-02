# Function / operator catalog — design

> The reasoning behind the function / operator catalog. The **catalog is authoritative**
> ([../functions/catalog.toml](../functions/catalog.toml)); this doc is the *why*. When a
> decision here changes, change it in the catalog and here in the same edit, and update
> [CLAUDE.md](../../CLAUDE.md) if it revises a load-bearing commitment.

The catalog is canonical shared data (CLAUDE.md §5): each entry names an operator, its
operand contract, result type, and NULL behavior. It is the single place the operator
semantics are stated, so the per-language cores — and the future codegen "middle path"
that emits their stubs — descend from one contract instead of N hand-written ones.

## 1. Role & scope

Like the grammar ([grammar.md](grammar.md)), the catalog was **backfilled**: the three
cores ([impl/rust](../../impl/rust), [impl/go](../../impl/go), [impl/ts](../../impl/ts))
hand-wrote the comparison and null-test logic in lockstep first, and an authored catalog
followed. So the first version is **descriptive** — it documents exactly the operators
the cores implement today and nothing more:

| Kind | Operators |
|---|---|
| `comparison` | `=` `<` `>` `<=` `>=` |
| `null_test` | `IS NULL`, `IS NOT NULL` |

`<>` / `!=` are deliberately absent — they do not exist in the engine (see
[grammar.md](grammar.md) §4). From here the ordering inverts to spec-first (CLAUDE.md
§10/§11): a new operator is added to the catalog **first**, in the same change that adds
its parser/executor code and conformance entries. The catalog must stay descriptive — it
must not list an operator no core implements, nor omit one a core has.

The catalog defines what operators *do*; it does **not** restate how scalars compare or
promote. That division is load-bearing and is the subject of §4.

## 2. Result types before there is a `boolean`

There is no storable `boolean` scalar type yet ([types.md](types.md) §1 — the scalar set
is integers-only). A comparison therefore produces a **truth value**, not a storable
column value: three-valued `true` / `false` / `unknown`, used only to filter rows in
`WHERE`/`UPDATE`/`DELETE`. The catalog names this result `truth`, a **reserved
non-scalar result id**.

The `result` field is intentionally one field that holds *either* a scalar id from
[../types/scalars.toml](../types/scalars.toml) *or* a reserved id. Reserved ids:

- `truth` — the three-valued truth value above. When the `boolean` scalar type lands, it
  is what `boolean`-returning predicates evaluate to, with `unknown` represented as NULL;
  `truth` is not renamed to `boolean` because the value is not storable until then.
- `promoted` — the common promoted operand type, **reserved for future arithmetic**
  (`int32 + int32 → int32`); no operator uses it yet.

One unified field means every consumer (and the coherence checker) validates the result
the same way — "a known scalar id, or a known reserved id" — whether it is `truth` today,
`promoted` for arithmetic, or `boolean` later.

Both the comparisons and the null tests carry `result = "truth"`. Their difference is not
in the result *type* but in NULL handling (§3): a null test is guaranteed to land on a
definite `true`/`false`, which is expressed by its `null` field, not by a second result
id. Minting a separate two-valued id would duplicate information the `null` field already
carries.

## 3. NULL: propagation vs detection

The three-valued NULL logic itself lives in [../types/compare.toml](../types/compare.toml)
`[null]`. The catalog records, per operator, *which side of it the operator falls on*, in
the `null` field:

- `propagates` — any NULL operand makes the result `unknown`. The comparisons are here:
  `NULL = NULL` is `unknown`, equality is not reflexive across NULL, and a row whose
  predicate is `unknown` is excluded just like `false`.
- `detects` — the operator inspects NULL-ness and **always** returns a definite truth
  value, never `unknown`. The null tests are here: `IS NULL` / `IS NOT NULL` are the
  sanctioned way to observe a NULL.

A third value, `null_safe`, is **reserved** for `IS [NOT] DISTINCT FROM` (NULL-safe
equality) — a comparison whose NULL handling is total rather than propagating. It needs
the `boolean` type and is not authored yet; the checker already accepts the value so the
operator can be added later without touching the checker.

## 4. Operand resolution by reference, not duplication

A single comparison operator accepts many operand pairs: `int16 = int64`, `int32 < int16`,
and so on. The catalog expresses this with **operand families** plus a **resolution
reference**, not an enumerated overload per type pair:

```
arg_families   = ["integer", "integer"]
arg_resolution = "promote"
```

`arg_resolution = "promote"` means "reconcile a mixed-width pair by the promotion tower in
[../types/compare.toml](../types/compare.toml)" — widen both to the common (higher-`rank`)
type, then compare as one integer. The catalog states the operator's *contract*; the
*reconciliation* is deferred to the table that owns it. `compare.toml` already holds the
promotion strategy (`max-rank`), the comparability matrix, and the NULL logic; restating
any of it here would duplicate canonical data and drift.

The rejected alternative was an **enumerated** catalog: one entry per concrete
`(left, right)` pair (~45 rows for five operators over the nine integer pairs). It is
flat but re-encodes the promotion tower into the catalog, grows quadratically as families
(decimal, text, …) arrive, and creates two places that must agree about which pairs are
comparable. Family + reference keeps it one row per operator forever.

The coherence checker ([../functions/verify.rb](../functions/verify.rb)) enforces the
division: every `arg_families` entry must be a real `family` in `scalars.toml`, and a
`promote` resolution must name an operand pair that `compare.toml` actually lists as
comparable with a promotion rule for the family.

## 5. `<=` and `>=` are primitive, definable as Kleene-OR

The cores implement `<=` and `>=` directly, and the catalog lists them as primitive
`comparison` operators with the same family signature as `<` and `=`. They are *equal to*
`(< OR =)` and `(> OR =)` under three-valued (Kleene) OR — which is why a NULL operand
makes them `unknown` exactly as `<` and `=` do: `or(unknown, unknown)` is `unknown`, never
`false`. That equivalence is genuine reasoning, recorded here, but it is **not a data
field**: the catalog describes what the cores do (evaluate a primitive), and a
`derived_from` edge would be the catalog's only derivation, premature machinery for one
case, and would imply a rewrite the cores do not perform.

## 6. Deferred fields and the growth rule

Two fields are designed but **deliberately not authored yet**, so their absence is
intentional, not an oversight:

- `cost` — the deterministic, cross-core-identical evaluation cost of an operator
  (CLAUDE.md §13). It lands with the dedicated cost-accounting-seam item, where the whole
  unit schedule (page reads, rows produced, operator evaluations) is designed together as
  data — not piecemeal as a constant here. Adding a field to a data table later is cheap;
  designing the seam in fragments is not.
- `precedence` — meaningless until a general expression grammar exists ([grammar.md](grammar.md)
  §5: a single WHERE predicate, no nested expressions). It lands with the arithmetic /
  expression slice that first needs it.

Deferred kinds and reserved values, each to be authored spec-first with its own executor
slice and conformance entries ([../../TODO.md](../../TODO.md)):

- `arithmetic` (`+ - * / %`): `result = "promoted"`, trap-on-overflow (`22003`), defined
  divide/modulo-by-zero (`22012`).
- `logical` (`AND` / `OR` / `NOT`): three-valued connectives — waits on the `boolean` type.
- `IS [NOT] DISTINCT FROM`: `null = "null_safe"` — also waits on `boolean`.
- named `function` entries.
