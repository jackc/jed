# `ALTER TABLE` — design

> The semantics of `ALTER TABLE`. The **grammar is authoritative** for the surface
> ([../grammar/grammar.ebnf](../grammar/grammar.ebnf) — `alter_table`); the **error
> registry** ([../errors/registry.toml](../errors/registry.toml)) owns the codes; the
> **on-disk format** ([../fileformat/format.md](../fileformat/format.md)) owns the catalog
> encoding; [constraints.md](constraints.md) owns the constraint semantics an `ADD
> CONSTRAINT` reuses. This doc is the *why* and the precise behavior the three cores must
> reproduce identically (CLAUDE.md §2, §8). When a decision here changes, change the
> data/grammar and here in the same edit.
>
> **Status: Slices 1–5 landed.** The canonical grammar and all three native cores implement the
> catalog-only frame: renames, column defaults/nullability, and `ADD`/`DROP CONSTRAINT` for CHECK,
> UNIQUE, FOREIGN KEY, and EXCLUDE, including comma-action atomicity and validating scans; `ADD
> COLUMN` appends the catalog column and atomically rebuilds the table (and re-keys it for an inline
> PRIMARY KEY), `DROP COLUMN` physically removes non-PK columns while compacting dependent
> ordinals, `ALTER COLUMN TYPE` converts each row, and standalone `ADD`/`DROP PRIMARY KEY` re-key
> between declared keys and synthetic rowids.

`ALTER TABLE` mutates a table's definition in place — its columns, its constraints, its
name. It is the last major DDL gap: `CREATE TABLE` / `DROP TABLE` / `CREATE INDEX` /
`DROP INDEX` / the `ALTER SEQUENCE` forms exist, but a created table's shape is frozen.

## 0. Two mechanical facts that shape everything

Every decision below follows from how jed physically stores a table
([format.md](../fileformat/format.md)):

1. **Columns are identified by a dense 0-based ordinal** (declaration position), and *every*
   catalog structure references them **positionally** — the `pk_ordinal` list, each index
   `key_element`, the FK `fk_local_ordinal` / `fk_ref_ordinal` lists, the `excl_col_ordinal`
   list. Leaf pages are physically **column-major by ordinal**, with a `K+1`-entry column
   directory where `K` = the catalog's `col_count`; a leaf's column count *must equal* the
   catalog's (there are no ragged rows). Consequence: **changing the set or order of columns
   changes the physical row layout** and, if it shifts ordinals, every positional reference.
2. **`CHECK` / `DEFAULT` / index-key / partial-predicate expressions are stored as
   re-parsed text** ([format.md](../fileformat/format.md), *Check-expression text*) that
   re-resolves against columns **by name** at load. Consequence: **renaming or dropping a
   column can silently break stored expression text** unless the text is rewritten with it.

These split the surface into two tiers: **catalog-only** edits (§2 — cheap, no data
touched, no `format_version` bump) and **table rewrites** (§3 — rebuild the table's B-tree).

## 1. Grammar and the all-or-nothing model

```
alter_table         ::= "ALTER" "TABLE" ("IF" "EXISTS")? qualified_table
                        ( "RENAME" ( "COLUMN"? identifier "TO" identifier
                                   | "CONSTRAINT" identifier "TO" identifier
                                   | "TO" identifier )
                        | alter_table_action ("," alter_table_action)* )
alter_table_action  ::= "ADD" "COLUMN"? ("IF" "NOT" "EXISTS")? column_def
                      | "ADD" table_constraint
                      | "DROP" "COLUMN"? ("IF" "EXISTS")? identifier ("CASCADE" | "RESTRICT")?
                      | "DROP" "CONSTRAINT" ("IF" "EXISTS")? identifier ("CASCADE" | "RESTRICT")?
                      | "DROP" "PRIMARY" "KEY" ("CASCADE" | "RESTRICT")?
                      | "ALTER" "COLUMN"? identifier alter_column_action
alter_column_action ::= "SET" "DEFAULT" expr
                      | "DROP" "DEFAULT"
                      | "SET" "NOT" "NULL"
                      | "DROP" "NOT" "NULL"
                      | ("SET" "DATA")? "TYPE" type_name ("USING" expr)?
```

`column_def` and `table_constraint` are the exact `CREATE TABLE` productions
([constraints.md](constraints.md)) — an added column carries its own inline constraints, an
added constraint is the same composite-capable form. `COLUMN` / `DATA` are noise words
(PG-faithful) and, like every keyword here, **not reserved** (grammar.md §3): a table or
column may be named `add`, `alter`, `column`, `rename`, `type`; the parser distinguishes
positionally. `qualified_table` accepts an attached-database qualifier (`db.t`,
[attached-databases.md](attached-databases.md)); the ALTER routes to that database's catalog.

**The `RENAME` forms are standalone** — a single rename, never comma-combined and never
mixed with other actions (PG's split; the same split `alter_sequence` already makes between
`RENAME TO` and its option list). Everything else is the **comma-separated multi-action**
form.

**Multi-action is two-phase / all-or-nothing** (the model INSERT/UPDATE/DELETE and `DROP
TABLE`'s comma list already use). The whole action list is validated against the resolved
**end state** — actions apply left-to-right to an in-memory working copy of the catalog (a
later action sees an earlier one's effect: `ADD COLUMN c … , ALTER COLUMN c SET NOT NULL` is
legal), the end state is checked, and only then is anything written. Any failure aborts the
entire statement with nothing changed. The edit stages in the writer's pending write-set and
commits atomically under the single-writer model (§3); on commit it **bumps the catalog
generation** so the prepared-plan cache invalidates (the existing `catGen` mechanism).

`IF EXISTS` on the table makes a missing table a no-op success (`42P01` otherwise; a
non-table relation is `42809`). `IF NOT EXISTS` on `ADD COLUMN` / `IF EXISTS` on `DROP
COLUMN` / `DROP CONSTRAINT` make that one action a no-op instead of `42701` / `42703` /
`42704`.

## 2. Catalog-only forms (Tier A — no data rewrite)

These change only the table's catalog entry. No leaf page is touched; **no `format_version`
bump** (an old reader would reject the file only on the version byte, so a form that adds no
new catalog field could even stay version-compatible — decided per slice).

### 2.1 `RENAME TO` (table)

Move the catalog key to the new name. `42P07` (`relation already exists`) if the new name is
already a table/index/sequence in the same database (the shared relation namespace); renaming
to the current name is `42P07` too (PG-faithful). An owned sequence's `nextval` default text
references its table by name — a table rename rewrites those owned-sequence defaults (the
mirror of `ALTER SEQUENCE … RENAME`'s §15.3 rewrite, [sequences.md](sequences.md)).

### 2.2 `RENAME COLUMN … TO …`

Change the column's `col_name`. `42703` if the old name is unknown, `42701` if the new name
collides with another column. Because indexes/PK/FK/EXCLUDE reference the column **by
ordinal** (§0.1), none of them needs touching — **but every stored expression text (§0.2)
that names the column must be rewritten**: this table's `CHECK` and `DEFAULT` expressions,
its expression-index keys and partial predicates, and — because a *parent*'s FK resolves
child columns by name only at load — nothing on the parent side (FK stores ordinals), but a
**child** table's dependency is by ordinal too, so only *this table's own* expression text is
at risk. The rewrite re-parses each stored expression, substitutes the identifier, and
re-serializes the canonical text; a name that appears only as a quoted string or unrelated
identifier is untouched. (This is the one Tier-A form with a sharp edge — it is why `RENAME`
is its own slice.)

### 2.3 `RENAME CONSTRAINT … TO …`

Change a constraint's catalog name. `42704` if unknown, `42710` if the new name already names
a constraint of the table (rename-to-self included, PG-faithful). For a UNIQUE / EXCLUDE
constraint the backing index shares the name; renaming the constraint renames the backing
index and its store with it (they are one object — [indexes.md §8](indexes.md)). Because that
name also lives in the relation namespace, a relation collision is `42P07` and the reserved
`jed_` prefix is `42939`; CHECK / FK names remain exempt because they own no relation
([introspection.md §4](introspection.md)).

**Deliberate PG divergence (ledgered §7):** jed persists/reserves no PRIMARY KEY constraint
object — the `<t>_pkey` name is derived for introspection. Therefore `RENAME CONSTRAINT
<t>_pkey TO …` is `42704`, where PostgreSQL renames it. Persisting a custom PK name needs a
new catalog field, so it is deferred with the PK re-key slice (§3.4).

### 2.4 `ALTER COLUMN … SET DEFAULT expr` / `DROP DEFAULT`

Set or clear the column's default. `SET DEFAULT` re-runs `CREATE TABLE`'s default handling: a
constant is coerced once and stored as value bytes (flag bit2); a non-constant expression
(`uuidv7()`, `1 + 1`) is stored as text and evaluated per row through the entropy/clock seam
(flag bit3, [constraints.md §2](constraints.md)). `DROP DEFAULT` clears both bits. A default
that does not coerce to the column type is `42804`. Existing rows are **not** rewritten — a
default only affects future inserts (PG-faithful).

An IDENTITY column rejects both forms with `42601` (PG-faithful): its synthesized default is part
of identity management, which remains in the deferred identity-specific ALTER surface (§6).

### 2.5 `ALTER COLUMN … SET NOT NULL` / `DROP NOT NULL`

Flip flag bit1. `DROP NOT NULL` is pure catalog (a PK member cannot drop NOT NULL — `42P16`).
`SET NOT NULL` needs a **validating full scan**: any existing NULL in the column traps
`23502` and aborts, leaving the catalog unchanged. The scan is metered like any read.
An IDENTITY column cannot `DROP NOT NULL` (`42601`, PG-faithful); that invariant belongs to the
deferred `DROP IDENTITY` form rather than this generic nullability edit.

### 2.6 `ADD table_constraint`

Add a `CHECK` / `UNIQUE` / `FOREIGN KEY` / `EXCLUDE` (and `PRIMARY KEY` — but that re-keys, so
it is Tier B, §3.4). Reuses the `CREATE TABLE` constraint machinery
([constraints.md](constraints.md)) plus a **validating scan of existing rows** against the
end state: a `CHECK` violated by a current row is `23514`; a `UNIQUE` with existing duplicates
is `23505` (and the scan **builds the backing index**); a `FOREIGN KEY` with an orphan child
row is `23503`; an `EXCLUDE` with a conflicting pair is `23P01`. Validation uses the same
**end-state** semantics as everything else (§4). This retires the standing
`ALTER TABLE … ADD CONSTRAINT` follow-ons noted under the FK and EXCLUDE items in
[TODO.md](../../TODO.md). The scan charges its ordinary page/row/evaluation units; additionally,
each candidate tuple in a self-referential FK search and each unordered EXCLUDE row pair charges
`constraint_check` and guards immediately, bounding the otherwise quadratic validation work
deterministically ([cost.md](cost.md) §3).

The standing CREATE TABLE scope narrowings also apply here: adding a `FOREIGN KEY` or `EXCLUDE`
constraint to a session-local temporary table or an attached-database table is `0A000`. The scope
gate runs before resolving the constraint so an unsupported catalog cannot be partially constructed.

### 2.7 `DROP CONSTRAINT`

Remove a named constraint. `42704` if unknown (unless `IF EXISTS`). A `CHECK` / `FOREIGN KEY`
is pure catalog removal. A `UNIQUE` / `EXCLUDE` also drops its backing index (this becomes the
named handle for removing a unique index — the inverse of the [indexes.md §8](indexes.md)
note that `DROP INDEX` is currently the only way). `RESTRICT` (default) refuses to drop a
`UNIQUE`/`PRIMARY KEY` a `FOREIGN KEY` references (`2BP01`); `CASCADE` drops those FKs with it.

## 3. Table-rewrite forms (Tier B — rebuild the B-tree)

Because the leaf layout is positional and `K` is pinned to `col_count` (§0.1), these
**re-pack every leaf** of the table (and rebuild affected indexes). The key property:
**a rewrite emits an ordinary current-format table, so none of these needs a `format_version`
bump** — the expensive-but-clean path is also the format-neutral one, and the "dataset fits
in RAM" design target (CLAUDE.md §9) makes a whole-table rewrite at ALTER time acceptable.
The rewrite runs inside the writer's staging buffer and commits atomically like any mutation.

### 3.1 `ADD COLUMN [constraints]`

Append a column entry (new highest ordinal — appended, never inserted mid-list, so no
existing ordinal shifts) and rewrite each leaf to add the new column region. Each existing
row's new value is its `DEFAULT` (a constant, or a per-row expression through the
entropy/clock seam — so `ADD COLUMN id uuid DEFAULT uuidv7()` gives every row a distinct
deterministic-given-the-seam value), or NULL if no default. `ADD COLUMN … NOT NULL` with no
default over a non-empty table is `23502` (PG-faithful). Inline `UNIQUE` / `PRIMARY KEY` /
`REFERENCES` validate as in §2.6/§3.4; a `serial` / `IDENTITY` column auto-creates its owned
sequence ([sequences.md](sequences.md)).

**Implemented slice-3 details.** Multiple added columns share one key-ordered rewrite scan and one
statement entropy/clock seam. The value placed into old rows is the default captured by that `ADD`
action; a later comma-action `ALTER c SET DEFAULT ...` changes future inserts, not the scheduled
rewrite value. An inline PRIMARY KEY is legal only when the table has no PK, re-encodes every table
key, and rebuilds every secondary-index suffix; duplicate rewritten keys are `23505` on `<t>_pkey`.
The rewrite retains the no-PK synthetic-rowid high-water when no PK is added. All validation and
sequence advancement are statement-atomic. Owned-sequence auto-naming reserves named inline index
additions, and later index actions see pending owned sequences, so the two cannot overlap in the
shared relation namespace; each replacement row also pays the ordinary `value_compress` write charge
before the cost ceiling is checked and before the rewrite is published.

### 3.2 `DROP COLUMN`

Remove the column and rewrite each leaf without its region — **and renumber every surviving
positional reference** (§0.1): each `pk_ordinal` / `key_element` / FK / `excl_col_ordinal`
greater than the dropped ordinal decrements by one. Dependency handling matches PostgreSQL's
internal/external distinction: same-table `CHECK`, index (including a UNIQUE backing index), and
`EXCLUDE` objects that use the column are internally dependent and are dropped automatically even
under `RESTRICT` (the default). A FOREIGN KEY owned by the column through its local (referencing)
side is likewise dropped automatically. An FK that uses the column only on its referenced side
(including a self-referential FK) is an external dependency and blocks with `2BP01` unless
`CASCADE` drops it. Dropping a column also rewrites any stored expression text (§0.2) that survives;
a `CHECK`, expression index, or partial-index predicate that referenced the column is removed with
its owning object.
Dropping a **PK member** implies dropping the whole PK and re-keying to synthetic rowid (§3.4)
— the hairy case; deferred to its own slice. A column referenced by a **parent** FK (this
table is the parent) blocks under RESTRICT.

**Deliberate PG divergence (ledgered §7):** PG keeps a dropped column as a tombstone
(`attisdropped`, ordinal never reused, dead bytes retained in every row forever); jed
**physically removes** it and compacts ordinals. jed's dense-ordinal model has no room for a
tombstone, and a full rewrite keeps the file clean — consistent with "boring, explicit"
(CLAUDE.md §10) and the RAM-sized design target. Observable difference: in jed a later
`ADD COLUMN` may reuse the name/position; introspection ([introspection.md](introspection.md))
never shows a dropped column.

**Implemented slice-4 details.** Multiple DROP/ADD actions share one key-ordered rewrite scan. The
executor carries each final column as either an original-row ordinal or an ADD action's captured
default, so arbitrary left-to-right combinations publish one final dense row shape. Surviving local
PK/index/FK/EXCLUDE ordinals, incoming FK referenced ordinals, and owned-sequence owner ordinals are
compacted together; a dropped serial/IDENTITY column auto-drops its owned sequence. A non-PK drop
does not re-key the table: PK values and storage-key bytes are unchanged even when their catalog
ordinals shift. Existing unaffected index stores likewise remain valid; dependent stores are removed
and newly-added constraint stores are built only after final-state validation. A dropped backing-index
name reused by a later action identifies a new object, so its store is rebuilt even though the final
catalog name matches the original. Dropping a serial/IDENTITY column removes its existing owned sequence
from the action-visible relation namespace immediately: a later serial addition may reuse the released
name, while a later default cannot resolve or advance the removed sequence.

### 3.3 `ALTER COLUMN … TYPE type [USING expr]`

Re-encode every value of the column to the new type (via the identity cast, or the `USING`
expression evaluated per row), re-validate against the column's constraints, and rebuild every
index/constraint that touches it. The hardest form: a failed cast is the cast's own error
(`22003` overflow, `22P02` malformed, etc.); a `USING` expression is a general per-row
expression over the row's columns. If the column is a key member, its key encoding changes, so
the table (and dependent indexes) re-key — a full rebuild with final-key uniqueness validation.
The target type must be key-encodable before PRIMARY KEY metadata is retained (`0A000` otherwise).

The conversion uses jed's explicit cast matrix: without `USING`, the old column value is cast to
the named target; with `USING`, the expression result is cast to it. Actions apply left-to-right,
so a `USING` expression sees the row shape and types produced by earlier actions in the same
statement. Existing defaults are converted and retained; an incompatible default aborts the ALTER.
Every surviving CHECK, UNIQUE, FK, EXCLUDE, expression index, and partial predicate is re-resolved
and validated against the end state, and all secondary indexes are rebuilt because an expression
may reference the changed column indirectly.

An IDENTITY column's owned sequence changes integer type atomically with the column, including a
sequence pending from an earlier `ADD COLUMN` action in the same statement. Matching PostgreSQL,
bounds equal to the old type defaults become the new type defaults, explicit bounds are preserved,
and the sequence START/current value must fit the resulting type and bounds (`22023` otherwise).

### 3.4 `ADD` / `DROP PRIMARY KEY`

Not catalog-only: the PK **is** the row key. `ADD [CONSTRAINT name] PRIMARY KEY (…)` over a table currently on
synthetic rowids re-keys every row onto the new key (validating uniqueness → `23505`, and
NOT-NULL on each member → `23502`) — a full B-tree rebuild that **reuses the existing
UPDATE-of-PK re-keying path** ([constraints.md §6.5/§6.7](constraints.md)). `DROP PRIMARY KEY`
re-keys back to fresh dense synthetic rowids in old-key scan order and retains the members' NOT NULL
status (PostgreSQL also retains it). `RESTRICT` is the default and blocks a referenced PK with
`2BP01`; `CASCADE` removes the dependent FKs. Both are rewrites; sequenced after §3.1–§3.3. jed
still derives `<table>_pkey` rather than persisting a custom PK name; a name in the ADD spelling is
accepted consistently with CREATE TABLE but does not create a renameable constraint object.

## 4. Validation semantics — end-state, two-phase

Every validating form (§2.5, §2.6, §3.1, §3.4) uses jed's **end-state** constraint semantics,
not PG's per-row transient check: the statement's final table state is validated as a whole,
so a re-key or swap that is transiently invalid but finally valid **succeeds** where PG fails
the intermediate step — the same documented divergence UNIQUE and UPDATE-PK re-keying already
carry ([constraints.md §6.5](constraints.md)). The two-phase pass (validate the entire end
state, then write) gives per-statement atomicity without cross-statement transactions.
Incoming-FK validation reads this final cascade-adjusted state, so an FK removed by an earlier
`DROP PRIMARY KEY CASCADE` or dependent-column cascade cannot reject a later type action.

## 5. Conformance and cost obligations

- **Oracle-check** row/error behavior against PostgreSQL (`rake corpus:check`); ledger each
  divergence (dropped-column physical removal §3.2, end-state validation §4).
- **Per-core unit tests only for what the corpus cannot express** (CLAUDE.md §10): the
  dropped-column tombstone divergence, catalog/introspection state (ordinals after a drop),
  the on-disk rewrite (a golden round-trip proving a rewritten table is byte-identical to the
  equivalent freshly-`CREATE`d one), and `format_version`-neutrality of the rewrite forms.
- A rewrite form accrues normal `page_read` / write cost; pin a `# cost:` on the rewrite
  slices so the cross-core cost stays identical (CLAUDE.md §13).

## 6. Deliberately excluded (Tier C — we own our surface, CLAUDE.md §1)

Most of PG's `ALTER TABLE` menu is moot here and is **not** planned: `OWNER TO` / RLS /
`{ENABLE|DISABLE} TRIGGER` (no roles/triggers, §3); `ATTACH`/`DETACH PARTITION`, `INHERIT`
(no partitioning/inheritance); `SET TABLESPACE`, `SET (fillfactor=…)`, `SET STATISTICS`,
`CLUSTER ON`, `SET {LOGGED|UNLOGGED}` (no storage/planner knobs of that kind); `SET SCHEMA`
(no schemas). Each is a `0A000` if it ever reaches the parser. The one plausible **later**
addition is identity management (`ALTER COLUMN … ADD GENERATED …`, `SET GENERATED
{ALWAYS|BY DEFAULT}`, `RESTART`), since IDENTITY columns and `ALTER SEQUENCE … RESTART`
already exist — a small follow-on, not scheduled.

## 7. Divergence ledger

| Divergence | jed | PostgreSQL | Why |
|---|---|---|---|
| Dropped column | Physically removed; ordinals compacted; name/position reusable (§3.2) | Tombstoned (`attisdropped`); dead bytes retained; ordinal never reused | Dense-ordinal format has no tombstone slot; rewrite keeps the file clean (§0.1, CLAUDE.md §10) |
| Validation timing | End-state (§4) | Per-row transient | jed's standing end-state model (constraints.md §6.5); a finally-valid re-key succeeds |
| Column rename | Rewrites this table's stored expression text (§2.2) | Same effect via dependency graph | jed stores expression *text*, not a resolved node tree (§0.2) |
| Rename PK constraint | `<t>_pkey` is `42704` — no named PK object (§2.3); an ADD-supplied name is not persisted (§3.4) | Renames the auto-named or explicit PK constraint | jed persists no PK/NOT NULL constraint object; keeping the derived handle avoids a format field |
| Type conversion without `USING` | Uses jed's explicit `CAST` matrix (§3.3) | Requires an implicit/assignment cast | One canonical strict conversion matrix; potentially lossy conversions remain visibly bounded to DDL rather than relaxing assignment |
| Drop-primary-key spelling | `DROP PRIMARY KEY [CASCADE\|RESTRICT]` (§3.4) | `DROP CONSTRAINT <name>` | jed has no persisted PK constraint object/name to address |
| ALTER TABLE on a non-table | `42809` for an index or sequence | Lenient for some relation kinds | jed's ALTER TABLE owns only the table surface; object-specific ALTER statements remain separate |

## 8. Slicing

Ordered lowest-risk → highest, each a vertical slice (CLAUDE.md §10):

1. **✅ Grammar + `RENAME` + the catalog-only column edits** — `alter_table` production, the
   multi-action all-or-nothing frame, `RENAME {TO | COLUMN | CONSTRAINT}`, `SET/DROP DEFAULT`,
   `SET/DROP NOT NULL`. Zero format risk; establishes the whole scaffold.
2. **✅ `ADD` / `DROP CONSTRAINT`** — `CHECK` / `UNIQUE` / `FOREIGN KEY` / `EXCLUDE` with the
   validating scan (retires the FK/EXCLUDE `ADD CONSTRAINT` follow-ons in TODO).
3. **✅ `ADD COLUMN`** — the first rewrite; per-row default evaluation, inline constraints, and
   inline-PK re-keying.
4. **✅ `DROP COLUMN`** — the ordinal renumber + dependency cascade (non-PK columns).
5. **✅ `ALTER COLUMN TYPE`** + **`ADD`/`DROP PRIMARY KEY`** — the re-encode/re-key rewrites.

## 9. `format_version` impact

**None expected.** Tier A is catalog-field edits within the current entry layout; Tier B
rewrites produce ordinary current-format tables. A bump is required only if a slice needs a
*new* catalog field or leaf encoding — not anticipated for any form above. (Contrast the
tombstone alternative rejected in §3.2, which *would* have forced a format change.)
