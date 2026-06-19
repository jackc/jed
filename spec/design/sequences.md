# Sequences — design

> `CREATE SEQUENCE name [options]` / `DROP SEQUENCE` and the value functions `nextval('s')`,
> `currval('s')` (later `setval`/`lastval`): named, persisted, monotonic **i64 generators** — the
> PostgreSQL sequence object. A sequence is a database-level catalog object (like a composite type),
> created and dropped at runtime, persisted in the catalog, and advanced by `nextval`. This doc is
> the contract all three cores implement in lockstep (CLAUDE.md §2); the grammar is in
> [../grammar/grammar.ebnf](../grammar/grammar.ebnf) + [grammar.md](grammar.md), the value functions
> in [../functions/catalog.toml](../functions/catalog.toml) + [functions.md](functions.md), the byte
> layout in [../fileformat/format.md](../fileformat/format.md) (`format_version` 12), the cost
> contract in [cost.md](cost.md), and the errors in [../errors/registry.toml](../errors/registry.toml).
> PostgreSQL semantics were pinned against the live `postgres:18` oracle (CLAUDE.md §1).

A sequence is the third kind of **database-level catalog object** (after tables and composite types)
and the first object whose entire reason for existing is to carry **mutable state** — a counter that
`nextval` advances. That state lives in the snapshot catalog, so a sequence's current value is part
of the committed database image and moves through the §3 staging-buffer / commit machinery exactly
like a table's rows. The one decision that defines the feature is what happens to that advance on
`ROLLBACK` — see §5.

## 1. Surface

```sql
CREATE SEQUENCE s
CREATE SEQUENCE [IF NOT EXISTS] s
    [INCREMENT [BY] n] [MINVALUE m | NO MINVALUE] [MAXVALUE x | NO MAXVALUE]
    [START [WITH] s0] [CACHE c] [[NO] CYCLE]
ALTER SEQUENCE [IF EXISTS] s RESTART [WITH n]   -- reset the counter (S2 — §4)
DROP SEQUENCE [IF EXISTS] s [, ...] [RESTRICT]

CREATE TABLE t (id serial, ...)                 -- the serial pseudo-types (S3 — §12): an
CREATE TABLE t (id bigserial, ...)              -- auto-created OWNED sequence + DEFAULT
CREATE TABLE t (id smallserial, ...)            -- nextval(...) + NOT NULL; DROP TABLE auto-drops it

SELECT nextval('s')            -- advance and return the new value (a WRITE statement — §4)
SELECT currval('s')            -- the last value nextval/setval produced IN THIS SESSION (§6)
SELECT setval('s', n)          -- set the counter; next nextval = n + INCREMENT (a WRITE — §4)
SELECT setval('s', n, false)   -- set the counter; next nextval = n (is_called = false)
SELECT lastval()               -- the value the most recent nextval returned this session (§6)
```

- **i64-valued.** A sequence generates `i64` (`bigint`) values, matching PostgreSQL's internal
  `int8` sequence representation. The `AS smallint | integer | bigint` typmod (PG 10+) is deferred
  `0A000` — every jed sequence is the `bigint` flavor this slice. `nextval`/`currval` return `i64`.
- **Options.** `INCREMENT BY` (non-zero step; `22023` for zero), `MINVALUE`/`MAXVALUE` (the inclusive
  bounds; `NO MINVALUE`/`NO MAXVALUE` select the type defaults), `START WITH` (the first value), and
  `[NO] CYCLE` (wrap vs. error at a bound). Defaults match PostgreSQL: a positive `INCREMENT`
  (default `1`) gives `MINVALUE 1`, `MAXVALUE 2^63-1`, `START` = `MINVALUE`; a negative `INCREMENT`
  gives `MINVALUE -(2^63-1)`, `MAXVALUE -1`, `START` = `MAXVALUE`. A `START`/`MINVALUE`/`MAXVALUE`
  combination that is inconsistent (`START < MINVALUE`, `START > MAXVALUE`, or `MINVALUE > MAXVALUE`)
  is `22023` at `CREATE`.
- **`CACHE`** is **parsed and stored but behaviorally `1`** — see §7. A `CACHE < 1` is `22023`.
- **`nextval('s')`/`currval('s')`/`setval('s', n[, is_called])`** take the sequence **name as a text
  argument** (the PG `nextval('s'::regclass)` form, with the `regclass` cast implicit). They are the
  first built-in functions to resolve a string argument to a **catalog object** and (`nextval`/
  `setval`) the first to **mutate** the database during expression evaluation. **`lastval()`** takes
  no argument (the first 0-arg sequence function). `setval`/`lastval` land in **S2** (§11) — their
  precise semantics are §4 (`setval`) and §6 (`lastval`).
- **`DROP SEQUENCE`** is `RESTRICT` by default and RESTRICT-only this slice (`CASCADE` is `0A000`).
  A missing sequence is `42P01` (sequences share the relation namespace — like `DROP TABLE`), unless
  `IF EXISTS`. **No dependency tracking this slice:** a plain `column DEFAULT nextval('s')` does *not*
  create a dependency in PostgreSQL either (only `serial`/`OWNED BY`/identity do, both deferred), so
  `DROP SEQUENCE` of a sequence named in some column default succeeds and a later `INSERT` raises
  `42P01` at evaluation — exactly PG.
- **`ALTER SEQUENCE [IF EXISTS] s RESTART [WITH n]`** lands in **S2** (§4) — the only `ALTER` action
  this slice. `ALTER SEQUENCE … SET INCREMENT|MINVALUE|… RENAME|OWNED BY|AS type` stay `0A000`.

## 2. Sequences as a catalog object — `Snapshot.sequences`

A sequence is resident in the snapshot catalog beside `tables` and `types`:

- A new `sequences` map on `Snapshot` (Rust `HashMap<String, SequenceDef>`, Go `map[string]*SequenceDef`,
  TS `Map<string, SequenceDef>`), keyed by **lowercased name** — the same keying tables/types use.
- `SequenceDef = { name, increment, min_value, max_value, start, cache, cycle, last_value, is_called }`
  — all `i64` except `name`, `cycle`, and `is_called`. `last_value` + `is_called` are the **mutable
  counter state** (PG's sequence-tuple fields); the rest are immutable definition (PG's `pg_sequence`
  fields). On `CREATE`, `last_value = start` and `is_called = false`.
- Because the whole `Snapshot` is the unit a commit publishes and a rollback discards (CLAUDE.md §3,
  [transactions.md §4.5](transactions.md)), the counter is **transactional by construction** — no
  separate store, no seam, no special casing (§5).

The name occupies the **relation namespace** it shares with tables and indexes: `CREATE SEQUENCE s`
when a table, index, or sequence `s` exists is `42P07 duplicate_table` (PG), and a `CREATE TABLE s`
after `CREATE SEQUENCE s` is likewise `42P07`. (This slice enforces the sequence↔sequence and
sequence↔table collisions; a sequence↔index collision rides the same name set.)

## 3. Catalog & on-disk format (`format_version` 12, owner added in `14`)

Sequences extend the v9 **kind-tagged** catalog (today `0` = table, `1` = composite-type) with a
third kind. The on-disk shape (full byte layout in [../fileformat/format.md](../fileformat/format.md)):

- **`entry_kind = 2`** is a sequence entry. Emission order across the catalog is **composite-type
  entries (kind 1) first, then sequence entries (kind 2), then table entries (kind 0)**, each group in
  ascending lowercased-name order; `item_count` per catalog page counts all entries, packed greedily
  exactly as before. Tables stay last because a column may reference a composite type by name at load;
  a sequence is referenced by **nothing** at load (a `DEFAULT nextval('s')` is stored expr-text,
  resolved at evaluation), so sequence entries need no two-pass and may sit anywhere ahead of the
  tables — they are grouped with the other non-table objects for a clean "schema objects, then tables"
  layout.
- **Sequence entry** (after the `entry_kind = 2` byte): `name_len u16` + name, then six fixed
  `i64` fields **big-endian two's-complement, no sign-flip** (a value-codec context, not a key —
  like the interval body) in this order — `increment`, `min_value`, `max_value`, `start`, `cache`,
  `last_value` — then `flags u8` (bit 0 = `cycle`, bit 1 = `is_called`, **bit 2 = `has_owner`** —
  new in v14; bits 3–7 reserved, written 0). The six i64 fields are fixed-width, no presence tags:
  every one is always present and non-NULL. **When `has_owner` is set** (an `owned` sequence — a
  `serial` column's sequence, §12), the flags byte is followed by the owner reference:
  `owner_table_len u16` + owner table name, then `owner_column u16` (the owning column's **0-based
  ordinal**). A non-owned sequence (a plain `CREATE SEQUENCE`) writes nothing after the flags byte —
  the v12 shape, unchanged.
- **`format_version` 12** was the original sequence bump; **`format_version` 14** adds the optional
  owner reference (the only on-disk change — bit 2 + the trailing owner bytes). Each is a clean break
  (the v14 reader rejects v13 and earlier, as every prior bump did). All existing `.jed` goldens
  regenerate at a bump (only the version byte changes for a file with no owned sequences; an owned
  sequence gains the trailing owner bytes). The reference encoder
  ([../fileformat/verify.rb](../fileformat/verify.rb)) carries the `sequences:` fixture key, the
  `sequence_table.jed` golden, and the v14 `serial_table.jed` golden, byte-identical
  `rust == go == ts == ruby`.

A sequence entry adds no value-codec, key-encoding, or B-tree change — a sequence owns no rows and no
tree, only its catalog tuple (like a `FOREIGN KEY`, which also owns no B-tree).

## 4. The value functions — name resolution, the write implication, read-only

`nextval`/`currval` are ordinary `[[operator]]` catalog rows (`kind = "function"`, `arg_families =
["text"]`, `result = "i64"`, `null = "propagates"`, `volatility = "volatile"`), resolved by the
normal overload path. Three things make them the first of their kind:

- **Name → catalog object at evaluation time.** The argument is a `text` value naming a sequence. The
  evaluator (which already holds the executor/snapshot — the same access `now()` uses for the clock
  seam) looks the name up in `Snapshot.sequences` (case-insensitively, the catalog keying). A missing
  sequence is `42P01 undefined_table` (PG renders "relation \"s\" does not exist"). A **NULL** argument
  propagates NULL (the `null = "propagates"` discipline) — matching PG, where `nextval(NULL)` is NULL.
- **`nextval`/`setval` mutate, so the statement is a WRITE.** A `SELECT nextval('s')` advances the
  sequence and therefore must run on the **write path**: take the single-writer gate, stage the
  advanced `SequenceDef` into the working snapshot, and commit it (autocommit) or carry it in the open
  transaction. The executor's write-detection (`stmt_is_write`) is extended: a statement is a write if
  its resolved expression tree contains a **sequence-mutating** function (`nextval`; `setval` in S2).
  `currval` is pure-read (session state only) and never forces the write path.
- **Read-only transactions.** `nextval`/`setval` inside a `READ ONLY` transaction (or a read handle)
  is `25006 read_only_sql_transaction` (PG: "cannot execute nextval() in a read-only transaction").
  `currval` is allowed in a read-only transaction.

**`nextval` semantics** (PG-exact, [pg `sequence.c`]): on a sequence with state `(last_value,
is_called)`:

- if `!is_called`: the result is `last_value` (the `START` on a fresh sequence); set `is_called =
  true`. The counter is *not* incremented — the first `nextval` returns `START`.
- else: compute `next = last_value + increment`. If `increment > 0` and `next > max_value`: if `cycle`,
  `next = min_value`, else `2200H sequence_generator_limit_exceeded` ("nextval: reached maximum value
  of sequence \"s\""). Symmetrically for `increment < 0` / `next < min_value`. The result is `next`;
  set `last_value = next`. The add is **overflow-safe** (a wrap past the i64 boundary is treated as
  crossing the bound, never a native overflow).

After a successful `nextval`, the value is recorded in the **session** state (§6) for `currval` and
`lastval`.

**`setval` semantics** (PG-exact, [pg `sequence.c` `do_setval`]): `setval('s', n)` and `setval('s',
n, is_called)` set the counter directly. The value `n` must lie in `[min_value, max_value]` or it is
`22003 numeric_value_out_of_range` ("setval: value n is out of bounds for sequence \"s\"
(min..max)"). On success: `last_value = n` and `is_called` = the third argument (default `true`); the
result is `n`. The effect on the next `nextval` follows from `is_called` — `setval('s', n)` (called)
makes the next `nextval` return `n + increment`; `setval('s', n, false)` makes it return `n`.
`setval` is a **write** (the write path + `25006` in a read-only txn, exactly like `nextval`) and is
**transactional** (rolls back — §5). Two deliberate session-state asymmetries with `nextval`, both
matching PG (verified against the oracle): (a) `setval` updates `currval` **only when `is_called` is
true** (with `is_called = false`, `currval` keeps its prior value / stays `55000`); (b) `setval`
**never** updates `lastval` — `lastval` tracks `nextval` alone (§6).

**`ALTER SEQUENCE [IF EXISTS] s RESTART [WITH n]`** resets the counter: `last_value = n` (`RESTART
WITH n`) or `last_value = start` (bare `RESTART`, the original `START` — `RESTART` does **not** change
the stored `start`), with `is_called = false`, so the next `nextval` returns that value. A value
outside `[min_value, max_value]` is `22023 invalid_parameter_value` (PG: "RESTART value (n) cannot be
greater than MAXVALUE (max)" / "… less than MINVALUE (min)" — note `22023`, **not** `setval`'s
`22003`). A missing sequence is `42P01` unless `IF EXISTS` (then a no-op). `ALTER` is a catalog
mutation (the write path, transactional) and touches **no** session state — `currval`/`lastval` are
unchanged by a `RESTART`.

## 5. The defining decision — transactional sequences (a documented PG divergence)

> **jed sequences are transactional: `nextval` rolls back with its transaction.** This is a
> deliberate divergence from PostgreSQL, where sequence advances are **non-transactional** (never
> rolled back; gaps are allowed) — and it is already mandated by
> [determinism.md §5](determinism.md): "Sequences / `SERIAL` / identity columns … are **fully
> deterministic** (a monotonic counter, reconstructed on load) and stay inside the contract. **Do not
> exempt them.**"

The reasoning, recorded here and in the override ledger:

- **PG's non-transactionality is a concurrency optimization jed does not need.** PG lets `nextval`
  escape transaction rollback so concurrent sessions never block on a sequence — the cost is gaps. jed
  is **single-writer** (CLAUDE.md §3): there is no concurrent contention on the counter to optimize
  away, so the only thing PG's gaps would buy us is nondeterminism.
- **It would break the cross-core determinism contract.** A non-transactional counter advancing across
  rollback is observable mutable state outside the snapshot; to keep `rust == go == ts` byte-identical
  it would need a host **seam** and a **determinism-ledger exemption**
  ([determinism_exceptions.toml](../conformance/determinism_exceptions.toml)) — exactly what
  determinism.md §5 forbids for counters. Transactional sequences need **neither**: the counter is an
  ordinary snapshot field, deterministic by construction, reconstructed from disk on open.
- **What is observably different from PG:** after `BEGIN; SELECT nextval('s'); ROLLBACK;`, jed's `s` is
  unchanged; PG's `s` keeps the advance. After a failed statement inside a transaction, same. Within a
  *committed* transaction the behavior is identical to PG (the advance persists, `currval` sees it).
  Two successful `nextval`s in autocommit also match PG (each commits). The divergence surfaces
  **only** on rollback/abort.

This makes the entire feature deterministic with no new seam: a sequence's value is a pure function of
`(CREATE options, the committed sequence of nextval/setval calls)`, identical on every core.

## 6. `currval` / `lastval` — per-session state, not snapshot state

`currval('s')` returns the value most recently produced by `nextval('s')` **in the current session**
(handle) — PostgreSQL's strictly session-local semantics, **independent of other sessions and of the
committed sequence value**. This is **per-handle transient state**, NOT part of the snapshot and NOT
persisted:

- Each `Database` handle carries a small `session_seq: map<lowercased-name → i64>` of the last value
  this handle's `nextval`/`setval` produced for each sequence, plus a single `session_last: i64?` —
  the most-recent-overall value `nextval` returned (the `lastval` source).
- `currval('s')` before any `nextval('s')`/`setval('s', …, true)` in this session is `55000
  object_not_in_prerequisite_state` ("currval of sequence \"s\" is not yet defined in this session") —
  even if the sequence exists and another session advanced it. A missing sequence is still `42P01`
  (the name is resolved against the catalog first).
- **`lastval()`** returns `session_last` — the value the most recent `nextval` (of **any** sequence)
  returned in this session — and is `55000 object_not_in_prerequisite_state` ("lastval is not yet
  defined in this session") before the first `nextval`. It takes **no name argument** (no `42P01`
  path). Two oracle-pinned details: `lastval` reads `nextval`'s history **only** (a `setval` never
  updates `session_last`, so `lastval` is unaffected by it), and `currval`'s per-sequence map **is**
  updated by `setval('s', n)` (called) but **not** by `setval('s', n, false)`.
- Because jed read results depend only on commit order + the session's own call history (never wall
  clock), `currval` is deterministic *given the session's statement sequence* — the conformance corpus
  is single-handle and sequential, so it pins `currval` directly; the cross-session independence is a
  per-core / concurrency-suite concern.
- **Does `currval`/`lastval` survive rollback?** jed records the session values (`session_seq`,
  `session_last`) on a *successful* `nextval`/`setval` evaluation, flushed together with the counter
  advance on statement success. Since a rolled-back `nextval`/`setval` did not commit and (jed §5) did
  not advance the sequence, the session values it set are also discarded on rollback, keeping
  `currval`/`lastval` consistent with the transactional counter. (Moot under PG-comparison since PG's
  session value *would* survive — another facet of the §5 divergence, ledgered.)

## 7. `CACHE` — accepted, never value-burning (a documented PG divergence)

PostgreSQL's `CACHE c` pre-allocates `c` values to a session at once, so values are consumed in
per-session blocks and **gaps appear** when a session exits with cache unused (and the cached block is
non-transactional). That is two nondeterminism sources at once (cross-session block interleaving + gap
on disconnect), incompatible with the §5 determinism stance. jed therefore **parses and stores
`CACHE c`** (so the clause is accepted and round-trips for fidelity and future `ALTER`) but
**behaves as `CACHE 1`**: every `nextval` advances the single shared counter by exactly `increment`,
no per-session reservation, no gaps. Recorded as a divergence. (`CACHE` < 1 is `22023`.)

## 8. Cost

A new `sequence_advance` cost unit ([../cost/schedule.toml](../cost/schedule.toml)) is charged once
per `nextval`/`setval` evaluation, **in addition** to the one `operator_eval` every function call
already rides — the advance touches and rewrites a catalog tuple, more than a pure value→value map.
`currval` is a pure session-state read and charges only its `operator_eval`. The unit keeps a runaway
`nextval` (e.g. a `generate_series`-driven sweep) bounded by `max_cost` (CLAUDE.md §13, the deterministic
ceiling) and keeps cost cross-core identical (CLAUDE.md §8). Catalog-tuple I/O is schema-bounded, so a
flat per-call weight is a sound bound.

## 9. Errors

| Failure | Code |
|---|---|
| `CREATE SEQUENCE` name already a relation (no `IF NOT EXISTS`) | `42P07` duplicate_table |
| `nextval`/`currval` on a missing sequence | `42P01` undefined_table |
| `DROP SEQUENCE` of a missing sequence (no `IF EXISTS`) | `42P01` undefined_table |
| `DROP SEQUENCE` of an `OWNED` (`serial`) sequence, RESTRICT (§12) | `2BP01` dependent_objects_still_exist |
| `serial` column with an explicit `DEFAULT` (§12) | `42601` syntax_error |
| `setval`/`ALTER … RESTART`/`nextval`/`currval` on a missing sequence | `42P01` undefined_table |
| `currval` before `nextval`/`setval(…,true)`; `lastval` before any `nextval`, this session | `55000` object_not_in_prerequisite_state |
| `nextval` past `MAXVALUE`/`MINVALUE` without `CYCLE` | `2200H` sequence_generator_limit_exceeded |
| `setval` value outside `[MINVALUE, MAXVALUE]` | `22003` numeric_value_out_of_range |
| `INCREMENT 0`, `CACHE < 1`, inconsistent `START`/`MIN`/`MAX`, or `RESTART` value out of bounds | `22023` invalid_parameter_value |
| `nextval`/`setval`/`ALTER SEQUENCE` in a read-only transaction | `25006` read_only_sql_transaction |
| Corrupt sequence catalog entry | `XX001` data_corrupted |
| `ALTER SEQUENCE` actions other than `RESTART`; `DROP SEQUENCE … CASCADE`; `AS type` typmod; `serial`; identity | `0A000` feature_not_supported |

## 10. Ratified decisions and deliberate PostgreSQL divergences

Default is "match PostgreSQL" (CLAUDE.md §1); the divergences below each have an overriding reason and
are recorded in [../conformance/oracle_overrides.toml](../conformance/oracle_overrides.toml).

1. **Transactional sequences** — `nextval` rolls back with the transaction (§5). Overriding reason:
   determinism (CLAUDE.md §8/§10, determinism.md §5) + the single-writer model removes PG's
   concurrency rationale. The headline divergence.
2. **`CACHE` is no value-burning** — accepted and stored, behaves as `CACHE 1` (§7). Same reason.
3. **`bigint`-only** — no `AS smallint|integer` typmod this slice (`0A000`); jed sequences are i64.
4. **No implicit dependency from a plain `DEFAULT nextval('s')`** — matches PG (only `serial`/identity
   create one); `DROP SEQUENCE` needs no dependency tracking this slice (§1).
5. **`nextval`/`setval` make the statement a write** — required by the single-writer staging model
   (§4); a `SELECT nextval('s')` commits a new snapshot. Observably matches PG (the value persists).
6. **Session-local `currval`/`lastval` with `55000`** — adopted as-is (§6).
7. **`setval`/`ALTER RESTART` are transactional** — they roll back like `nextval` (§5, the same
   reason). Their session-state asymmetries match PG exactly (`setval(…,false)` leaves `currval`
   alone; `setval` never touches `lastval`; `RESTART` touches no session state — §4/§6).
8. **`setval` out-of-bounds is `22003`, `RESTART` out-of-bounds is `22023`** — the two distinct PG
   error paths (`do_setval` vs. `init_params`) are preserved as-is (§4).
9. **`serial` sequences are `bigint`-flavored** (§12) — `serial`/`smallserial` do NOT get PG's
   per-type `AS integer`/`AS smallint` sequence (the `AS type` typmod is `0A000` this slice, decision
   3). The owned sequence is the default ascending i64 sequence; the *column* is `i32`/`i16` and
   bounds stored values (a too-large `nextval` traps `22003` at the INSERT coercion rather than PG's
   `2200H` at the sequence). Overriding reason: the deferred `AS type`. Same divergence-class as
   decision 3, ledgered.
10. **The owned (`serial`) sequence is dropped with its table; dropping it alone is `2BP01`** (§12) —
    the `OWNED BY` dependency. Matches PG. Unlike a *plain* `DEFAULT nextval('s')` (decision 4, no
    dependency), a `serial`-created sequence carries a persisted owner link.

## 11. Delivery (sub-slices)

Sequences are **not a single vertical slice**. They land as ordered, independently-shippable
sub-slices, each passing `rake ci`:

- **S0** — this design doc + the error-code registrations + the CLAUDE.md §9 / TODO.md touch + the §5
  determinism divergence recorded. The decisions are ratified spec-first before any core changes.
- **S1** — `CREATE SEQUENCE` (full option grammar) / `DROP SEQUENCE` + the `sequences` catalog map +
  `format_version` 12 + the `sequence_table.jed` golden (`rust == go == ts == ruby`) + `nextval` +
  `currval` + the `sequence_advance` cost unit + the write-path detection + the read-only `25006` +
  the conformance corpus (`ddl/sequence.test`, `expr/sequence_value.test`) + capabilities
  `ddl.sequence` / `func.sequence`. The "it's alive" slice — a sequence is created, advanced, read,
  persisted, and dropped.
- **S2** ✅ — `setval(s, n[, is_called])` + `lastval()` + `ALTER SEQUENCE [IF EXISTS] s RESTART
  [WITH n]` + the `session_last` lastval source + corpus coverage of `CYCLE` wraparound and the
  bound errors. `setval`/`ALTER RESTART` reuse the `nextval` write-path + transactional-rollback
  machinery (the `pending_seq` flush); `setval` charges the existing `sequence_advance` unit. With
  `setval`/`ALTER RESTART` available, the corpus reaches a known counter state in **one replayable
  statement** and then asserts a single `nextval`/`currval`, so the S1 "advance via `statement ok`,
  read via terminal `query`" scaffolding is replaced by direct `setval`-then-assert checks.
- **S3** ✅ — the `serial` / `bigserial` / `smallserial` pseudo-types (§12): column sugar that, at
  `CREATE TABLE`, creates an **owned** sequence + a `DEFAULT nextval(...)` + `NOT NULL` and records
  the `OWNED BY` link, so `DROP TABLE` auto-drops the owned sequence and `DROP SEQUENCE` of an owned
  sequence is `2BP01`. The owner reference is persisted (**`format_version` 14**, the `has_owner`
  flag bit + trailing owner bytes — §3), with the `serial_table.jed` golden
  (`rust == go == ts == ruby`) and capability `ddl.serial`. `serial` sequences are `bigint`-flavored
  (the `AS type` deferral, §12) — a documented divergence.
- **S4** — `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY` columns (the SQL-standard identity surface)
  + `OVERRIDING { SYSTEM | USER } VALUE`.

Each later slice is its own design-doc revision + corpus + (where it touches bytes) a `format_version`
note and golden.

## 12. `serial` / `bigserial` / `smallserial` — the auto-sequence pseudo-types (S3)

`serial` is not a real type: it is **`CREATE TABLE` column sugar** (PostgreSQL) for "an auto-numbered
column backed by its own sequence". jed adopts PG's desugaring exactly. A column declared

```sql
CREATE TABLE t (id serial, ...)
```

is, at `CREATE TABLE` execution, expanded to the equivalent of

```sql
CREATE SEQUENCE t_id_seq;                              -- an OWNED sequence (default i64, §3)
-- column id  =>  i32 NOT NULL DEFAULT nextval('t_id_seq')
-- the sequence is OWNED BY t.id  (DROP TABLE t auto-drops t_id_seq; §12.3)
```

### 12.1 The three pseudo-types and their aliases

Recognized **only** in a `CREATE TABLE` column-type position (not in `CAST`, `CREATE SEQUENCE … AS`,
a `CREATE TYPE` field, or an array element type — there `serial` is an undefined type, the existing
`42704`/“type does not exist” path, matching PG):

| Pseudo-type | Aliases | Column type |
|---|---|---|
| `serial` | `serial4` | `i32` (`integer`) |
| `bigserial` | `serial8` | `i64` (`bigint`) |
| `smallserial` | `serial2` | `i16` (`smallint`) |

The pseudo-type sets **only** the column's underlying type; everything else is the desugaring below.
A `serial` column carries **no typmod** and **no `[]` array suffix** — `serial(p)` / `serial[]` is a
`42601` syntax-style rejection (jed: `42601`; the natural “undefined type” path also suffices — PG
rejects `serial[]` as `0A000` "array of serial is not implemented", a wording divergence not worth a
dedicated branch).

### 12.2 The desugaring (at `CREATE TABLE`)

For each `serial` column `c` of table `t`, in column declaration order:

1. **Column type** ← `i32` / `i64` / `i16` per the table above.
2. **`NOT NULL`** is forced on the column (PG: a `serial` column is `NOT NULL`). An explicit
   `NOT NULL` is harmlessly redundant; an explicit `NULL` is not a jed surface, so no conflict arises.
3. **An explicit `DEFAULT` is rejected** `42601` — "multiple default values specified for column
   *c* of table *t*" (PG): a `serial` column's default is `nextval(...)`, and a second default is an
   error.
4. **The owned sequence** is created: a **default ascending i64 sequence** (`INCREMENT 1`,
   `MINVALUE 1`, `MAXVALUE 2^63-1`, `START 1`, `CACHE 1`, no `CYCLE`) — the same `SequenceDef`
   `CREATE SEQUENCE t_id_seq` would build, **plus** an `owned_by = (t, ordinal-of-c)` link (§3,
   persisted v14). It is `bigint`-flavored for all three pseudo-types (decision 9 — the `AS type`
   deferral); the *column's* narrower type bounds stored values.
5. **The sequence name** is `lower(t)_lower(c)_seq`. If that name already names a relation
   (table / index / sequence — the shared relation namespace, §2), including a sequence created by an
   *earlier* `serial` column of the **same** statement, the smallest integer suffix `1`, `2`, … is
   appended until free (`t_id_seq` taken → `t_id_seq1`), matching PG's `ChooseRelationName`.
6. **The column `DEFAULT`** becomes the expression default `nextval ( '<seqname>' )` — stored as
   expression text (the `format_version` 8 expression-default mechanism — [constraints.md §2](constraints.md)),
   evaluated per row at INSERT through the existing `nextval` write path (which advances the sequence
   and is transactional, §4/§5). Supplying an explicit value for the column at INSERT overrides the
   default and does **not** advance the sequence (PG — the value and the counter can later collide,
   exactly as in PG).

A `serial` column may be a `PRIMARY KEY` (the common `id serial PRIMARY KEY` form): the `i32`/`i64`/`i16`
type is key-encodable, so the PK gate is satisfied; the column is then PK + `NOT NULL` + the
`nextval` default.

### 12.3 `OWNED BY` — the dependency (`DROP TABLE` auto-drop, `DROP SEQUENCE` 2BP01)

The persisted `owned_by` link (§3) is the only thing distinguishing a `serial`-created sequence from a
manually-created one referenced by a plain `DEFAULT nextval('s')` (which creates **no** dependency,
decision 4). It drives two behaviors, both matching PG:

- **`DROP TABLE t`** auto-drops every sequence `owned_by` `t`, silently, as part of the drop (after
  the `FOREIGN KEY`-dependent check, which an owned sequence never trips). The owned sequences are
  removed from the catalog before the table; the drop stays zero-cost. After reopening a persisted
  database the link is still present (v14), so the auto-drop survives a close/open cycle.
- **`DROP SEQUENCE s`** where `s` is owned is `2BP01 dependent_objects_still_exist` —
  "cannot drop sequence *s* because other objects depend on it: default value for column *c* of table
  *t* depends on sequence *s*" (RESTRICT; `CASCADE` is `0A000` this slice). The owner table is always
  present when this fires (the table's own `DROP TABLE` would have auto-dropped the sequence first),
  so the column name for the detail is always resolvable.

`DROP TYPE` / `DROP INDEX` are unaffected (a sequence is owned only by a table column). There is no
standalone `OWNED BY` / `ALTER SEQUENCE … OWNED BY` surface this slice — ownership is established
**only** by `serial` (the `OWNED BY` clause stays `0A000`, §9's `ALTER` row).

### 12.4 Determinism

`serial` adds no new nondeterminism: the owned sequence is an ordinary snapshot field (transactional,
§5), the auto-generated name is a pure function of `(table, column)` + the catalog (the suffix scan is
deterministic), and the auto-drop is a deterministic catalog edit. The whole feature is byte-identical
cross-core, pinned by the `serial_table.jed` golden (§3) and the `ddl/serial.test` corpus.
