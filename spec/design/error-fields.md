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
> table stamp, with per-core `error_fields` tests; additive, no format bump).
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
| `Detail` | `D` | **defer** (§7) | the offending *values*; jed ships no DETAIL line today (house style, [constraints.md](constraints.md) §6). Leading phase-2 candidate. |
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

- **`Detail` (leading phase-2 candidate).** PostgreSQL's `DETAIL: Key (id)=(1) already exists.`
  names the offending *values* — the second-most-useful field after the constraint name. jed ships
  no DETAIL line today ([constraints.md](constraints.md) §6, a documented house-style divergence).
  Adding a *structured* `Detail` is orthogonal to the rendered single-line message, but it requires
  formatting values into the error via the value→text path; that is a larger surface (and revisits
  the house-style decision), so it is deferred, not rejected. If adopted it must render values
  through the existing deterministic text-output path so the field stays cross-core-identical.
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
