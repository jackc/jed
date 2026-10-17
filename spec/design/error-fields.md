# Structured error fields — design

> **The problem this fixes:** a jed `EngineError` carries only a SQLSTATE and a free-text
> `message`. A constraint's name is spelled *into that message* ("duplicate key value violates
> unique constraint: users_pkey") but is not exposed as a **structured field**, so a host that
> wants to know *which* constraint fired must regex a string the spec explicitly declares
> informational and non-contractual ([../errors/registry.toml](../errors/registry.toml) header;
> [api.md](api.md) §7). PostgreSQL avoids exactly this by shipping the constraint name (and
> table/column/type) as separate diagnostic fields. This doc decides which of those fields jed
> adopts, modeled on the **pgx `pgconn.PgError`** struct, and how they populate without diverging
> from PostgreSQL past what jed's design already requires. **Status: landed** (all three cores —
> Rust `Option<String>` fields, Go `string`, TS optional — via typed constructors + a DML-boundary
> table stamp, with per-core `error_fields` tests; additive, no format bump; `Detail`, `Position`,
> and `Hint` likewise landed in all three cores, pinned by shared fixtures). The fifth field,
> **`Detail`** (the offending values — `Key (id)=(1) already exists.`), is specified in §9; the sixth
> and seventh, **`Position`** (a 1-based character offset into the query) and **`Hint`**
> (PostgreSQL's suggestion — "Perhaps you meant …", "You might need to add explicit type casts.",
> the LATERAL hints), in §10 and §11.
> [api.md](api.md) §7 owns the host-facing error surface; [../errors/registry.toml](../errors/registry.toml)
> owns the code/template registry.

## 1. The gap

All three cores define `EngineError` identically: a `SqlState` plus a `message`
([impl/rust/src/error.rs](../../impl/rust/src/error.rs), [impl/go/errors.go](../../impl/go/errors.go),
[impl/ts/src/errors.ts](../../impl/ts/src/errors.ts)). The message *does* contain the constraint
name for the whole integrity-violation class (23502/23505/23514/23503/23P01) — every raise site
already fills the registry template's `{name}`/`{table}` placeholders. The name is therefore
**computed and discarded into prose**.

Two facts make that prose unusable for programmatic handling:

1. **The message is contractually non-matchable.** "Message TEXT is informational — matching is
   on `code`/`name`, never on prose" ([../errors/registry.toml](../errors/registry.toml) header);
   [api.md](api.md) §7 calls it "an informational (never-matched) message." A host that regexes it
   is building on a surface the project reserves the right to reword.
2. **`code` alone under-identifies.** `23505` says *a* unique constraint was violated; it cannot
   say whether it was `users_email_key` or `users_username_key` — the routine question a host asks
   to map the failure back to a user-facing field. PostgreSQL answers it with a structured field;
   jed currently cannot.

This is a divergence from PostgreSQL with no overriding reason behind it (§1 of the brief) — the
data is already in hand at every raise site. The fix is to stop throwing it away.

## 2. Reference model: pgx `pgconn.PgError`

pgx (the maintainer's PostgreSQL driver) surfaces the PostgreSQL protocol error fields as a flat
struct. It is the right model because it *is* the PostgreSQL error surface, decoded. Its 18 fields,
each mapped to a jed disposition:

| pgx field | PG proto | jed disposition | why |
|---|---|---|---|
| `Code` | `C` | **have** (`state`/`.code()`) | the SQLSTATE; already the matched identity |
| `Message` | `M` | **have** (`message`) | the informational line; already present |
| `ConstraintName` | `n` | **added** | the headline; already computed at every 23xxx raise site |
| `TableName` | `t` | **added** | the relation the write targeted; known at the raise site |
| `ColumnName` | `c` | **added** | column-specific errors (23502 not-null) know it |
| `DataTypeName` | `d` | **added** | 22003/22001 already template `{type}` |
| `Detail` | `D` | **added** (§9) | the offending *values*, PostgreSQL's sentences; a structured field only — the rendered message stays one line |
| `Position` | `P` | **added** (§10) | 1-based character offset of the offending token; set for parse errors and name-resolution errors |
| `Hint` | `H` | **added** (§11) | PostgreSQL's misspelled-column and misused-alias suggestions; a structured field only |
| `SchemaName` | `s` | **omit** (map later) | jed has **no schemas** — the qualifier is a *database* ([attached-databases.md](attached-databases.md), [introspection.md](introspection.md) §3). A `DatabaseName` analog is the natural mapping if/when attachment-qualified errors need it. |
| `Severity` | `S` | **exclude** | jed has one severity (error); no notice/warning channel |
| `SeverityUnlocalized` | `V` | **exclude** | no localization (single language, determinism) |
| `InternalPosition` | `p` | **exclude** | no internally-generated queries |
| `InternalQuery` | `q` | **exclude** | same |
| `Where` | `W` | **exclude** | no PL/procedures → no context traceback |
| `File` | `F` | **exclude (hard)** | see §6 — would break cross-core byte-identity |
| `Line` | `L` | **exclude (hard)** | see §6 |
| `Routine` | `R` | **exclude (hard)** | see §6 |

## 3. The v1 field set

`EngineError` gains four optional string fields, named to mirror pgx/PostgreSQL exactly, each in
the core's idiom:

| concept | Rust | Go | TS |
|---|---|---|---|
| existing | `state: SqlState` | `State SqlState` | `state: SqlState` |
| existing | `message: String` | `Message string` | `message` (via `Error`) |
| constraint | `constraint_name: Option<String>` | `ConstraintName string` | `constraintName?: string` |
| table | `table_name: Option<String>` | `TableName string` | `tableName?: string` |
| column | `column_name: Option<String>` | `ColumnName string` | `columnName?: string` |
| data type | `data_type_name: Option<String>` | `DataTypeName string` | `dataTypeName?: string` |
| detail (§9) | `detail: Option<String>` | `Detail string` | `detail?: string` |
| position (§10) | `position: Option<u32>` | `Position int32` | `position?: number` |
| hint (§11) | `hint: Option<String>` | `Hint string` | `hint?: string` |

**Absence representation is idiomatic per core** — Rust `None`, Go `""` (and `0` for `Position`, as
in pgx), TS `undefined` — matching
pgx's own choice (`string` zero-value `""`) for Go. This is not a cross-core divergence in any
observable sense: the conformance corpus matches on `code` only (§8), so the fields are never
cross-checked byte-for-byte the way values on disk are; they are a host-API ergonomic, checked by
per-core unit tests. The Go core's field names land **identical to pgx's**, which is a small nicety
for the driver's author.

**Rendering is unchanged.** `Display`/`Error()` keeps jed's current `"{code}: {message}"` form
([error.rs](../../impl/rust/src/error.rs) `fmt`). We deliberately do **not** adopt pgx's
`"{severity}: {message} (SQLSTATE {code})"` — jed has no severity, and the corpus's
`statement error <regex>` and the stock-runner code-embedding both depend on the current format.
The new fields are additive metadata, not a message change.

## 4. Population map

Which errors set which fields (matching how PostgreSQL populates the same protocol fields):

| SQLSTATE | name | `ConstraintName` | `TableName` | `ColumnName` | `DataTypeName` |
|---|---|---|---|---|---|
| 23505 | unique_violation | ✓ index name / derived `<table>_pkey` | ✓ | | |
| 23514 | check_violation | ✓ | ✓ | | |
| 23503 | foreign_key_violation | ✓ | ✓ (the written table) | | |
| 23P01 | exclusion_violation | ✓ backing GiST index name | ✓ | | |
| 23502 | not_null_violation | — (jed's NOT NULL is unnamed, as in PG) | ✓ | ✓ | |
| 22003 | numeric_value_out_of_range | | | | ✓ |
| 22001 | string_data_right_truncation | | | ✓ | ✓ (`varchar(n)`) |

Each of these class-23 rows also sets `Detail` (§9), subject to the §9.2 exceptions and the §9.4
privilege gate.

The `<table>_pkey` value for a primary-key `23505` stays the derived name jed already prints
([constraints.md](constraints.md) §5.4) — jed persists no such relation, but the *reported*
constraint name matches PostgreSQL's auto-name, so `ConstraintName` and the message agree.

## 5. Construction — one source, no drift

The failure mode to avoid: the message says `users_pkey` while `ConstraintName` says something
else, because two code paths formatted the name independently. **The template and the field must
come from one call.**

The implementation is a small family of **typed constructor helpers**, one per integrity error,
that own both the registry message *and* the field population (Rust `EngineError::…`, Go `new…`,
TS `…Violation`):

```
// Rust (the Go/TS mirrors take the same shape)
EngineError::unique_violation(table, constraint)          // msg + ConstraintName + TableName
EngineError::check_violation(table, constraint)
EngineError::fk_violation_insert(table, constraint)
EngineError::fk_violation_delete(parent, constraint, child)
EngineError::exclusion_violation(table, constraint)
EngineError::not_null_violation(column)                   // msg + ColumnName (table stamped later)
```

with a generic builder (`EngineError::new(state, msg).with_constraint(..).with_table(..)`) as the
escape hatch. `new` leaves all four fields empty, so the several-hundred existing `new(state, msg)`
call sites compile and behave unchanged; only the integrity raise sites (in `executor/dml.rs`,
`executor/ddl.rs`, `executor/store_encode.rs` and their Go/TS mirrors) switched to the helpers.

**23502's table comes from the boundary, not the raise site.** `not_null_violation` (and the
22003/22001 coercion errors) are raised inside the value-coercion path (`store_value` /
`coerce_for_store`), which knows the *column* but not the *relation*. The relation is stamped where
the coercion is driven — the per-row DML boundary — via `.map_err(|e| e.with_table(relation))` in
Rust, `stampTable(err, table)` in Go, and a `try/catch` + `stampTable` in TS. Every error that path
produces is about storing into that relation, so the stamp is always correct.

**`DataTypeName` routes through the existing `overflow(ty)` helper** (plus the decimal- and
range-element overflow sites and the `varchar(n)` truncation), so every "value out of range for type
X" carries the type without touching each arithmetic site individually.

## 6. Determinism & cross-core identity — why `File`/`Line`/`Routine` are excluded

pgx's `File`/`Line`/`Routine` carry the PostgreSQL **C source** location that raised the error.
The jed analog would be the *core's* source location — and Rust, Go, and TS raise the same logical
error from different files at different lines. Exposing them would make the error surface **disagree
across cores**, which is precisely the byte-identity/determinism guarantee the whole project is
built to protect (CLAUDE.md §8, §10). They are a hard exclusion, not a "no analog" omission.

The v1 fields are safe on this axis because every value is a **catalog identifier** (constraint,
table, column, type name) that is already cross-core-identical by construction. No new determinism
ledger entry is needed, and there is **no on-disk format change** — this is purely the in-memory
error surface.

## 7. Deferred and open

- **`Detail`** — no longer deferred; specified in §9.
- **`Position`** — no longer deferred; specified in §10.
- **`Hint`** — no longer deferred; specified in §11: the two suggestion families, the LATERAL hints
  (§11.8), and PostgreSQL's fixed sentences for the conditions jed shares (§11.4–§11.7). PostgreSQL's
  remaining hints on surfaces jed implements remain open: the collation hints (`42P21` / `42P22`
  "…applying the COLLATE clause…"), and the DETAIL sentences that accompany the §11.8 hints ("There is
  a column named …, but it cannot be referenced from this part of the query.").
- **`DatabaseName` (the `SchemaName` analog).** jed qualifies by database, not schema. If
  attachment-qualified errors ever need to name the database, this is the additive field to add;
  not motivated yet.

Each deferred field is purely additive to the struct, so shipping v1 forecloses none of them.

## 8. Testing & docs

- **Per-core unit tests, not the corpus.** The sqllogictest corpus matches `statement error` on the
  rendered code/prose only, so it structurally cannot assert a structured field (CLAUDE.md §10 —
  "host-API surface" / "catalog introspection" are exactly the sanctioned unit-test categories). The
  per-core `error_fields` tests ([impl/rust/tests/error_fields.rs](../../impl/rust/tests/error_fields.rs),
  [impl/go/error_fields_test.go](../../impl/go/error_fields_test.go),
  [impl/ts/tests/error_fields.test.ts](../../impl/ts/tests/error_fields.test.ts)) assert
  `ConstraintName`/`TableName`/`ColumnName`/`DataTypeName` on the caught error, one case per code
  (modeled on the `foreign_key` tests).
- **/web.** [api.md](api.md) §7 and the `web/src/routes/docs/api/` error-surface docs document the
  four fields (the same-change rule, CLAUDE.md §10).
- **No format bump, no cost change, no determinism-ledger entry** (§6).

## 9. `Detail` — the offending values

PostgreSQL's `DETAIL: Key (id)=(1) already exists.` names the *values* that broke the constraint. It
is the most useful field after the constraint name: a host can tell the user which email is taken
without re-querying. jed adds it as a fifth structured field. All three cores implement this section,
and the shared fixture [../errors/detail_cases.txt](../errors/detail_cases.txt) pins the text.

### 9.1 Decision: a structured field, not a second message line

jed's house style is a **single-line message** with no `DETAIL:` line ([constraints.md](constraints.md)
§6.7, [upsert.md](upsert.md) §9). That style is **kept**. `Detail` is structured metadata, exactly like
`ConstraintName`. `Display`/`Error()`/`toString()` stays `"{code}: {message}"`, so the corpus's
`statement error` regexes, the stock-runner code embedding, and every host's logged error string are
unchanged. The house-style rule now reads: *the rendered message carries no DETAIL; the values are in
the `Detail` field*.

The CLI is a presentation layer. It prints a psql-style second line, `DETAIL:  …`, after the error line
when `Detail` is set ([cli.md](cli.md) §5).

### 9.2 Text forms — PostgreSQL's sentences

`Detail` uses PostgreSQL 18's exact sentences, byte-for-byte except for the value and label rendering in
§9.3. `{cols}` is the key's comma-and-space-separated label list, `{vals}` the matching value list, and
`{t}` a relation name in double quotes, verbatim (PostgreSQL's `"%s"`, not `quote_ident`).

| SQLSTATE | raised by | `Detail` |
|---|---|---|
| 23505 | INSERT / UPDATE / upsert (a stored or in-statement duplicate) | `Key ({cols})=({vals}) already exists.` |
| 23505 | `CREATE UNIQUE INDEX`, `ALTER TABLE … ADD UNIQUE / PRIMARY KEY` over existing rows | `Key ({cols})=({vals}) is duplicated.` |
| 23503 | child side: INSERT/UPDATE (and `ADD FOREIGN KEY` validation) | `Key ({cols})=({vals}) is not present in table "{parent}".` |
| 23503 | child side, `MATCH FULL`: a partly-NULL referencing tuple ([constraints.md](constraints.md) §6.3) | `MATCH FULL does not allow mixing of null and nonnull key values.` |
| 23503 | parent side: DELETE/UPDATE stranding a child | `Key ({refcols})=({vals}) is still referenced from table "{child}".` |
| 23P01 | INSERT / UPDATE | `Key ({cols})=({vals}) conflicts with existing key ({cols})=({other}).` |
| 23P01 | `ALTER TABLE … ADD EXCLUDE` over existing rows | `Key ({cols})=({vals}) conflicts with key ({cols})=({other}).` |
| 23514 | INSERT / UPDATE | `Failing row contains ({row}).` |
| 23502 | INSERT / UPDATE | `Failing row contains ({row}).` |
| 22004 | `jsonb_set_lax` with a SQL NULL value and `null_value_treatment` `'raise_exception'` ([json-sql-functions.md](json-sql-functions.md) §2) | `Exception was raised because null_value_treatment is "raise_exception".` |
| 22P05 | a decoded `\u0000` in JSON input, a de-escaping `json` function, or a jsonpath string ([json.md](json.md) §4.2, [jsonpath.md](jsonpath.md) §2.1) | `\u0000 cannot be converted to text.` |
| 22P02 | an unpaired surrogate `\u` escape, decoded (same sites) | `Unicode low surrogate must follow a high surrogate.` / `Unicode high surrogate must not follow a high surrogate.` |
| 22P02 | a JSON `\u` escape without four hex digits ([json.md](json.md) §4.2) | `"\u" must be followed by four hexadecimal digits.` |
| 42P10 | a reference from the right side of a `RIGHT` / `FULL` join to that join's left side ([grammar.md](grammar.md) §44) | `The combining JOIN type must be INNER or LEFT for a LATERAL reference.` |

`{row}` is every column of the target table in ordinal order, from the candidate (new) row.

**Absent.** `Detail` is absent (`None`/`""`/`undefined`) for every other error, including these
class-23 raises, which PostgreSQL also gives no DETAIL:

- the `ALTER TABLE` validations `ADD CHECK` (23514) and `SET NOT NULL` / `ADD PRIMARY KEY` over a
  NULL (23502);
- a NULL in a NOT NULL **composite field** (23502 from inside a composite value). PostgreSQL has no
  such constraint, so there is no sentence to match;
- a 23505 found only when phase 2 writes, under a writable-CTE read pin (an earlier sub-statement of
  the same `WITH` staged the key — constraints.md §7 step 9, writable-cte.md §7). The error stands; the
  Detail is not built on that write path.

**Which values.** The values are those of the violation jed already reports, in the existing
deterministic visitation order (constraints.md §7; planner.md for UPDATE/DELETE targets). `Detail`
never changes *which* error is raised. For 23505 the values are the *new* row's key. For 23P01
`{vals}` is the new row (or, in DDL validation, the row being checked) and `{other}` is the stored or
earlier-in-statement row it conflicts with. For 23503 parent side, `{vals}` is the referenced parent
key being removed or changed.

### 9.3 Rendering — the deterministic text path

- **Values** render through the **result-cell renderer**, the text a `SELECT` of that value shows, in
  the session's time zone (`Value::render_in`, the host `render`; [timezones.md](timezones.md) §9.5).
  This is the deterministic text-output path the corpus already cross-checks. It covers every keyable
  and storable type, including composite, array, and range values, which `CAST(… AS text)` does not
  ([types.md](types.md) §5).
- **NULL** renders as `null` (PostgreSQL's DETAIL spelling, not the corpus's `NULL`). This applies to
  a top-level value only. A NULL *inside* an array or composite keeps that container's own text
  (`{1,NULL,3}`), as in PostgreSQL.
- **Booleans** render `true`/`false`, jed's result-cell form (types.md §10). PostgreSQL's DETAIL uses
  `boolout`'s `t`/`f`. jed keeps one value renderer rather than adding a DETAIL-only spelling.
- **Truncation.** Each rendered value is cut to at most **64 bytes**, at the last UTF-8 character
  boundary that fits, followed by `...` when anything was cut. That is PostgreSQL's
  `ExecBuildSlotValueDescription` limit (`maxfieldlen = 64`, `pg_mbcliplen`). jed applies it to
  *every* form, including keys. PostgreSQL renders index keys untruncated; jed's cap is a deliberate,
  ledgered divergence (§9.7): it bounds the error's size whatever the value (CLAUDE.md §13). Truncation
  is applied to the rendered text, so a composite or array is cut as a whole, not per element.
- **Column labels** use jed's identifier quoting rule (`quote_identifier`, grammar.md §3): bare when
  `[a-z_][a-z0-9_]*`, otherwise double-quoted with `"` doubled. This is PostgreSQL's `quote_ident` except
  that jed has no reserved keywords, so `Key (select)=(1)` stays bare.
- **Expression-key labels** (an expression index, indexes.md §10) use the index's persisted canonical
  token text, the form `jed_constraints.expression` already shows (constraints.md §4.5): `Key (lower ( e
  ))=(x)`. PostgreSQL's `pg_get_indexdef` prints `lower(e)`. jed reuses its one cross-core-identical
  expression text rather than adding a second renderer.

### 9.4 Privilege gate — no values the session cannot read

A detail can expose data the writing session is not allowed to read. A 23P01 shows *another* row's
key, and a "Failing row" shows columns a view may hide. PostgreSQL withholds the values unless the
user can SELECT them (`BuildIndexValueDescription`, `ri_ReportViolation`). jed follows it, using the
session envelope (session.md §5.3):

- Values appear only when the session holds **`SELECT` on the relation whose values are shown**. For
  23505, 23P01, 23514, and 23502 that is the written table. For 23503 child side it is the child (the
  written table), and for parent side it is the parent (the written table).
- Without that privilege, the 23503 forms degrade to PostgreSQL's value-free sentences
  `Key is not present in table "{parent}".` / `Key is still referenced from table "{child}".`. The
  `MATCH FULL` sentence names no values and is kept unchanged, as PostgreSQL does. Every other form
  is **absent**.
- The check is on the **base** relation. A session privileged only on an updatable view
  ([views.md](views.md) §15) that writes through it does not see values. That is the conservative
  reading; a host that wants values grants SELECT on the base table.
- jed has no per-column privileges (session.md §5.3), so the table-level check is the whole rule.

The default session is fully permissive, so `Detail` is present for an ordinary host.

### 9.5 NOT NULL is checked after the row is formed (PostgreSQL's order)

"Failing row contains" needs the whole candidate row. PostgreSQL forms the row first, coercing every
column and evaluating every default, then checks NOT NULL. So a later column's error wins over an
earlier column's NULL. Probed on PostgreSQL 18:

- `INSERT INTO z(a, b) VALUES (NULL, 100000)` with `a i32 NOT NULL, b i16` gives `22003`.
- A failing default (`DEFAULT 1/0`) on a later column gives `22012`.

jed used to check NOT NULL inline while it coerced each column, so it raised `23502` first. It now
matches PostgreSQL. Each column is coerced (defaults evaluated) in ordinal order. A NULL into a NOT
NULL column is noted, and the column holds NULL. Once the row is complete, the first noted column (in
ordinal order) raises `23502`, before CHECK (constraints.md §4.4). This changes which error a
statement with several faults reports, toward PostgreSQL. It does not change any single-fault
statement.

### 9.6 Determinism and cost

`Detail` is built **after** the error is decided, from values the statement already holds. It is
**unmetered** and cannot change the error code, the reported cost, or the abort point. Its text is
cross-core byte-identical. A `timestamptz` renders in the session zone (deterministic given the
session), and a float as its PostgreSQL text ([float.md](float.md) §9).

There is no format bump and no cost-schedule change.

### 9.7 Divergences from PostgreSQL (recorded)

- 64-byte truncation applies to key values too (§9.3), not only to the "Failing row" values.
- Values use jed's result-cell text, so a boolean is `true`/`false`, not PostgreSQL's `t`/`f` (§9.3).
- Labels follow jed's identifier quoting (no reserved keywords) and canonical token text for an
  expression key (§9.3).
- The privilege gate checks base-table `SELECT` only. There are no column privileges, and a view-only
  writer sees no values (§9.4).
- `Detail` is a structured field. It is never appended to the rendered message (§9.1).

### 9.8 Testing

The corpus cannot assert a structured field (§8). A shared, language-neutral fixture
[../errors/detail_cases.txt](../errors/detail_cases.txt) lists cases: setup SQL, an optional privilege
envelope, the failing statement, the expected code, and the expected `Detail` (or its absence). Each
core's `error_fields` test runs every case. The fixture's expected texts were captured from
PostgreSQL 18 wherever a PostgreSQL equivalent exists. The cases that differ by design (truncated keys,
privilege-gated forms, expression labels) are marked in the fixture.

## 10. `Position` — where in the query

PostgreSQL's `P` field is a cursor into the statement: `SELECT nope FROM t` fails with `42703` at
position 8. A query tool uses it to underline the offending text, which matters most for ad-hoc
query surfaces (CLAUDE.md §13). jed adds it as a sixth structured field.

### 10.1 Unit and origin

- **1-based, in characters.** Position `1` is the first character of the statement text. A character
  is a Unicode code point, so `é` counts once whatever its UTF-8 or UTF-16 width. This is
  PostgreSQL's unit (the protocol's "positions are measured in characters not bytes"). Each core
  converts its native index (Rust/Go bytes, TS UTF-16 code units) when it records a token.
- **End of input is length + 1.** An error detected at end of input points one past the last
  character, as PostgreSQL's `at end of input` does (`SELECT 1 +` → `11`).
- **Relative to the text the host passed.** For `execute`, `query`, and `prepare` that is the one
  statement. For `execute_script` it is the whole script: the statement's position is shifted by the
  number of characters before the statement's span, as PostgreSQL does for a multi-statement simple
  query. A host that runs `split_statements` itself gets positions relative to each span's text.
- **Absent** (`None`/`0`/`undefined`) when the error has no source location. In particular, text jed
  stored itself never yields a position: a view body, a CHECK, DEFAULT, or index expression, or any
  other catalog text re-parsed from storage, is parsed without positions. A position into text the
  host never wrote would be misleading. The one re-parse that keeps positions is `CREATE VIEW`'s
  validation of the body it is creating: the canonical body text re-lexes to the statement's own
  body tokens one for one, so each token keeps its position in the statement. The body jed then
  stores is parsed again without positions.

### 10.2 Which errors carry it

| raised by | SQLSTATEs | position of |
|---|---|---|
| the lexer | every code except class 54 | the first character of the lexeme being scanned (the opening `'` of an unterminated string, the `/*` of an unterminated comment, the bad character); a Unicode-escape decoding or `UESCAPE` error points where PostgreSQL's does ([grammar.md](grammar.md) §3) |
| the parser | every code except class 54 | the token at which the parse stopped (§10.3) |
| a column reference in an expression (select list, WHERE, ON, GROUP BY, HAVING, ORDER BY, RETURNING, SET values, …) | `42703`, `42702`, `42P01`, `42P09`, `42P10` | the reference's first character — the qualifier of `t.c` |
| an `INSERT` column-list name or an `UPDATE … SET` / `ON CONFLICT DO UPDATE SET` target | `42703` | the column name |
| a FROM-clause table or view name | `42P01` | the name — the database qualifier of `db.t` |
| an `INSERT` / `UPDATE` / `DELETE` target, also in a writable CTE or under `EXPLAIN` | `42P01` | the name — the database qualifier of `db.t` |
| a function call, aggregate, window function, or FROM-clause table function with no implementation for its arguments, or with several and none best | `42883`, `42725` | the function name; for a special form the keyword PostgreSQL rewrites from (`SUBSTRING`, `POSITION`, `OVERLAY`, the `AT` of `AT TIME ZONE`, the `LIKE` of `LIKE … ESCAPE`) |
| an operator with no implementation for its operand types | `42883`, `42804` | the operator token; the `NOT` of `NOT IN` / `NOT BETWEEN` / `NOT LIKE`, else the `IN` / `BETWEEN` / `LIKE` / `ILIKE` keyword; the `IS` of `IS [NOT] DISTINCT FROM`; the comparison of `op ANY` / `op ALL` |
| a non-boolean argument of `WHERE`, `JOIN … ON`, `HAVING`, `AND`, `OR`, `NOT`, a searched `CASE WHEN`, `FILTER`, or `CHECK` | `42804` | the argument's location (below) |
| a set of expressions with no common type: `CASE` results, `COALESCE`, `GREATEST` / `LEAST`, `ARRAY[…]` elements, a `VALUES` column, a set-operation column | `42804` | the location of the first expression that does not unify with those before it, in PostgreSQL's order: a `CASE`'s `ELSE` result first, then its `THEN` results; a set operation's right operand's column |
| a value of the wrong type for its column: an `INSERT … VALUES` expression, an `INSERT … SELECT` column, an `UPDATE … SET` / `DO UPDATE SET` value | `42804` | the value's location |
| a string literal converted at resolve time: cast (`'abc'::int`, `CAST('abc' AS int)`), typed (`int 'abc'`), or adapting to its context (`uuid_col = 'x'`, `UPDATE … SET range_col = '[1,'`) | any input error (`22P02`, `22003`, `22007`, …) | the literal |

An expression's **location** is PostgreSQL's `exprLocation`: the leftmost character the expression was
written at, grouping parentheses excluded — a function call's name, an operator expression's left
operand's location, a prefix operator, a `CASE` keyword, a literal (a folded negative number's minus
sign), a `::` cast's operand. jed records it only where it is exact; for an expression whose leftmost
token it does not record (a bind parameter, a scalar subquery, `EXISTS`, `ROW(…)`, `ARRAY[…]`,
`COALESCE`, `GREATEST`, `NULLIF`, `EXTRACT`, the SQL/JSON constructors, `CAST(x AS t)` of a non-literal whose
PostgreSQL location depends on whether the cast is a no-op) the location is unknown and the error
carries no position, rather than a wrong one.

The class-54 program limits (`54000` statement too long, `54001` nesting too deep) are resource
limits, not mistakes at a place, and carry no position (PostgreSQL's stack-depth error has none).
An error raised while the statement runs carries none either, as in PostgreSQL: a cast of a value at
run time (`'abc'::text::int`, `text_col::int`), a store-time conversion of an `INSERT … VALUES` literal
slot (jed converts a bare literal slot when it stores the row, where PostgreSQL coerces it at parse
time — the slot's input error is therefore unpositioned). Every other error leaves the field absent.
PostgreSQL positions still more resolver errors (`42704` unknown type, `42846` cannot cast, a simple
`CASE`'s `WHEN` comparison); each is purely additive here, needing only the node's recorded position.

jed reports most operator type mismatches as `42804` (its strict type check) where PostgreSQL's
operator lookup reports `42883` (`1 + 'a'::text`); the position is the same operator token. The
fixture marks those cases `# jed:`.

A position is stamped by the **innermost** site that knows one. A column-reference error raised
inside a subquery keeps the subquery's reference's position; an outer wrapper never overwrites it.
An expression node decides the position of the errors its own resolution raises — a function call
its `42883`, an operator its `42883` / `42804`, a string literal its input error — and marks every
error leaving it as decided, so an error an operand raised is never claimed by the enclosing node
even when the operand recorded no position. A raise site that names a specific operand (`AND`'s
non-boolean side, the first mismatching `CASE` result) stamps that operand's location itself.

### 10.3 The parser's error token

PostgreSQL's grammar reports a syntax error "at or near" the lookahead token: the first token that
cannot continue a valid statement prefix. jed's recursive-descent parsers identify the same token as
the **furthest token the parser examined** before failing, counting the current token (`peek`) and
each consumed token (`advance`) but not lookahead probes (`peek_at(n)` at any offset, including 0)
or the precomputed parenthesized-query table. "Furthest" rather than "last" keeps backtracking honest: a
speculative parse that rewinds (an INSERT value slot re-parsed as an expression) cannot pull the
position backward.

For an error the parser raises after reading a construct (a duplicate option, an out-of-range integer
literal, a deferred `0A000` form), the same rule applies: the position is the furthest token read.
That is usually the token just past the construct, where PostgreSQL would name the construct
itself; the code and message are unaffected.

The three parsers are hand-mirrored, so they examine tokens in the same order and agree on the
position by construction. The shared fixture (§10.5) pins it.

### 10.4 Representation in the AST

Each core records, per token, its 1-based character position, beside the token stream. The parser
copies a position into the AST nodes that name things: a column reference (bare and qualified), an
`ORDER BY` column key, an `INSERT` column-list name, an `UPDATE` / `DO UPDATE` `SET` target, a FROM
table reference, an `INSERT` / `UPDATE` / `DELETE` target, a literal, a typed literal (its string),
a function call (its name, or a special form's keyword), an operator (its token, §10.2), a `CASE`,
and a `CAST(… AS …)` (its keyword; a `::` cast records none). The `IN` / `BETWEEN` desugaring gives
its comparisons the construct's operator position. These positions are **invisible to AST equality**, like the existing view-rewrite
token positions (`TableRefPos`, `StarPos`, views.md §5): `a + b` written twice still matches as a GROUP
BY expression. A parse of stored text records no positions (§10.1).

A column-reference error is stamped where the resolver resolves that node: the error's position is
set from the node when the error is `42703`, `42702`, `42P01`, or `42P09` and has no position yet.
The other expression errors are stamped as §10.2 describes; the "decided" mark is internal
bookkeeping on the error, never a field.

### 10.5 Testing

The corpus cannot assert a structured field (§8). A shared fixture,
[../errors/position_cases.txt](../errors/position_cases.txt), lists cases in the `detail_cases.txt`
format with `position:` and `hint:` directives. Each core's `error_fields` test runs every case. The
expected positions are PostgreSQL 18's for the same statement wherever jed rejects the statement at
the same token; a case where jed's token surface differs is marked `# jed:`.

## 11. `Hint` — "Perhaps you meant …"

PostgreSQL attaches a HINT to an unknown column when a column of a similar name is in scope:
`SELECT nme FROM t` → `Perhaps you meant to reference the column "t.name".`. It is the most useful hint
for an ad-hoc query tool, and it is deterministic (an edit distance over the catalog names in scope).
jed adds `Hint` as a seventh structured field and populates it for two suggestion families (§11.1,
§11.2), the LATERAL hints (§11.8), and PostgreSQL's fixed sentences for the conditions jed shares
(§11.4–§11.7). Like `Detail`, it is never appended to the rendered message (§9.1); the CLI prints it
as psql's `HINT:  …` line.

### 11.1 Misspelled column (`42703` on a column reference)

The hint uses PostgreSQL's `errorMissingColumn` search (`parse_relation.c`) exactly:

1. **Candidates.** Walk the scope chain from the reference's own query level outward. At each level
   visit the FROM relations in FROM order, then the inner relations of an unaliased parenthesized
   join; the walk also visits the relations the reference cannot see, in §11.8's order. Skip a
   relation with no label. Within a relation visit its columns in ordinal order (a write through a
   view: the view's columns). Each candidate is `(label, column)`.
2. **Relation penalty.** For a qualified reference `q.c`, a relation's penalty is the Levenshtein
   distance (insert, delete, substitute all cost 1, over characters) from `q` to its label. For a bare
   reference the penalty is 0.
3. **State.** Start with `distance = 4` (PostgreSQL's `MAX_FUZZY_DISTANCE + 1`) and no first or second
   match. For each candidate, in order:
   - skip it when `penalty > distance`;
   - let `d` be the Levenshtein distance from the candidate column to `c`; skip it when
     `d > len(c) / 2` (integer division, `len` in **bytes**, as PostgreSQL's `strlen`);
   - let `d' = d + penalty`;
   - if `d' < distance`: `distance = d'`, first = this candidate, second = none;
   - else if `d' == distance`: if there is a second match, clear both (three equal candidates are too
     many; `distance` stays); else if there is a first match, second = this candidate.
4. **An exact match.** A candidate with `penalty == 0` and `d == 0` exists only when the column is in
   the walk but unreachable from here: a relation the reference cannot see (§11.8), or a
   qualifier-only relation. §11.8 decides the hint then; the fuzzy suggestion is not used.
5. **Text.** One match: `Perhaps you meant to reference the column "{label}.{column}".`. Two:
   `Perhaps you meant to reference the column "{l1}.{c1}" or the column "{l2}.{c2}".`. None: no hint.
   Names are inserted verbatim inside the double quotes (PostgreSQL's `"%s.%s"`, not `quote_ident`).

A computed distance above the bound is never used, so an implementation may compute the full
distance instead of PostgreSQL's bounded `varstr_levenshtein_less_equal`; the outcome is identical.

Only a column-reference `42703` (the §10.2 expression row) gets this hint. A misspelled `INSERT`
column-list name or `SET` target does not, matching PostgreSQL.

### 11.2 Table name used instead of its alias (`42P01` on a qualifier)

`SELECT t.a FROM t f` names the table, not its alias `f`. When a qualifier matches no relation label
in the scope chain, the hint comes from the first relation in the §11.1 walk (§11.8 relations
included) that is labeled with the qualifier or is a base table or view reference whose **catalog
name** is the qualifier (PostgreSQL's `searchRangeTableForRel`). A visible relation found by its
catalog name under a different label gives
`Perhaps you meant to reference the table alias "{label}".`; a relation the reference cannot see gives
§11.8's hint or none. PostgreSQL also rewords this message to "invalid reference to FROM-clause entry
for table"; jed keeps its message (informational, and stable for the corpus).

### 11.3 Determinism and divergences

The hint is built after the error is decided, from names already in scope. It is unmetered and never
changes the error code, cost, or abort point. Its text is cross-core identical: it depends only on
catalog names and the fixed walk order.

Recorded divergences from PostgreSQL:

- PostgreSQL visits an unaliased join's inner relations at their place in the range table; jed visits
  a level's visible ones after its FROM relations. This can change which of two equally close
  candidates is named first.
- An aliased parenthesized join hides its inner relations, and jed does not record them among the
  relations a reference cannot see (§11.8). PostgreSQL still offers them as misspelled-column
  candidates; jed does not.
- Every other PostgreSQL hint is absent (§7), except those of §11.4–§11.8.

### 11.4 `jsonb_set_lax`'s `raise_exception` (`22004`)

`jsonb_set_lax` with a SQL NULL value and `null_value_treatment => 'raise_exception'`
([json-sql-functions.md](json-sql-functions.md) §2) raises `22004` `JSON value must not be null` with
PostgreSQL's fixed Detail (§9.2) and Hint, byte-for-byte:
`To avoid, either change the null_value_treatment argument or ensure that an SQL NULL is not passed.`
Both are constant text, so they are deterministic and cross-core identical; the error carries no
Position (it is raised at evaluation, as in PostgreSQL). The shared fixtures pin both
([../errors/detail_cases.txt](../errors/detail_cases.txt),
[../errors/position_cases.txt](../errors/position_cases.txt)).

### 11.5 Invalid Unicode escape (`42601`)

A malformed escape in a `U&'…'` string or `U&"…"` identifier ([grammar.md](grammar.md) §3) raises
`42601` `invalid Unicode escape` with PostgreSQL's fixed Hint, byte-for-byte:
`Unicode escapes must be \XXXX or \+XXXXXX.` — naming the backslash even under a `UESCAPE`
character, as PostgreSQL does. The other Unicode-escape errors (escape value, surrogate pair,
`UESCAPE`) carry no hint. Constant text, so cross-core identical; the shared position fixture pins it
([../errors/position_cases.txt](../errors/position_cases.txt)).

### 11.6 `jsonb_insert` on an existing key (`22023`)

`jsonb_insert` whose path ends at an existing object key ([json-sql-functions.md](json-sql-functions.md)
§2.1) raises `22023` `cannot replace existing key` with PostgreSQL's fixed Hint, byte-for-byte:
`Try using the function jsonb_set to replace key value.` Constant text, so cross-core identical; the
error carries no Position (raised at evaluation), and the shared position fixture pins it
([../errors/position_cases.txt](../errors/position_cases.txt)).

### 11.7 Function, operator, and assignment hints

PostgreSQL attaches a fixed sentence to each of these conditions. jed attaches the same sentence,
byte-for-byte, wherever it raises the same condition, including where jed's SQLSTATE differs from
PostgreSQL's for it (the operator mismatches jed reports as `42804`, §10.2). The sentence is constant
text, so it is cross-core identical, unmetered, and changes no code, cost, or abort point.

| condition | SQLSTATE | `Hint` (PostgreSQL source) |
|---|---|---|
| a function call, aggregate, window function, or FROM-clause table function with no implementation for its name and arguments (unknown name, wrong argument count or types, an unknown named parameter, a special form's rewritten function) | `42883`; a window function's argument-type check: `42804` | `No function matches the given name and argument types. You might need to add explicit type casts.` (`parse_func.c`) |
| several functions match an unknown-typed argument and none is best | `42725` | `Could not choose a best candidate function. You might need to add explicit type casts.` |
| an infix operator with no implementation for its operand types (`+`, `=`, `\|\|`, `->`, `@>`, `LIKE`, `IN`, `BETWEEN`, `IS DISTINCT FROM`, `op ANY` / `ALL`, …) | `42883`, `42804` | `No operator matches the given name and argument types. You might need to add explicit type casts.` (`parse_oper.c`) |
| prefix `-` with no implementation for its operand type | `42804` | `No operator matches the given name and argument type. You might need to add an explicit type cast.` |
| an operator whose every operand is an untyped string literal or `NULL` (`'1' + '2'`) | `42804` | `Could not choose a best candidate operator. You might need to add explicit type casts.` (PostgreSQL reports these as `42725`: several operators match `unknown` operands) |
| a sort key whose type has no ordering operator (`ORDER BY` a `json` column) | `42883` | `Use an explicit ordering operator or modify the query.` (`get_sort_group_operators`) |
| a value of the wrong type for its column: an `INSERT … VALUES` expression or literal slot, an `INSERT … SELECT` column, an `UPDATE … SET` / `DO UPDATE SET` value, a column `DEFAULT` (`CREATE TABLE`, `ADD COLUMN`, `SET DEFAULT`) | `42804` | `You will need to rewrite or cast the expression.` (`transformAssignedExpr`, `cookDefault`) |
| a recursive CTE whose recursive term changes a column's type or typmod | `42804` | `Cast the output of the non-recursive term to the correct type.` (`parse_cte.c`) |

**Which errors.** The hint belongs to the condition, not to the code: the raise site that detects the
condition sets it. The function sentence is set where function lookup fails. The operator sentences
are set by the operator node whose own resolution fails (the node that also positions the error,
§10.2), not by an error an operand raised. The assignment sentence is set where the value is checked
against its column, at resolve time or, for a literal slot or a constant default, when it is stored.
A raise site can also decide that its error carries no hint, and an enclosing context never adds one
then. These conditions carry **no** hint, as in PostgreSQL:

- a known function used as a window function (`abs(1) OVER ()`; PostgreSQL's `42809`, jed's `42883`);
  an unknown name used that way is a lookup failure and carries the function sentence;
- a grouping or `DISTINCT` key, or a set operation's column, whose type has no equality operator
  (`could not identify an equality operator for type json`);
- a row comparison of unequal sizes (PostgreSQL's `42601`, jed's `42804`);
- a composite value with the wrong number of fields for its column (PostgreSQL's `42846` cast error).

**Recorded divergences.**

- jed applies the operator and assignment sentences to every operator and assignment mismatch it
  reports, including those PostgreSQL accepts through an implicit cast that jed's strict type system
  omits (`1::float8 = 1::int`, `INSERT INTO t (int_col) VALUES (1.5)`). The sentence's advice holds in
  jed.
- jed has no `NULLIF`, so `NULLIF(1, 'a'::text)` is jed's unknown-function error with the function
  sentence; PostgreSQL rewrites it to `=` and gives the operator sentence.
- PostgreSQL's `ALTER TABLE … ALTER COLUMN … TYPE` hints (`You might need to specify "USING x::t".`,
  `You might need to add an explicit cast.`) are not attached: jed converts a column with an explicit
  cast ([alter.md](alter.md) §3.3), so the condition PostgreSQL hints about — a type with no
  assignment cast — is not the one jed reports, and suggesting the cast jed already applied would
  mislead.

### 11.8 LATERAL hints — relations a reference cannot see

PostgreSQL's range table at each query level holds every FROM relation processed so far, including
ones a reference there cannot see. Its hint search (`errorMissingColumn`, `errorMissingRTE`) walks
them too. jed reproduces that walk for the two places it hides relations at a level:

- **A non-LATERAL derived table, `VALUES` list, or parenthesized join at FROM position `i > 0`.** While
  its body is planned, the relations to its left are recorded: base tables, views, CTE references,
  table functions, derived tables, and the inner relations of an unaliased parenthesized join (an
  aliased one hides its inner relations and is itself a join, which PostgreSQL's search skips). The
  walk visits them right after the body's outermost level, innermost body first.
- **A JOIN's `ON` clause**, which cannot see the comma items to the left of its join segment. The
  walk visits them before the clause's own relations.

Each such relation is marked **LATERAL-reachable** when marking the subquery `LATERAL` would make it
visible: a comma item to the left of the item's join segment, or an item of the segment when the join
to the item is not `RIGHT` or `FULL`. It is marked **qualifier-only** when it is an inner relation of
a completed join (an item of the segment past the segment's first join, or an inner relation of an
unaliased parenthesized join) that LATERAL cannot reach. Neither mark applies from an `ON` clause:
the clause's hidden comma items, and a parenthesized join's left relations seen from its own `ON`
clause, are in the walk but not reachable. A table function's arguments are resolved with
PostgreSQL's `p_lateral_active` set, so a reference there never gets the LATERAL hint.

Then, for a `42703` (`errorMissingColumn`): let the **exact matches** be the walked relations with
penalty 0 (§11.1 step 2) that have the column.

| exact matches | `Hint` |
|---|---|
| none | the §11.1 suggestion over every walked relation |
| one, LATERAL-reachable | `To reference that column, you must mark this subquery with LATERAL.` |
| one, qualifier-only, bare reference | `To reference that column, you must use a table-qualified name.` |
| one, otherwise | none |
| two or more, bare reference | `Try using a table-qualified name.` |
| two or more, qualified reference | none |

For a `42P01` qualifier (`errorMissingRTE`, §11.2): the first walked relation labeled with the
qualifier or naming it gives, when hidden and LATERAL-reachable,
`To reference that table, you must mark this subquery with LATERAL.`; when hidden otherwise, no
hint.

PostgreSQL also gives these errors a DETAIL (`There is a column named "a" in table "t", but it cannot
be referenced from this part of the query.`) and rewords the `42P01` message; jed keeps its message
and adds no Detail yet (§7). PostgreSQL's other LATERAL condition, a reference from a `LATERAL` item
across a `RIGHT` or `FULL` join (`42P10`), carries a DETAIL and no hint, as in jed (§9.2), except for an
`UPDATE … FROM` / `DELETE … USING` target, which jed does not support.

The walk is built only on the error path from names already resolved, so it is unmetered and
deterministic. The shared fixture ([../errors/position_cases.txt](../errors/position_cases.txt))
pins every form against PostgreSQL 18.
