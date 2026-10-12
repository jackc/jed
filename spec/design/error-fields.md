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
> table stamp, with per-core `error_fields` tests; additive, no format bump). The fifth field,
> **`Detail`** (the offending values — `Key (id)=(1) already exists.`), is specified in §9.
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
| `Position` | `P` | **defer** (§7) | cursor offset into the query; real value for 42601, but needs the parser to thread byte positions |
| `Hint` | `H` | **defer** (§7) | jed errors are terse by design; no hint corpus |
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

**Absence representation is idiomatic per core** — Rust `None`, Go `""`, TS `undefined` — matching
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
- **`Position`.** A 1-based cursor offset into the query for syntax/name errors (42601/42703). High
  value for tooling, but needs the hand-written parsers to thread byte positions through — a real
  plumbing cost, separable from this note.
- **`Hint`.** Deferred; jed's errors are intentionally terse and there is no hint corpus.
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
| 23503 | parent side: DELETE/UPDATE stranding a child | `Key ({refcols})=({vals}) is still referenced from table "{child}".` |
| 23P01 | INSERT / UPDATE | `Key ({cols})=({vals}) conflicts with existing key ({cols})=({other}).` |
| 23P01 | `ALTER TABLE … ADD EXCLUDE` over existing rows | `Key ({cols})=({vals}) conflicts with key ({cols})=({other}).` |
| 23514 | INSERT / UPDATE | `Failing row contains ({row}).` |
| 23502 | INSERT / UPDATE | `Failing row contains ({row}).` |

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
  `Key is not present in table "{parent}".` / `Key is still referenced from table "{child}".`. Every
  other form is **absent**.
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
cross-core byte-identical, with two input-determined exceptions:

- a `timestamptz` renders in the session zone (deterministic given the session);
- a float value's *layout* follows the existing `float-render-layout` determinism exception, whose
  surface now names `Detail`.

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
