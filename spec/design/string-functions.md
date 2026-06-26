# String / text scalar functions — design

> The reasoning behind jed's string-processing built-ins (PostgreSQL's "String Functions
> and Operators", PG manual §9.4). The **catalog is authoritative**
> ([../functions/catalog.toml](../functions/catalog.toml)); this doc is the *why* and the
> per-function semantics. When a decision here changes, change it in the catalog and here in
> the same edit. Read [functions.md](functions.md) §9 first — these all reuse the scalar-
> function mold (`[[operator]]`, `kind = "function"`).

## 1. Scope & shape

These are the per-row, pure, side-effect-free text functions (CLAUDE.md §13): each maps
its argument values to one output value and touches nothing else. They are all
**`kind = "function"`** rows resolved through the generic scalar path
([functions.md](functions.md) §9, `resolve_scalar_func` / `resolveScalarFunc`): the overload
is picked by argument families, the result type by the catalog `result` code, the kernel by
name, and **NULL propagates** at eval (`null = "propagates"` — any NULL argument → NULL,
short-circuited before the kernel runs). No new resolved-expression node is needed — they
ride `RExpr::ScalarFunc` / `reScalarFunc` / `scalarFunc` like `abs`/`round`. Each charges one
`operator_eval` (the uniform per-call weight) plus its arguments' own costs.

PostgreSQL is the behavioral default (CLAUDE.md §1) and every one of these is oracle-pinned
against `postgres:18` — they live on the comparable surface, so the corpus rows are imported
from the live oracle (`rake corpus:import`) and any deliberate divergence is recorded here.

## 2. The character-unit decision — code points, and the cross-core trap

PostgreSQL's character-oriented string functions count and index by **character**, which
under the server encoding `UTF8` means a **Unicode code point** — *not* a byte and *not* a
UTF-16 code unit. jed's one collation is `C` over UTF-8 ([types.md](types.md) §11), so jed
matches PG by counting **code points**:

- **Character-counting / character-indexing** functions — `length`, `char_length`,
  `character_length`, `substr`, `left`, `right`, `lpad`, `rpad`, `reverse`, `strpos`,
  `split_part`, `position` — operate on the code-point sequence.
- **Byte / bit functions** — `octet_length`, `bit_length` — operate on the **UTF-8 encoded
  bytes** (`bit_length = octet_length × 8`).

This is a §8 cross-core divergence hotspot. Rust `String` and Go `string` are UTF-8, so
code-point iteration is `chars()` / `for _, r := range s` and byte length is `len(s)` /
`s.len()`. **TypeScript strings are UTF-16**, so a naïve `.length` would count UTF-16 code
units (wrong for astral characters, which are a surrogate pair) and a naïve byte indexing
would be wrong everywhere non-ASCII. The TS core therefore iterates code points
(`[...s]` / `for (const ch of s)`, which the spec's iterator yields per code point) and
computes byte length / bytes through a UTF-8 encoder (`TextEncoder`). The same trap the
collation `ORDER BY` work handled (memory: *Unicode test authoring*) — the corpus exercises
an astral character (e.g. `U+1F600`) so a UTF-16-unit bug is caught.

## 3. Per-function semantics

### `length(text) → int`

The number of **characters** (code points) in the string. `length('héllo') = 5`
(é is one code point), `length('') = 0`, `length('𝄞') = 1` (one astral code point, two UTF-16
units — the TS trap). STRICT: `length(NULL) → NULL`. Result is `int` (i32); a realistic
string never exceeds the i32 range, matching PG's `int4` result. PostgreSQL also defines
`length(bytea)` and an encoding-name 2-arg form; jed implements the `text` overload (the
byte count is `octet_length`).
