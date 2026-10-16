# Stored definitions this binary cannot parse

> The contract all three cores implement for stored SQL text that the opening binary's parser
> rejects. It replaces the old rule that such text makes the whole catalog load fail with `XX001`.
> The error is `XX003` in [../errors/registry.toml](../errors/registry.toml). The byte layouts are
> unchanged ([../fileformat/format.md](../fileformat/format.md)); no `format_version` bump.

## 1. The problem

The catalog stores five kinds of SQL as canonical token text, and every core parses that text again
when it loads the catalog:

| Kind | Stored as | Design doc |
|---|---|---|
| view definition | view entry `definition` | [views.md](views.md) §3 |
| expression `DEFAULT` | column entry expression text | [constraints.md](constraints.md) §2 |
| `CHECK` constraint | check entry `check_expr` | [constraints.md](constraints.md) §4.5 |
| expression index key | index key element text | [indexes.md](indexes.md) §6 |
| partial-index predicate | index `predicate` text | [indexes.md](indexes.md) §9 |

A release can add syntax without a `format_version` bump, because the bytes that hold the text do not
change. `SEARCH`/`CYCLE` did so ([recursive-cte.md](recursive-cte.md)). An older binary that opens a
file a newer one edited can then meet text its parser rejects. Until now that failed the whole load
with `XX001`, which reports a healthy file as corrupt and locks the older program out of every table:
standalone, in shared multi-process mode ([locking.md](locking.md)), where a co-resident older process
reloads the catalog after a newer one commits, and under any future page service.

Text the parser rejects is not evidence of corruption. Page checksums already detect damage at rest,
and a catalog page that passes its checksum holds the bytes some jed wrote.

## 2. The rule

**Load keeps the object.** When a stored definition's text fails to parse, the load records the
object as **unparsed** and continues. Any parser error counts, including a nesting-depth refusal. Every
other check on the entry still applies and is still `XX001`: lengths, UTF-8 validity, flag bits,
ordinals, and a view's dependency list, output names, and acyclicity
([views.md](views.md) §3) are all checked from the stored bytes without parsing the text.

**Use fails, and only use.** A statement fails with **`XX003`** (`unparsable_stored_definition`) only
when it needs the parsed form of an unparsed object. Every other statement runs normally. The error is raised during planning or
per-statement resolution, before any row is read or written, so a failed statement changes nothing
and charges no cost (planning is unmetered, [cost.md](cost.md)).

**The text is never rewritten.** An unparsed object keeps its stored text byte for byte. Every commit
that re-serializes the catalog writes that text back unchanged, along with the rest of the entry (a
view's columns, dependencies, and check option; a check's host dependencies; an index's flags and
dependency lists). A newer binary that opens the file later finds the original definition and uses it
normally. No statement edits an unparsed object's text: the statements that would (§3) fail instead.

**Removal always works.** Dropping an unparsed object, or the table that owns it, never needs its
parsed form: `DROP VIEW` (with `CASCADE`, through the persisted dependency list), `DROP INDEX`,
`ALTER TABLE … DROP CONSTRAINT`, `ALTER TABLE … ALTER COLUMN … DROP DEFAULT` or `SET DEFAULT`, and
`DROP TABLE` all succeed. Redefining the object under the older binary is therefore a drop followed by
a create in its own syntax, or `CREATE OR REPLACE VIEW` for a view (§3.1).

## 3. Per kind

The statements that need an object follow from what each kind is used for. They match the rules
already in force for host-function dependencies ([extensibility.md](extensibility.md) §7.1–§8.1) and
timezone dependencies ([index-dependencies.md](index-dependencies.md)), which contain a missing or
changed dependency to the statements that would use it.

### 3.1 View

A regular view is read-required: it has no stored rows to fall back on.

- **Fails `XX003`:** any statement that references the view, directly or through another view's body
  (reading it, writing through it, `EXPLAIN` of either), and reading `jed_columns`, which plans every
  view to type its columns ([introspection.md](introspection.md) §5 *View columns*: a view is never
  silently omitted).
- **Fails `XX003`:** a statement that must re-plan or rewrite the view because something it depends on
  changes: `ALTER TABLE` of a table it depends on (every action, including the `RENAME` forms,
  [views.md](views.md) §7/§8), `ALTER VIEW … RENAME` of a view it reads, and `CREATE OR REPLACE VIEW`
  of a view it reads directly or transitively. A view that reads an unparsed view cannot be planned
  either, so the same holds one level up: renaming an unparsed view that another view reads fails,
  because the reader must be rewritten. The `XX003` passes through re-validation, so the message names
  the unparsed view rather than the generic *would change the definition* `0A000`.
- **Proceeds:** `DROP VIEW` of it; `DROP TABLE … CASCADE` / `DROP VIEW … CASCADE` /
  `ALTER TABLE … DROP COLUMN … CASCADE` that reach it; the `2BP01` dependency refusals, which read the
  persisted dependency list; `ALTER VIEW … RENAME TO` and `RENAME COLUMN` of the unparsed view itself
  when no view reads it (they change its catalog key or its stored column names, not its definition);
  reads and writes of the tables it reads; `jed_views`.
- **Proceeds:** `CREATE OR REPLACE VIEW` of the unparsed view itself. Its old column types cannot be
  derived, so the replacement is compared as for an old body that no longer plans
  ([views.md](views.md) §1.2): the column count and names must hold (`42P16`), the types are not
  compared. This is the way to redefine the view in the older binary's own syntax without dropping
  the views that read it.

### 3.2 `CHECK` constraint

A check is evaluated only when a row is written, so reads never need it.

- **Fails `XX003`:** every statement that evaluates the table's checks: `INSERT` (including `ON
  CONFLICT … DO UPDATE`), `UPDATE`, a foreign-key action that updates a row of the table, and `ALTER
  COLUMN … TYPE`, which re-validates every check.
- **Fails `XX003`:** statements that must analyze or edit the text: `ALTER TABLE … DROP COLUMN` (which
  drops the constraints that reference the column, and so must know which do) and the `RENAME TO` /
  `RENAME COLUMN` forms of `ALTER TABLE` (which rewrite the text).
- **Proceeds:** `SELECT`, `DELETE` (including `ON DELETE CASCADE` deletes of child rows),
  `DROP CONSTRAINT` of it, `ADD CONSTRAINT` of another constraint, `DROP TABLE`.

### 3.3 Expression `DEFAULT`

A default is evaluated only when a write applies it ([extensibility.md](extensibility.md) §7.1.1
already resolves only the defaults a statement can apply).

- **Fails `XX003`:** a statement that applies that column's default: an `INSERT` that omits the column
  or writes `DEFAULT` for it (or `DEFAULT VALUES`), `UPDATE … SET col = DEFAULT`, and an `ON DELETE` /
  `ON UPDATE SET DEFAULT` action into the column.
- **Fails `XX003`:** `ALTER COLUMN … TYPE` of that column (the default is re-typed through a cast) and
  the `RENAME TO` / `RENAME COLUMN` forms of `ALTER TABLE` (which rewrite the text).
- **Proceeds:** every other statement, including an `INSERT` that supplies the column, `SET DEFAULT` /
  `DROP DEFAULT` on it, and `DROP TABLE`. `jed_columns.column_default` shows the stored text.

### 3.4 Expression index and partial index

An index whose key expression or predicate is unparsed cannot compute its entries, so it can be
neither maintained nor trusted for an access path. This is the index rule of
[index-dependencies.md](index-dependencies.md) *Persistence and use*.

- **Reads proceed without it.** The planner excludes the index from access-path inventory, exactly as
  it excludes an index whose dependencies do not match; reads use the other paths, ultimately a full
  scan of the table, and return the same rows.
- **Fails `XX003`:** every write that maintains the table's indexes: `INSERT`, `UPDATE`, `DELETE`, and
  the foreign-key actions that write the table. A `UNIQUE` index is therefore never bypassed: the
  table is read-only while the index is unparsed.
- **Fails `XX003`:** `ALTER TABLE` actions that rebuild or rewrite the index: `ALTER COLUMN … TYPE`,
  `DROP COLUMN`, and the `RENAME` forms.
- **Proceeds:** `DROP INDEX` of it, `DROP TABLE`, `ALTER TABLE … ADD COLUMN` (with or without a
  default; it leaves every index entry valid), and every read.

## 4. Introspection

An unparsed object appears in introspection exactly as a parsed one, with its stored text verbatim:
`jed_views.definition`, `jed_constraints` (a check's expression), `jed_indexes` (the key and predicate
text), and `jed_columns.column_default`. No relation gains a column. The `XX003` message names the
object, which is how a user finds it; introspection does not mark it. The one exception is
`jed_columns` while an unparsed view exists, which fails `XX003` (§3.1).

The CLI dump emits the stored text unchanged, so a dump taken by an older binary recreates the object
under a binary that can parse it.

## 5. The error

`XX003` `unparsable_stored_definition` (class `XX`, internal error), template *stored definition does
not parse in this version of jed: {detail}*. The detail names the object: `view "v"`,
`check constraint "c" of table "t"`, `default of column "c" of table "t"`, or `index "i" of table "t"`.
It carries no position, since the failing text is not part of the statement.

It is a new sibling of `XX002`, following the resolved decision in
[compatibility.md](compatibility.md) §12 #4: each kind of "this binary cannot use this object" gets its
own code rather than overloading the collation code. It is not `XX001`, because nothing is corrupt, and
not `0A000`, which reports a construct the *statement* uses. PostgreSQL has no equivalent; its
`XX003` is unassigned.

When one statement needs several unparsed objects, the first error follows the existing resolution
order: checks before defaults before indexes within a write ([extensibility.md](extensibility.md)
§7.1.4), checks in name byte order, defaults in column order, indexes in catalog order, and views in
the order planning reaches them.

## 6. Scope

The rule covers every database a handle opens: `main`, file and in-memory attachments, and catalog
reloads after a foreign commit in shared mode. Session-local temporary objects never round-trip
through another binary, so they are never unparsed. Text that parses but fails resolution (an unknown
function, a missing sequence) was already handled per use and is unchanged
([views.md](views.md) §3, [extensibility.md](extensibility.md) §7.1).

## 7. Verification

The SQL corpus cannot write text the engine fails to parse, so the shared byte fixture
`spec/fileformat/fixtures/unparsable_definitions.jed` is forged by the Ruby reference
([../fileformat/verify.rb](../fileformat/verify.rb)), which does not parse SQL. It holds one object of
each kind whose stored text is not valid jed syntax, beside ordinary tables and a parsable view. The
corpus file `suites/storage/unparsable_definitions.test` opens it (`# fixture:`) in every core and
asserts §3's outcomes: the reads that proceed, the statements that fail `XX003`, each drop that
succeeds, and, after the drops, a table that is writable again. Each core's golden test reloads the
fixture and re-serializes it, asserting the bytes are unchanged (§2, *the text is never rewritten*).
