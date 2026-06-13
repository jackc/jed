# The `jed` CLI — design

The `jed` binary: a full-screen terminal client (TUI) for interactive use, plus a plain
stdout **script mode** for automation — the role `psql` / `sqlite3` play for their engines,
taken with a deliberately modern interface (an editor pane, a results grid, and a schema
sidebar instead of a 30-year-old meta-command REPL).

## 1. Scope & non-goals

The CLI is a **host program**, not a core (CLAUDE.md §2): it links the **Rust** core
(`impl/rust`) through the public embedding API ([api.md](api.md)) and adds no engine
behavior. It conforms to nothing and votes on nothing — the conformance corpus binds the
*engine* it embeds; the CLI's own output (table layout, footers, prompts) is a product
surface, versioned with the CLI, never with the spec. There is **one** CLI; it is not
reimplemented per language.

Non-goals: no server / wire protocol (jed is embedded, §1); no query cancellation (the
engine has none — the deterministic **cost ceiling** is the runaway-query defense, §13);
no SQL dialect of its own (statements pass through the statement splitter **verbatim** —
the engine's grammar, including `--` / `/* */` comments since grammar.md §33, is the only
dialect).

## 2. Crate layout & dependencies (the §14 record)

The CLI lives at **`/cli`** — a standalone crate beside `/impl` and `/spec`, deliberately
**not** in a workspace with `impl/rust`, so the engine crate stays hermetic and
zero-dependency. The engine is consumed as a path dependency (`jed = { path =
"../impl/rust" }`).

**Dependencies** (CLAUDE.md §14 — proposed in planning, **explicitly approved by the
maintainer 2026-06-12**, recorded here):

| Crate | Why |
|---|---|
| `ratatui` | the TUI framework (layout, widgets, terminal buffer diffing) |
| `crossterm` | terminal backend (raw mode, events, colors) — ratatui's default backend |
| `tui-textarea` | the multi-line SQL editor widget (cursor movement, selection, undo) |

Containment rules: these dependencies exist **only** in `/cli/Cargo.toml` — never in any
engine core (`git diff impl/rust/Cargo.toml impl/go/go.mod impl/ts/package.json` stays
empty); they sit at a well-defined edge (terminal I/O in a host program), never inside the
parser/planner/executor; and they cannot leak nondeterminism into engine behavior (the
engine never sees them). Flag parsing is hand-rolled over `std::env::args` (~10 flags do
not justify `clap` — §10 boring-and-explicit). `Cargo.lock` is committed (the crate is a
binary).

## 3. Invocation

```
jed [OPTIONS] [DBFILE]

  (no DBFILE)             transient in-memory database
  --create                create DBFILE instead of opening it (58P02 if it exists)
  --readonly              open DBFILE read-only (api.md §2.1): writes fail with 25006,
                          the file is never touched
  --page-size N           with --create only: the page size locked into the file
  -c SQL                  execute the statements, then exit (repeatable) — script mode
  -f FILE                 execute a SQL file, then exit (repeatable; '-' = stdin) — script mode
  --import-csv TABLE=FILE import an RFC 4180 CSV into TABLE (repeatable) — script mode
  --dump                  write the database as SQL (schema + rows + indexes), then exit
  --format FORMAT         script-mode output format:
                          aligned (default) | box | markdown | csv | json
  -o FILE                 script mode: write results to FILE instead of stdout
                          ('-' = stdout); errors still go to stderr
  --max-cost N            cost ceiling: statements abort with 54P01 at cost N (api.md §8)
  --continue-on-error     script mode: keep going after a SQL error (default: stop)
  -q, --quiet             script mode: suppress OK lines (results and errors still print)
  --version, -h, --help
```

**Mode select.** `-c`/`-f` present, or stdin not a TTY → **script mode** (plain stdout,
no raw terminal). Otherwise → **TUI mode**. `-c` and `-f` compose in command-line order.

**Open vs create is strict.** `jed app.jed` opens (a missing file is `58P01` + a hint to
pass `--create`); `jed --create app.jed` creates (an existing file is `58P02`). No silent
open-or-create: a typo'd path must not durably create an empty database (the engine writes
the initial image at `create` — api.md §2), and implicit creation is the same footgun
sqlite3 is known for. No DBFILE means a **transient in-memory** database (the sqlite3 /
DuckDB convention; ideal for trying SQL), stated by the TUI status bar / script-mode banner.
`--readonly` opens the file via the engine's read-only mode (api.md §2.1 — every transaction
defaults to READ ONLY, writes are `25006`, the file is opened without write access); it
requires a DBFILE and excludes `--create`. The TUI status bar appends `(read-only)` to the
path.

**Exit codes.** `0` success · `1` startup/usage error (bad flags; `58P01`/`58P02`/
`XX001`/`58030` on open/create) · `2` a SQL statement failed in script mode.

**`--import-csv TABLE=FILE` imports a CSV** — the read half of the CSV story (`--format
csv` + `-o` is the export half). It parses the RFC 4180 dialect the exporter writes
(header row required; quoted fields, `""` escaping, CRLF or LF; quoted fields may span
lines), maps the header's names case-insensitively to TABLE's columns (columns the CSV
omits take their DEFAULT/NULL), and synthesizes **one multi-row `INSERT`** — so an import
is **atomic** (the engine's per-statement all-or-nothing: one bad row imports nothing) and
reports through the ordinary `OK, N rows (cost C)` footer. Values are built as typed SQL
literals from each column's declared type — numerics and booleans are validated with their
row/column position (booleans also accept PG `COPY`'s `t`/`f`), everything else passes
through the engine's own coercion (`22P02` etc.). A bare empty field imports as **NULL**,
a quoted `""` as the empty string (the PG `COPY ... CSV` convention — the inverse of the
§5 export policy). Imports are sources like `-c`/`-f` and run in command-line order, so
`-c 'CREATE TABLE ...' --import-csv t=data.csv` works in one invocation.

**`--dump` writes the database as SQL** (the sqlite3 `.dump` role): per table — in the
catalog's standing order — a `CREATE TABLE` (columns with typmods, NOT NULL, DEFAULT;
table-level `PRIMARY KEY` in key order; named `CHECK`s from their persisted text), one
`INSERT` per row in primary-key order (a no-PK table dumps in storage order, which replays
into the same rowids), then `CREATE [UNIQUE] INDEX` statements (after the rows, so a
replay builds each index once — a UNIQUE *constraint* dumps as its backing unique index,
which recreates the same state). The script is wrapped in one `BEGIN`/`COMMIT` so a
file-backed replay commits durably once. Round-trip contract: dump → replay into a fresh
database → dump is **byte-identical**. `--dump` requires a DBFILE, excludes `-c`/`-f`/
`--import-csv`, and composes with `--readonly` (the natural pairing) and `-o`.

**`-o FILE` redirects script-mode results** (OK lines, tables — everything that would hit
stdout) to FILE; errors stay on stderr, so a failing script still reports on the terminal.
`-o -` keeps stdout, letting scripts parameterize the destination uniformly. `-o` is
script-mode only (a usage error in TUI mode). There is deliberately **no built-in pager**:
the TUI grid is the interactive pager, and script output composes with `less` by pipe.

**Stop-on-error is the script-mode default** (psql's `ON_ERROR_STOP`-off default is a
classic half-applied-migration footgun). It is safe by construction: under autocommit a
failed statement already rolled back atomically, and `close()` rolls back any open
explicit transaction — a failed script never half-commits a block. `--continue-on-error`
restores the classic behavior and exits `2` if any statement failed.

## 4. The statement splitter

The engine parses **exactly one statement per call, with no terminator** (grammar.md §1).
The CLI owns statement splitting — a character state machine shared by script mode and the
TUI editor's run action:

- States: `normal` / `in-string` / `in-line-comment` / `in-block-comment(depth)`,
  mirroring the engine lexer's rules exactly (grammar.md §33; `'...'` with `''` escaping is
  the **only** quoting — no double-quoted identifiers, no dollar-quoting).
- `;` **outside** strings and comments terminates a statement. The semicolon is
  **stripped**; everything else — including comments, which the engine accepts — passes
  through **verbatim**.
- Whitespace-/comment-only statements are skipped (so `;;`, trailing `;`, and comment-only
  lines are not errors — the engine's "no statement" `42601` is never provoked by framing).
- At end of input (EOF / the run key), a non-empty remainder runs as a final statement —
  `echo 'SELECT 1' | jed` needs no semicolon.
- An unterminated string or block comment at end of input is a CLI-reported error (script
  mode: exit `2`).
- The splitter tracks each statement's starting line for `file:line:` error prefixes.

## 5. Output formats (script mode)

Every cell renders through the engine's canonical `Value::render()` — byte-identical to
the conformance corpus' rendering, in every format.

- **`aligned`** (default): psql-flavored ASCII — ` | ` separators, `-+-` header rule;
  int/decimal columns right-aligned, everything else left-aligned; **NULL renders as
  `NULL`** (the engine's canonical rendering — a deliberate divergence from psql's blank
  cell, which is ambiguous against the empty string). Footers: query → `(N rows, cost C)`;
  DML without RETURNING → `OK, N rows (cost C)` (the engine's affected-row count, api.md
  §4); other non-query statements → `OK (cost C)`; `BEGIN`/`COMMIT`/`ROLLBACK` → the bare
  tag. **Cost is shown by default**: it is deterministic, reproducible, and a headline
  feature (§13) — wall-clock time is not printed at all in script mode (nondeterministic
  output breaks golden tests and diffs).
- **`box`**: the `aligned` layout framed in Unicode box-drawing rules (`┌─┬─┐` / `├─┼─┤` /
  `└─┴─┘`), same alignment policy, NULL rendering, and footers — a second human format.
- **`markdown`**: a GitHub-flavored table, padded like `aligned`, with `---:` alignment
  hints on numeric columns. Pipes are escaped (`\|`) and embedded newlines become `<br>`
  so a cell cannot break the table. Pure data — no footers (it is meant to be pasted into
  a document).
- **`csv`**: RFC 4180 — header row, `,` separator, `"` quoting/escaping; **NULL → empty
  field** (the PG `COPY ... CSV` convention; the NULL-vs-empty-text ambiguity is accepted,
  v1). No footers, ever.
- **`json`**: one JSON array of row objects, keys in column order. Scalar mapping:
  int → JSON number (exact — JSON's grammar has arbitrary-precision integers), boolean →
  `true`/`false`, NULL → `null`, **decimal → string** (a JSON number would round-trip
  through f64 in most readers and betray the exact-decimal contract), text/bytea/uuid/
  timestamp/timestamptz → their `render()` strings. No footers.

**Errors** print to **stderr** as one line — `ERROR 23505: duplicate key value violates
unique constraint: t_pkey` — the SQLSTATE inline (errors are structured data, §5/§10),
prefixed `FILE:LINE: ` in script mode. CLI-generated hints (a second `hint: ...` line):
missing file → `pass --create`; `54P01` → `raise the ceiling with --max-cost`.

## 6. TUI mode

Layout: **schema sidebar** (left, toggleable) · **query editor** (top right,
tui-textarea) · **results grid** (bottom right) · **status bar** (bottom) · a one-line
**message log** between editor and grid for per-statement tags and errors.

- **Editor**: multi-line SQL; `Ctrl+Enter` (or `F5` — not every terminal can report
  Ctrl+Enter) runs the buffer through the splitter; statements execute sequentially,
  stopping at the first error. The message line carries the last statement's tag
  (`OK (cost C)` / `N rows (cost C)`) or the error that stopped the batch; the grid
  shows the **last** query result. **`Tab` autocompletes** the word at the cursor from
  the live catalog (table names, column names — completed in canonical spelling) plus
  the grammar's keywords/type/function names (case-styled after the typed prefix): one
  match completes inline, several open a popup at the cursor (`↑`/`↓` select,
  `Enter`/`Tab` accept, `Esc` closes, any other key closes and types). At a non-word
  position `Tab` stays an ordinary tab. The buffer is **syntax-highlighted** by a small
  line-spanning tokenizer mirroring the engine's lexical rules (grammar.md §33 — strings
  and nested block comments carry across lines): keywords cyan, strings green, numbers
  magenta, comments dim. The editor view is rendered from the highlighter's spans
  (tui-textarea offers no per-token styling; it still owns all editing state), with the
  real terminal cursor when focused.
- **Results grid**: scrollable on both axes when focused (arrows / PgUp / PgDn / Home /
  End); header row pinned; cells via `Value::render()`, NULL dimmed. Footer:
  `N rows · cost C` (wall time may appear here, clearly cosmetic — it never appears in
  script output).
- **Schema sidebar**: built from `db.table_names()` + `db.table(name)` (api.md §6) —
  table → columns (`name type`, PK / NOT NULL markers), indexes (UNIQUE flag), CHECK
  names. Refreshed after any successful statement batch. Enter on a table name inserts it
  into the editor.
- **Status bar**: db file path or `memory` · transaction state from `db.in_transaction()`
  — `autocommit` / `TX` / `TX FAILED` (failed is CLI-tracked: set when a statement errors
  while a transaction is open, cleared when the transaction ends; the engine's `25P02`
  then explains itself) · `max_cost` when set.
- **History**: session statements persisted to `~/.jed_history` (override:
  `JED_HISTFILE`); `Ctrl+R` opens a pick-list, Enter loads the entry into the editor.
- **Keys**: `Tab`/`Shift+Tab` cycle pane focus · `Esc` leaves the editor · `F1` (or `?`
  outside the editor) help overlay · `Ctrl+Q` (or `q` outside the editor) quits. Quitting
  with an open transaction rolls it back (`close()` semantics, api.md §2.3) — the status
  bar made the state visible.

## 7. Determinism & testing

Script mode is **golden-testable**: the engine is deterministic, cost footers are exact,
wall-clock never prints, and there is no banner on piped stdin. End-to-end tests pipe
`testdata/*.sql` through the built binary and byte-compare stdout (queries in goldens use
`ORDER BY` — unordered row order is spec-unspecified, §8). The splitter and formatters are
unit-tested; the TUI layer is kept logic-free (session state, splitting, and rendering
live in shared modules exercised by the script-mode tests).

## 8. Future (not v1)

The original post-v1 list has fully landed: affected-row counts in `Outcome` (the
`OK, N rows` footer of §5) · editor autocomplete from the catalog (§6) · SQL syntax
highlighting (§6) · CSV import/export (`--import-csv` in, `--format csv` + `-o` out — §3)
· the `--dump` SQL export (§3) · `--readonly` (§3) · the `box`/`markdown` formats (§5) ·
`-o` output redirection (§3 — a built-in pager is deliberately out: the TUI grid pages
interactively, scripts pipe). Nothing further is currently scheduled.
