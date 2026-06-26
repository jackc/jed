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

### `char_length(text) → int`, `character_length(text) → int`

SQL-standard **aliases** of `length(text)` — the same code-point count, the same kernel.
PostgreSQL exposes all three names; jed routes `char_length`/`character_length` to the
`length` kernel (the resolver aliases the name, like `power`→`pow`). The `CHAR_LENGTH(x)`
keyword-call syntax is not special-cased — they are ordinary function names.

### `octet_length(text) → int`

The number of **bytes** in the UTF-8 encoding — `octet_length('héllo') = 6` (é encodes as
two bytes), `octet_length('') = 0`, `octet_length('😀') = 4`. The byte counterpart of
`length`. Rust/Go take the byte length of the UTF-8 string directly (`String::len` /
`len(s)`); TS computes it through the shared UTF-8 encoder (`utf8ByteLength`), since a JS
string is UTF-16 and `.length` would be neither bytes nor code points. PostgreSQL also
defines `octet_length(bytea)`; jed implements the `text` overload.

### `bit_length(text) → int`

The number of **bits** in the UTF-8 encoding — `octet_length × 8`. `bit_length('héllo') = 48`,
`bit_length('') = 0`. Same code path as `octet_length`, multiplied by eight.

### `substr(text, start [, count]) → text`

The **function** spelling of `SUBSTRING` (jed's `SUBSTRING … FROM … FOR` syntax is separate);
1-based and **code-point indexed** (`substr('héllo', 2, 3) = 'éll'`). Two overloads:

- `substr(s, start)` — the characters from position `start` to the end of the string.
- `substr(s, start, count)` — the `count` characters starting at `start`: the window
  `[start, start+count)` intersected with the valid range `[1, n]`.

A `start ≤ 0` or past the end **clips** rather than erroring, matching PostgreSQL:
`substr('alphabet', 0, 3) = 'al'` (the window `[0, 3)` keeps positions 1–2),
`substr('alphabet', -2, 5) = 'al'`, `substr('alphabet', 100, 2) = ''`,
`substr('alphabet', 5, 100) = 'abet'`. A **negative `count`** traps **`22011`**
(`substring_error`, *"negative substring length not allowed"*) — PostgreSQL's exact code. Any
NULL argument propagates. The shared per-core kernel works on a code-point vector
(`chars().collect()` / `[]rune` / `[...s]`) and computes the window with a saturating add so a
huge `start + count` cannot overflow (TS bigint is already exact). PostgreSQL's `substr` accepts
`bigint` positions; jed's `integer` family accepts any width (a bare integer literal is `i64`),
so `substr('x', 1, 2)` resolves directly without an int4 cast.

### `left(text, n) → text`

The first `n` characters (code points). A **negative** `n` returns all but the last `|n|`
characters: `left('abcde', 2) = 'ab'`, `left('abcde', -2) = 'abc'`, `left('abcde', 0) = ''`,
`left('abcde', 10) = 'abcde'`, `left('abcde', -10) = ''`. The kernel takes `chars[..end]` where
`end = clamp(n < 0 ? len+n : n, 0, len)` (a saturating add so an extreme negative `n` cannot
underflow). NULL args propagate.

### `right(text, n) → text`

The mirror of `left`: the last `n` characters (code points). A **negative** `n` returns all but
the first `|n|`: `right('abcde', 2) = 'de'`, `right('abcde', -2) = 'cde'`, `right('abcde', 0) = ''`,
`right('abcde', -10) = ''`. The kernel takes `chars[start..]` where
`start = clamp(n < 0 ? |n| : len-n, 0, len)` (`checked_neg` guards `i64::MIN` so the magnitude
cannot overflow). NULL args propagate.

### `lpad(text, length [, fill]) → text`

Pad on the **left** to `length` characters (code points) using `fill` (taken cyclically; default a
single space), truncating a longer string to its first `length` characters:
`lpad('hi', 5) = '   hi'`, `lpad('hi', 5, 'xy') = 'xyxhi'`, `lpad('hi', 1) = 'h'`,
`lpad('hi', 0) = ''`, `lpad('hi', 5, '') = 'hi'` (an empty fill cannot pad). NULL args propagate.

**Resource bound (CLAUDE.md §13).** `lpad`/`rpad` (and `repeat`) *amplify* — a small input can
request a huge output — so a `length` above `MAX_RESULT_CHARS` (PostgreSQL's `MaxAllocSize`,
`0x3FFFFFFF`) traps **`54000`** (`program_limit_exceeded`, *"requested length too large"*), exactly
PostgreSQL's behavior, bounding the allocation an untrusted query can demand. (Per-character cost
metering so the `max_cost` ceiling also bounds a sub-cap-but-still-large pad is a deferred follow-on;
the hard cap is the backstop.)

### `rpad(text, length [, fill]) → text`

The right-hand mirror of `lpad`: pad/truncate on the **right**. `rpad('hi', 5) = 'hi   '`,
`rpad('hi', 5, 'xy') = 'hixyx'`, `rpad('hello', 3) = 'hel'`. Shares the `pad_chars` kernel
(`left = false`) and the same `54000` length cap. NULL args propagate.

### `btrim(text [, characters]) → text`

The **function** spelling of `TRIM(BOTH characters FROM text)`: remove from **both** ends the
longest run of characters that each appear in the `characters` **set** (a set of code points, *not*
a substring; default a single space). `btrim('xxhixx', 'x') = 'hi'`, `btrim('  hi  ') = 'hi'`,
`btrim('héllo', 'ho') = 'éll'`, `btrim('abc', '') = 'abc'` (an empty set trims nothing). The shared
`trim_chars` kernel builds a code-point set and walks each chosen end; `ltrim`/`rtrim` reuse it with
one side disabled. NULL args propagate.

### `ltrim(text [, characters]) → text`

Like `btrim` but trims only the **leading** (left) run — the function form of
`TRIM(LEADING characters FROM text)`. `ltrim('xxhixx', 'x') = 'hixx'`, `ltrim('  hi  ') = 'hi  '`.
Reuses `trim_chars` with `do_right = false`. NULL args propagate.
