# Regular expressions — design

> Status: **Slice 1 (this doc)** — the engine + the four match operators `~ ~* !~ !~*`.
> **Slice 2** — the scalar functions `regexp_replace` / `regexp_match`. **Slice 3** — the
> Oracle-compat scalar functions `regexp_count` / `regexp_instr` / `regexp_substr` /
> `regexp_like` (§8b). Set-returning functions (`regexp_matches`, `regexp_split_to_table`) and
> the rest of PG's `regexp_*` family are **deferred** (§10). Stores nothing on disk → **no
> `format_version` bump**.

jed adds POSIX-style regular-expression matching with PostgreSQL's core operators (`~ ~* !~ !~*`)
and a small function set. **The flavor deliberately does NOT track PostgreSQL** (CLAUDE.md §1):
we own a clean, **RE2-able** subset chosen so the engine is **linear-time and immune to
catastrophic backtracking** — the SQL-standard `SIMILAR TO` is excluded, and so are
backreferences, lookaround, and named groups (the features that force backtracking). This is the
*overriding reason* for the divergence: **determinism (§8/§10) + untrusted-query safety (§13)**.

The engine is **hand-written in each core** (Rust/Go/TS) like the parser and executor — never
delegated to a language's native regex library. Native libraries would break the project's
core invariant three ways: different flavors (leftmost-first vs leftmost-longest), different
Unicode versions, and no meterable internal step count; JS's `RegExp` is moreover a backtracker
(ReDoS). See CLAUDE.md §2/§8/§14.

This document is the **cross-core compile-determinism contract**: it fixes the surface grammar,
the *exact* AST→bytecode lowering, and the Pike-VM execution model so that all three cores
compile a pattern to a byte-identical program and accrue byte-identical cost. It is as binding as
the key-encoding byte vectors ([encoding.md](encoding.md)); the fixtures in
[`spec/regex/`](../regex/) (`program_vectors.toml`, `match_vectors.toml`) pin it.

---

## 1. Surface — operators and semantics

Four binary operators, `text × text → boolean`, at the **comparison level** (precedence 35,
beside `LIKE`/`ILIKE` — [grammar.md](grammar.md) §22b):

| operator | meaning |
|---|---|
| `s ~ p`   | TRUE iff pattern `p` matches somewhere in subject `s` (case-sensitive) |
| `s ~* p`  | as `~`, case-insensitive |
| `s !~ p`  | `NOT (s ~ p)` |
| `s !~* p` | `NOT (s ~* p)` |

- **Match-anywhere (unanchored).** Like PG and unlike `LIKE`, `~` matches if the pattern matches
  any *substring* of the subject — `'abc' ~ 'b'` is TRUE. Anchor with `^`/`$` for whole-string.
- **NULL propagates.** A NULL operand yields NULL *before* the matcher runs (`null = "propagates"`,
  the LIKE rule) — never the pattern error.
- **Type.** `text × text`; a non-text operand is **42804** (`compare.toml` lists only text×text).
- **Invalid pattern → `2201B`** (`invalid_regular_expression`), raised when the pattern is
  compiled (§7). A *valid but too-large* compiled program is **`54001`** (`statement_too_complex`,
  §6) — the same structural-complexity gate as deep expression nesting.
- **Negation** (`!~`, `!~*`) is carried on the AST node (a `negated` flag), not a separate
  operator — the `NOT LIKE` precedent. There is no `NOT ~` keyword spelling (PG has none either).
- **Case-insensitive `~*`** simple-lowercases (1:1) *both* operands before compiling/matching —
  exactly the `ILIKE` mechanism ([collation.md](collation.md) §16): ASCII baseline when no Unicode
  property bundle is loaded, full simple Unicode mappings when one is. So `'FOO' ~* 'f.o'` is TRUE.
- **Match unit = Unicode code point** (not byte, not UTF-16 unit) — a §8 determinism surface, the
  `LIKE` precedent. Cores iterate Rust `chars()`, Go `[]rune`, TS `Array.from`.

---

## 2. Pattern grammar (the RE2 subset)

A pattern is a sequence of branches separated by `|` (alternation). Operator precedence, loosest
to tightest: **alternation** `|` → **concatenation** → **quantifier** (`* + ? {…}`) → **atom**.

**Atoms:**

| syntax | meaning |
|---|---|
| literal char | matches that one code point |
| `.` | any code point **except** newline `\n` (U+000A) — single-line; no `s`/dotall flag |
| `[...]` | character class (below) |
| `[^...]` | negated character class |
| `\d \w \s` | ASCII digit `[0-9]` / word `[0-9A-Za-z_]` / whitespace `[\t\n\v\f\r ]` |
| `\D \W \S` | their negations |
| `\n \t \r \f \v` | the control characters |
| `\<metachar>` | a literal metacharacter — `\. \* \+ \? \( \) \[ \] \{ \} \| \^ \$ \\` |
| `(...)` | capturing group (group index = its `(` order, 1-based) |
| `(?:...)` | non-capturing group |
| `^` `$` | zero-width anchors: start / end of the whole subject (no multiline) |
| `\A` `\z` | zero-width string-boundary anchors: start / **absolute** end of the whole subject — identical to `^`/`$` while there is no multiline mode, spelled separately so they stay string-anchored once one lands. `\z` is jed's RE2/PCRE spelling (PostgreSQL uses `\Z` for it) |

**Quantifiers** apply to the immediately preceding atom; each has a **greedy** and a **lazy**
(`?`-suffixed) form:

| greedy | lazy | meaning |
|---|---|---|
| `*` | `*?` | zero or more |
| `+` | `+?` | one or more |
| `?` | `??` | zero or one |
| `{n}` | `{n}?` | exactly `n` |
| `{n,}` | `{n,}?` | at least `n` |
| `{n,m}` | `{n,m}?` | between `n` and `m` (n ≤ m) |

**Character class `[...]`** contents: literal chars, ranges `a-z` (by code point), the predefined
escapes `\d \w \s \D \W \S`, and the control/`\<metachar>` escapes. `^` negates **only** as the
first char. `]` ends the class unless escaped `\]`. `-` is a literal at the first/last position or
escaped `\-`, else a range. Ranges are normalized to **sorted, merged, non-overlapping** code-point
intervals + a `negated` flag (the canonical form the fixtures pin).

**Deliberately excluded** (each `2201B` if used as the corresponding metasyntax, or a literal where
PCRE would also read it literally): backreferences `\1`, lookaround `(?=)`/`(?!)`/`(?<=)`/`(?<!)`,
named groups `(?<name>)`, inline flag groups `(?i)`, the `s`/`m`/`x` flags, `\b` word boundary,
POSIX `[[:alpha:]]` classes, `\xHH`/`\x{…}`/`\uHHHH` numeric escapes, Unicode property escapes
`\p{…}`, and **`\Z`** (the PCRE end-anchor that also matches *before a trailing newline*). `\Z` is
excluded on purpose: its **only** behavior distinct from `\z` is that trailing-newline leniency,
which jed does nowhere — `$` itself is strict end-of-subject — so admitting `\Z` would either
duplicate `\z` under a false-friend name or import a newline special-case jed rejects. (PostgreSQL
accepts `\Z`, as end-of-string only, so `s ~ 'p\Z'` is an oracle divergence recorded in the ledger.)
These are §10 follow-ons or permanently out (backtracking-forcing).

**Lenient `{`** (the PCRE rule, for ergonomics + determinism): a `{` begins a quantifier **only**
when it matches `{\d+}`, `{\d+,}`, or `{\d+,\d+}`; otherwise it is a **literal `{`**. So `'a{b}'`
matches the literal text `a{b}`. A well-formed interval with `m < n` is **`2201B`**.

**Error cases (all `2201B`):** a quantifier with no preceding atom (`*ab`, `a**`), an unbalanced
`(`/`[`, a trailing `\`, an unknown alphabetic escape (`\q`), a `{n,m}` with `m < n`.

---

## 3. Compilation — pattern → bytecode program (the exact lowering)

Compilation parses the pattern to a regex AST, then **emits a flat instruction array** in the
order specified here. All three cores MUST emit the identical sequence (pinned by
`program_vectors.toml`). Instruction operands that are jump targets are absolute instruction
indices.

### 3.1 Instruction set

| opcode | operands | execution |
|---|---|---|
| `Char` | `c` (code point) | if `sp < len` and `input[sp] == c`: thread → `pc+1` at `sp+1`; else die |
| `Any`  | — | if `sp < len` and `input[sp] != '\n'`: thread → `pc+1` at `sp+1`; else die |
| `Class`| `k` (class-table index) | if `sp < len` and `class[k]` admits `input[sp]`: thread → `pc+1` at `sp+1`; else die |
| `Split`| `x, y` | fork: add thread at `x` (**higher priority**), then at `y` |
| `Jmp`  | `x` | thread → `x` |
| `Save` | `n` (slot) | set capture slot `n` = `sp`; thread → `pc+1` |
| `AssertStart` | — | if `sp == 0`: thread → `pc+1`; else die |
| `AssertEnd`   | — | if `sp == len`: thread → `pc+1`; else die |
| `Match`| — | accept; record capture slots |

`Char`/`Any`/`Class` are **consuming** (advance `sp`); the rest are zero-width (epsilon). A class
table is an array of `{negated, ranges:[(lo,hi)…]}` referenced by index; identical classes are
**not** deduplicated (a fresh index per class occurrence — keeps emission a pure function of
position; the fixtures pin the indices).

### 3.2 Program wrapper — unanchored match-anywhere + group 0

The whole program is, in order:

```
0: Split  3, 1        # implicit LAZY .*? prefix → match may start at any position, leftmost preferred
1: Any                #   consume one code point …
2: Jmp    0           #   … and retry the prefix
3: Save   0           # group 0 (whole match) opens
   <emit(root)>       # the pattern body (§3.3)
   Save   1           # group 0 closes
   Match
```

The prefix's `Split 3, 1` is **lazy** (the body arm `3` is higher priority than the consume arm
`1`), which makes the match **leftmost** — the engine prefers to begin matching as early as
possible. Group `k`'s capture occupies slots `2k` and `2k+1`; group 0 is the whole match. (For the
boolean operators the captures are ignored — the operator is TRUE iff a `Match` is reached — but
the program is built capture-capable from Slice 1 so Slice 2 reuses it unchanged.) A pattern that
begins with `^` keeps the prefix; `AssertStart` simply kills every non-zero start — correct, no
special case.

### 3.3 `emit(node)` — per-node emission order

Let `pc` be the next free index. Targets written `→Lk` are resolved by the order below.

- **Literal `c`:** `Char c`.
- **Any (`.`):** `Any`.
- **Class:** `Class k` (append the normalized class to the class table, `k` = its append index).
- **Concat `[a, b, …]`:** `emit(a)`, `emit(b)`, … in source order.
- **Alternation** — **right-associative binary fold**: `a|b|c` ≡ `a|(b|c)`. For `X|Y`:
  ```
      Split  →LX, →LY
  LX: emit(X)
      Jmp    →LEND
  LY: emit(Y)
  LEND:
  ```
- **Group, capturing (index `i`):** `Save 2i`, `emit(sub)`, `Save 2i+1`.
- **Group, non-capturing:** `emit(sub)`.
- **`^`:** `AssertStart`.   **`$`:** `AssertEnd`.
- **Star `sub*` (greedy):**
  ```
  L1: Split  →L2, →L3      # greedy: enter-body arm first
  L2: emit(sub)
      Jmp    →L1
  L3:
  ```
  **lazy `sub*?`:** swap the `Split` arms → `Split →L3, →L2`.
- **Plus `sub+` (greedy):**
  ```
  L1: emit(sub)
      Split  →L1, →L3       # greedy: loop arm first
  L3:
  ```
  **lazy `sub+?`:** `Split →L3, →L1`.
- **Quest `sub?` (greedy):**
  ```
      Split  →L1, →L2       # greedy: take-it arm first
  L1: emit(sub)
  L2:
  ```
  **lazy `sub??`:** `Split →L2, →L1`.
- **Bounded `sub{n,m}` — UNROLL** (so `regex_compile` cost = instruction count and the program-size
  cap bounds it):
  - `{n}` → `emit(sub)` **n** times.
  - `{n,}` → `emit(sub)` **n** times, then `Star(greedy, sub)` (§Star) — i.e. n mandatory copies
    then a star of one more copy. (`{0,}` = `Star`.)
  - `{n,m}` (n ≤ m) → `emit(sub)` **n** times, then `Quest(greedy, sub)` **(m − n)** times. (`{0,0}`
    emits nothing.) The lazy form uses the lazy `Star`/`Quest` arm order for the optional tail.
  - **Size pre-check:** before unrolling, if `n·|sub|` (or `(m)·|sub|`) would push the program past
    `MAX_REGEX_PROGRAM`, raise **`54001`** without allocating — the cap is enforced *projectively*,
    not by overrunning memory (§6).

### 3.4 Determinism notes (the contract)

- Emission is a **pure function of the AST**, and parsing is a pure function of the pattern text,
  so `pattern → program` is total and identical across cores.
- The `Split` arm **order** is the whole of greedy-vs-lazy and leftmost-first — getting it wrong
  flips semantics *and* the `regex_step` count, so it is pinned per node above and in the fixtures.
- Class normalization (sort by `lo`, merge touching/overlapping ranges, then apply `negated` at
  match time — never by complementing the range list) is fixed so the class tables match byte-for-byte.

---

## 4. Execution — the Pike VM (leftmost-first, capture-tracking)

The VM simulates all NFA threads in lockstep over the input, one code point at a time. It is
**O(program × input)** — linear in input, with no backtracking — and tracks capture slots.

State: two thread lists `clist` (current position) and `nlist` (next), each a **priority-ordered**
list of threads `{pc, saves[]}` with a **per-list `visited` set of pcs** (the dedup that bounds work
to ≤ |program| threads per position). A `matched: saves | null` holds the best match so far.

```
matched := null
add_thread(clist, {pc:0, saves: [⊥;2·(ngroups+1)]}, sp:0)
for sp := 0 .. len (inclusive):
    nlist := empty
    i := 0
    while i < clist.len:
        t := clist[i]                           # NO charge here — the consume loop is unmetered
        switch prog[t.pc]:                       # (the state was already charged when add_thread explored it)
            Char c:  if sp < len and input[sp] == c:        add_thread(nlist, {t.pc+1, t.saves}, sp+1)
            Any:     if sp < len and input[sp] != '\n':     add_thread(nlist, {t.pc+1, t.saves}, sp+1)
            Class k: if sp < len and class_admits(k, input[sp]): add_thread(nlist, {t.pc+1, t.saves}, sp+1)
            Match:   matched := t.saves
                     break                       # CUT lower-priority threads in clist (leftmost-first)
        i := i + 1
    clist := nlist
    guard()                                      # §6 ceiling check, once per input position
    if clist.len == 0: break
return matched                                   # boolean operators: TRUE iff matched != null
```

`add_thread(list, t, sp)` performs the **epsilon-closure** and appends consuming/Match threads,
deduping by pc. **`regex_step` is charged here — exactly once per instruction visited (after the
dedup check)** — so the unit counts the distinct `(instruction, input-position)` states the VM
explores (`≤ |program| × (|input|+1)`), and the consume loop above never re-charges a state:

```
add_thread(list, t, sp):
    if t.pc ∈ list.visited: return               # dedup — first (highest-priority) wins, no charge
    list.visited.add(t.pc)
    charge regex_step                            # ONE step per instruction visited (the only charge site)
    switch prog[t.pc]:
        Jmp x:        add_thread(list, {x, t.saves}, sp)
        Split x, y:   add_thread(list, {x, t.saves}, sp); add_thread(list, {y, t.saves}, sp)   # x first (priority)
        Save n:       s2 := t.saves.clone(); s2[n] := sp; add_thread(list, {t.pc+1, s2}, sp)
        AssertStart:  if sp == 0:   add_thread(list, {t.pc+1, t.saves}, sp)
        AssertEnd:    if sp == len: add_thread(list, {t.pc+1, t.saves}, sp)
        _:            list.append(t)             # Char / Any / Class / Match — parked for the consume loop
```

A real implementation makes `add_thread` **iterative** (an explicit work stack, pushing the `y`
arm of a `Split` before `x` so `x` is processed first = higher priority) so a long Jmp/Split chain
in a large program cannot overflow the native stack — the same native-stack discipline as the
parser's depth limit.

**Leftmost-first acceptance.** Threads are processed in priority order. When a thread reaches
`Match`, it records `matched` and the inner loop **breaks**, discarding the lower-priority threads
*at the current position* — they cannot yield a more-preferred match. Higher-priority threads
already advanced into `nlist` keep running; if one reaches `Match` at a later position it
**overwrites** `matched` (it is more preferred). When all threads die, the last recorded `matched`
is the answer, with its capture slots. This is exactly Perl/PCRE leftmost-first greedy semantics
(Russ Cox, *Regular Expression Matching: the Virtual Machine Approach*).

**Capture slots** are code-point offsets into the (possibly case-folded, §1) subject; `⊥` (unset)
prints as NULL in `regexp_match` (Slice 2). Slot copying on `Save` is what the priority discipline
costs; for the boolean operators it is dead weight but kept for engine uniformity.

---

## 5. Constant-pattern precompilation

The common case `col ~ 'literal'` must compile **once**, not per row. At **resolve** time, if the
pattern operand is a constant text literal, the pattern is compiled and the program stored on the
resolved node (`RExpr::Regex.program = Some(prog)`); a non-constant pattern stores `None` and
compiles per row at eval. (Precedent: jed already mutates a resolved node at plan time —
`fold_uncorrelated_in_rexpr`.)

**Charging compile cost deterministically** (the cross-core contract):

- **Constant (precompiled):** charge `regex_compile × |program|` **once per statement execution**,
  on the node's **first evaluation** (tracked by a one-shot flag on the resolved node), then
  `guard()`. Never per row. A node short-circuited away and never evaluated charges no compile (no
  work, no cost). Every core compiles the identical program (the fixtures guarantee it), so the
  count is identical.
- **Non-constant:** charge `regex_compile × |program|` **each time** the pattern is compiled (each
  row whose pattern value is evaluated) — genuinely repeated work, correctly metered.

For `~*` the pattern is `fold_lower_simple`-folded *before* compilation, so the precompiled program
is that of the folded pattern, and each subject is folded before matching (the ILIKE precedent).

---

## 6. Resource bounds (untrusted-query safety, §13)

Two independent gates, mirroring the §13 two-gate model (a structural cap *and* the cost ceiling):

1. **Linear time by construction.** The Pike VM is O(program × input) with no backtracking, so the
   classic ReDoS patterns (`(a+)+$`, `(a|a)*b`) run in linear time rather than exploding. This holds
   *independent of* the cost meter — it is why the engine is an NFA simulation, not a backtracker.

2. **Deterministic cost meter** — two units in [`schedule.toml`](../cost/schedule.toml), accrued and
   guarded exactly like `decimal_work` (charge, then guard; [cost.md](cost.md) §3):
   - **`regex_compile`** — one unit per NFA instruction *emitted* while compiling a pattern. Bounds
     pattern-compile + `{n,m}` unroll work. Charged `|program|`× per compile (§5).
   - **`regex_step`** — one unit per Pike-VM thread-step (each instruction dispatched in the main
     loop or the epsilon-closure). Bounds the match work; total ≤ `|program| × (|input| + 1)`.
     Guarded once per input position, so a runaway match aborts **`54P01`** deterministically.

3. **`MAX_REGEX_PROGRAM`** — a fixed cross-core constant (**32768** instructions), checked
   *projectively* during emission (§3.3): a pattern whose compiled program would exceed it raises
   **`54001`** (`statement_too_complex`) at compile, before any large allocation. This guards the
   *unlimited* handle (`max_cost = 0`, the trusted path), where the cost ceiling cannot — exactly
   analogous to `MAX_EXPR_DEPTH` (parser nesting) and `MAX_COMPOSITE_DEPTH` (catalog depth), the
   other two structural-complexity triggers of `54001` (cost.md §7/§7b). The constant is pinned in
   `impl/go/spec_constants_test.go` and a `resource/regex_program_limit.test` corpus entry.

`2201B` is for a *malformed* pattern; `54001` for a *well-formed but too-large* one. Both surface at
compile (resolve for a constant pattern, eval for a per-row pattern).

---

## 7. Errors

| code | when |
|---|---|
| `2201B` `invalid_regular_expression` | malformed pattern: unbalanced `(`/`[`, trailing `\`, unknown alphabetic escape, quantifier with no atom, `{n,m}` with `m<n` (§2) |
| `54001` `statement_too_complex` | a well-formed pattern whose compiled program exceeds `MAX_REGEX_PROGRAM` (§6) |
| `54P01` `cost_limit_exceeded` | accrued `regex_compile`/`regex_step` (plus the statement's other cost) reaches the handle's `max_cost` (§6) |
| `42804` | a non-text operand to `~ ~* !~ !~*` |

`2201B` borrows PostgreSQL's SQLSTATE for the same condition (the LIKE/`22025` precedent of reusing
PG codes for the analogous condition). Its message is `invalid regular expression: {detail}` with a
short, **deterministic** detail (e.g. `"unbalanced parenthesis"`, `"quantifier operand missing"`) —
the message is part of the cross-core contract, so the detail strings are enumerated in this doc and
the corpus, not free-form.

**Detail strings (canonical):** `"unbalanced parenthesis"`, `"unbalanced bracket expression"`,
`"quantifier operand missing"`, `"trailing backslash"`, `"invalid escape \X"` (with the offending
char), `"invalid repetition count"` (`{n,m}`, `m<n` or non-numeric).

---

## 8. Slice 2 — `regexp_replace` / `regexp_match`

Two scalar functions over the same engine (the boolean operators ignore captures; these consume
them):

- **`regexp_replace(source, pattern, replacement [, flags])` → text.** Replace the **first** match
  (or **all** with the `g` flag) of `pattern` in `source` by `replacement`. The replacement is a
  *template*: `\1`…`\9` splice in capture group 1…9, `\&` the whole match, `\\` a literal backslash.
  (These are template back-references *into the replacement string* — NOT regex backreferences, so
  they do not compromise the RE2 linearity.) No match → `source` unchanged. A NULL arg → NULL.
- **`regexp_match(source, pattern [, flags])` → text[].** The capture array of the **first** match:
  element `i` is group `i`'s captured substring (group 0 omitted when there is ≥1 group, matching
  PG: the array is groups 1..n; a pattern with no group returns a 1-element array of the whole
  match). An unset (`⊥`) group element is **NULL**. **No match → NULL** (the whole result, not an
  empty array).

`flags` is a short text: `'i'` (case-insensitive, = the `~*` fold), `'g'` (global, `regexp_replace`
only). Unknown flag → `2201B`. These are jed's first **text-returning** and **text[]-returning**
functions; they resolve through a dedicated `RExpr::RegexFunc` node carrying the result in the
surrounding `ResolvedType` (the array/range-function precedent), not the scalar-`result` `ScalarFunc`
path. Cost: `operator_eval` (the call node) + `regex_compile` (once, constant pattern) + `regex_step`
(the match), plus, for a global replace, the match is re-run from the end of each match (each run
metered).

---

## 8b. Slice 3 — `regexp_count` / `regexp_instr` / `regexp_substr` / `regexp_like`

PostgreSQL's four **Oracle-compatibility** scalar functions (PG 15+), over the same Pike VM and the
same `RExpr::RegexFunc` node. Each is STRICT (a NULL argument → NULL). Unlike Slice 2 these take
**integer** positional arguments alongside the text ones; the BASIC subset **agrees with PostgreSQL**
(it is the RE2 subset that overlaps PG), so the corpus rows oracle-check.

The shared kernel is **non-overlapping match iteration** from a code-point start position — the same
advance rule the global `regexp_replace` already uses (after a match `[s,e)`, continue at `e`; an
**empty** match `s==e` advances at `e+1` so a nullable pattern can't loop). `regexp_count` runs it to
exhaustion; `regexp_substr`/`regexp_instr` stop at the **N-th** match.

- **`regexp_like(string, pattern [, flags])` → boolean.** TRUE iff `pattern` matches somewhere in
  `string` — exactly `string ~ pattern` (or `~*` with `'i'`). The simplest of the four; reuses the
  boolean `is_match` directly (no start parameter).
- **`regexp_count(string, pattern [, start [, flags]])` → integer.** The number of non-overlapping
  matches at or after the 1-based `start` (default 1). An empty-capable pattern matches the empty
  string at each gap (`regexp_count('abc', 'x*')` = 4: before `a`, `b`, `c`, and at end). `start`
  past the end → 0. **Note** the 3-argument overload's 3rd argument is `start` (an integer), **not**
  `flags`; `flags` appears only in the 4-argument form (the PG overload set).
- **`regexp_substr(string, pattern [, start [, N [, flags [, subexpr]]]])` → text.** The substring
  matched by the **N-th** match (1-based `N`, default 1) at or after `start`. `subexpr` (default 0)
  selects a parenthesized capture group within that match — 0 = the whole match, *k* = group *k*.
  No N-th match, an out-of-range `subexpr` (> the pattern's group count), or a group that did not
  participate → **NULL**.
- **`regexp_instr(string, pattern [, start [, N [, endoption [, flags [, subexpr]]]]])` → integer.**
  The 1-based position of the **N-th** match's `subexpr` span. `endoption` (default 0) picks **0** =
  the position of the first character of the span, **1** = the position *following* the last
  character. No N-th match, out-of-range `subexpr`, or a non-participating group → **0**.

**Argument validation** (before the pattern compiles, matching PG's order — a bad `start` beats a bad
pattern): `start` < 1, `N` < 1, `subexpr` < 0, and `endoption` ∉ {0, 1} each raise **`22023`**
(`invalid_parameter_value`) with the offending parameter named (`invalid value for parameter
"start": 0`). *Caveat:* a **constant** pattern is precompiled at resolve (§5), so a malformed
constant pattern surfaces its `2201B` before any per-row argument check — a deliberate, narrow
ordering divergence from PG for the (pattern-invalid **and** argument-invalid) case only; every
single-error case agrees.

**Flags.** Only `'i'` (case-insensitive) is meaningful here; `'g'` is `regexp_replace`-only, so any
flag other than `'i'` — including `'g'` — is `2201B` (jed's RE2 flavor owns a smaller flag set than
PG, regex.md §1; PG accepts `c/g/m/n/p/q/s/t/w/x` and rejects `'g'` on these with a different message
— a documented flavor divergence). The numeric arguments are **strictly typed** integers: a non-integer
in a numeric slot is `42883` (no matching overload), jed's strict-typing stance rather than PG's
implicit text→int cast. Cost is unchanged from Slice 2: `operator_eval` + `regex_compile` (once for a
constant pattern) + `regex_step` per match step explored across the iteration.

---

## 9. Cross-core fixtures

Two TOML fixture families in [`spec/regex/`](../regex/) (the `spec/encoding/` precedent), the
binding determinism contract:

- **`program_vectors.toml`** — `(pattern, flags) → (instruction listing, instruction count)`. Each
  core compiles the pattern and asserts the emitted program and `|program|` (= `regex_compile` cost)
  match the listing exactly. This is THE compile-determinism check.
- **`match_vectors.toml`** — `(pattern, flags, input) → (matched?, capture spans, regex_step cost)`.
  Each core runs the VM and asserts identical result, capture offsets (code-point), and step count.

Per CLAUDE.md §10 these are verified by **per-core unit tests** (cross-core compile/cost identity is
structurally outside the SQL corpus's reach); the SQL-observable behavior lives in the conformance
corpus (§11).

---

## 10. Deferred / excluded

**Deferred follow-ons** (each a later slice; `0A000` if attempted before then):
- Set-returning `regexp_matches` (setof text[]), `regexp_split_to_table` (setof text) — feasible on
  the existing SRF machinery, just out of this scope.
- `regexp_split_to_array` (PG's other Oracle-compat addition). The scalar Oracle-compat functions
  `regexp_count` / `regexp_instr` / `regexp_substr` / `regexp_like` **landed in Slice 3** (§8b).
- `\b` word boundary, POSIX `[[:class:]]`, numeric escapes `\xHH`/`\x{…}`, Unicode-property classes
  `\p{…}` (ties into the collation Unicode-property work), the `s`/`m`/`x` flags and inline `(?i)`.
- A lazy-DFA fast path (perf only — same semantics); regex-driven index acceleration (anchored
  `~ '^literal'` → a PK/index bound, the point-lookup precedent).

**Permanently excluded** (would force backtracking, defeating the linear-time guarantee):
backreferences `\1`, lookaround `(?=)`/`(?!)`/`(?<=)`/`(?<!)`, named groups. And the SQL-standard
`SIMILAR TO` (a separate, deliberately-omitted surface).

---

## 11. Conformance

- Capabilities ([manifest.toml](../conformance/manifest.toml)): `expr.regex_match` (`~`/`!~`),
  `expr.regex_imatch` (`~*`/`!~*`, builds on `expr.regex_match`), `resource.regex_program_limit`;
  Slice 2 adds `func.regexp_replace`, `func.regexp_match`; Slice 3 adds `func.regexp_count`,
  `func.regexp_instr`, `func.regexp_substr`, `func.regexp_like`.
- Corpus: `suites/expr/regex.test` (operators, NULL/42804/2201B, code-point/astral, `# cost:` pins),
  `suites/resource/regex_program_limit.test` (`54001` boundary, jed-specific — not oracle-checked),
  `suites/expr/regexp_functions.test` (Slice 2), `suites/expr/regexp_oracle_functions.test` (Slice 3).
- **Oracle divergence** (§7 conformance): jed's flavor is the RE2 subset, so only the **PG-agreeing
  subset** is oracle-checkable (`rake corpus:check`); flavor-divergent cases (a pattern PG accepts
  via a backref, greedy-edge differences) get **hand-authored expected output + an
  `oracle_overrides.toml` entry**. Cost is never oracle-checked (jed-specific).
- A NoREC metamorphic relation (`col ~ p` ≡ `NOT (col !~ p)`) and a `bench/` regex workload, per the
  §10 growth obligations.
